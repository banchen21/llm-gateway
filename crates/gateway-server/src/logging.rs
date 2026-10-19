//! Records every served request, and reads those records back for the GUI.
//!
//! Records live in a local SQLite database next to the configuration. It is written for
//! diagnosis and cost accounting, so it stores token counts and prompt shape but never
//! message bodies.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use gateway_core::ir::{ChatRequest, ContentPart, EventStream, ModelEvent, Usage};

/// The prompt's shape, recorded instead of its contents.
#[derive(Clone, Copy)]
pub struct PromptShape {
    pub system_chars: usize,
    pub messages: usize,
    pub tool_results: usize,
    pub tools: usize,
}

impl PromptShape {
    pub fn of(request: &ChatRequest) -> Self {
        PromptShape {
            system_chars: request.system.as_deref().map_or(0, |s| s.chars().count()),
            messages: request.messages.len(),
            tool_results: request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter(|part| matches!(part, ContentPart::ToolResult { .. }))
                .count(),
            tools: request.tools.len(),
        }
    }
}

pub enum Outcome {
    Served { usage: Option<Usage> },
    Failed { status: u16, message: String },
}

/// One request, owned, so it can outlive the borrow that produced it — a streaming
/// response is only recorded once its last frame has been sent.
pub struct Record {
    pub alias: String,
    pub provider: String,
    pub upstream_model: String,
    pub stream: bool,
    pub prompt: PromptShape,
    /// Wall time from accepting the client request to producing this record, replays
    /// included. A successful call that still took half a minute is a slow upstream, not
    /// a failed one, and only this number tells the two apart after the fact.
    pub duration_ms: u64,
    pub outcome: Outcome,
}

#[derive(Clone)]
pub struct RequestLog {
    db: Option<Arc<Mutex<Connection>>>,
}

impl RequestLog {
    /// Passing `None` disables recording.
    ///
    /// A database that cannot be opened disables recording rather than failing the
    /// gateway: losing the log is annoying, refusing to serve is not acceptable.
    pub fn new(path: Option<PathBuf>) -> Self {
        let Some(path) = path else {
            return RequestLog { db: None };
        };
        match open(&path) {
            Ok(connection) => {
                import_legacy_jsonl(&connection, &path);
                RequestLog {
                    db: Some(Arc::new(Mutex::new(connection))),
                }
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not open the request log");
                RequestLog { db: None }
            }
        }
    }

    pub async fn record(&self, record: Record) {
        let Some(db) = self.db.clone() else {
            return;
        };
        let row = Row::of(&record);
        let _ = tokio::task::spawn_blocking(move || {
            let connection = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Err(error) = insert(&connection, &row) {
                tracing::warn!(%error, "could not record a request");
            }
        })
        .await;
    }

    /// The most recent entries, newest first — the order the GUI lists them in.
    pub async fn recent(&self, limit: usize) -> Vec<Value> {
        let Some(db) = self.db.clone() else {
            return Vec::new();
        };
        tokio::task::spawn_blocking(move || {
            let connection = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match select_recent(&connection, limit) {
                Ok(entries) => entries,
                Err(error) => {
                    tracing::warn!(%error, "could not read the request log");
                    Vec::new()
                }
            }
        })
        .await
        .unwrap_or_default()
    }

    /// Empties the log and reports how many records were removed.
    pub async fn clear(&self) -> usize {
        let Some(db) = self.db.clone() else {
            return 0;
        };
        tokio::task::spawn_blocking(move || {
            let connection = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match connection.execute("DELETE FROM requests", []) {
                Ok(count) => count,
                Err(error) => {
                    tracing::warn!(%error, "could not clear the request log");
                    0
                }
            }
        })
        .await
        .unwrap_or(0)
    }
}

/// Captures the usage an upstream reports as it flows past, since the outbound serializer
/// consumes the stream and would otherwise swallow it.
pub fn capture_usage(mut events: EventStream, sink: Arc<Mutex<Option<Usage>>>) -> EventStream {
    Box::pin(async_stream::stream! {
        while let Some(result) = events.next().await {
            if let Ok(ModelEvent::Usage(usage)) = &result {
                *sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(*usage);
            }
            yield result;
        }
    })
}

pub fn captured_usage(sink: &Arc<Mutex<Option<Usage>>>) -> Option<Usage> {
    *sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                Some(error.to_string()),
            )
        })?;
    }
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS requests (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            at INTEGER NOT NULL,
            model TEXT NOT NULL,
            provider TEXT NOT NULL,
            upstream_model TEXT NOT NULL,
            stream INTEGER NOT NULL,
            duration_ms INTEGER NOT NULL,
            status INTEGER NOT NULL,
            input_tokens INTEGER,
            output_tokens INTEGER,
            cache_read_tokens INTEGER,
            cache_write_tokens INTEGER,
            system_chars INTEGER NOT NULL,
            messages INTEGER NOT NULL,
            tool_results INTEGER NOT NULL,
            tools INTEGER NOT NULL,
            error TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_requests_at ON requests(at DESC, id DESC);",
    )?;
    Ok(connection)
}

/// Brings a JSONL log written before the switch into the database, once.
///
/// The old file is left in place: it is the only copy of whatever it holds, and nothing
/// depends on it after the import.
fn import_legacy_jsonl(connection: &Connection, db_path: &Path) {
    let Some(legacy) = db_path.parent().map(|dir| dir.join("requests.jsonl")) else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&legacy) else {
        return;
    };
    let existing: i64 = connection
        .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
        .unwrap_or(0);
    if existing > 0 {
        return;
    }
    let mut imported = 0;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if insert(connection, &Row::of_json(&value)).is_ok() {
            imported += 1;
        }
    }
    if imported > 0 {
        tracing::info!(records = imported, path = %legacy.display(), "imported the JSONL request log");
    }
}

/// One row's worth of values, so the insert has a single shape to bind.
struct Row {
    at: i64,
    model: String,
    provider: String,
    upstream_model: String,
    stream: bool,
    duration_ms: i64,
    status: u16,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
    system_chars: i64,
    messages: i64,
    tool_results: i64,
    tools: i64,
    error: Option<String>,
}

impl Row {
    fn of(record: &Record) -> Self {
        let (status, usage, error) = match &record.outcome {
            Outcome::Served { usage } => (200, *usage, None),
            Outcome::Failed { status, message } => (*status, None, Some(message.clone())),
        };
        Row {
            at: unix_millis(),
            model: record.alias.clone(),
            provider: record.provider.clone(),
            upstream_model: record.upstream_model.clone(),
            stream: record.stream,
            duration_ms: record.duration_ms as i64,
            status,
            input_tokens: usage.map(|usage| usage.input_tokens as i64),
            output_tokens: usage.map(|usage| usage.output_tokens as i64),
            cache_read_tokens: usage.map(|usage| usage.cache_read_tokens as i64),
            cache_write_tokens: usage.map(|usage| usage.cache_write_tokens as i64),
            system_chars: record.prompt.system_chars as i64,
            messages: record.prompt.messages as i64,
            tool_results: record.prompt.tool_results as i64,
            tools: record.prompt.tools as i64,
            error,
        }
    }

    fn of_json(value: &Value) -> Self {
        let number = |path: &[&str]| -> Option<i64> {
            let mut cursor = value;
            for key in path {
                cursor = cursor.get(key)?;
            }
            cursor
                .as_i64()
                .or_else(|| cursor.as_u64().map(|n| n as i64))
        };
        Row {
            at: number(&["at"]).unwrap_or_default(),
            model: value["model"].as_str().unwrap_or_default().to_owned(),
            provider: value["provider"].as_str().unwrap_or_default().to_owned(),
            upstream_model: value["upstream_model"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            stream: value["stream"].as_bool().unwrap_or(false),
            duration_ms: number(&["duration_ms"]).unwrap_or_default(),
            status: number(&["status"]).unwrap_or_default() as u16,
            input_tokens: number(&["usage", "input_tokens"]),
            output_tokens: number(&["usage", "output_tokens"]),
            cache_read_tokens: number(&["usage", "cache_read_tokens"]),
            cache_write_tokens: number(&["usage", "cache_write_tokens"]),
            system_chars: number(&["prompt", "system_chars"]).unwrap_or_default(),
            messages: number(&["prompt", "messages"]).unwrap_or_default(),
            tool_results: number(&["prompt", "tool_results"]).unwrap_or_default(),
            tools: number(&["prompt", "tools"]).unwrap_or_default(),
            error: value["error"].as_str().map(str::to_owned),
        }
    }
}

fn insert(connection: &Connection, row: &Row) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO requests (
            at, model, provider, upstream_model, stream, duration_ms, status,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
            system_chars, messages, tool_results, tools, error
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            row.at,
            row.model,
            row.provider,
            row.upstream_model,
            row.stream,
            row.duration_ms,
            row.status,
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
            row.cache_write_tokens,
            row.system_chars,
            row.messages,
            row.tool_results,
            row.tools,
            row.error,
        ],
    )?;
    Ok(())
}

fn select_recent(connection: &Connection, limit: usize) -> rusqlite::Result<Vec<Value>> {
    let mut statement = connection.prepare(
        "SELECT at, model, provider, upstream_model, stream, duration_ms, status,
                input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                system_chars, messages, tool_results, tools, error
         FROM requests ORDER BY at DESC, id DESC LIMIT ?1",
    )?;
    let rows = statement.query_map([limit as i64], |row| {
        let stream: bool = row.get(4)?;
        let error: Option<String> = row.get(15)?;
        let mut value = json!({
            "at": row.get::<_, i64>(0)?,
            "model": row.get::<_, String>(1)?,
            "provider": row.get::<_, String>(2)?,
            "upstream_model": row.get::<_, String>(3)?,
            "stream": stream,
            "duration_ms": row.get::<_, i64>(5)?,
            "status": row.get::<_, i64>(6)?,
            "prompt": {
                "system_chars": row.get::<_, i64>(11)?,
                "messages": row.get::<_, i64>(12)?,
                "tool_results": row.get::<_, i64>(13)?,
                "tools": row.get::<_, i64>(14)?,
            },
        });
        let usage = match row.get::<_, Option<i64>>(7)? {
            Some(input) => Some(json!({
                "input_tokens": input,
                "output_tokens": row.get::<_, Option<i64>>(8)?.unwrap_or(0),
                "cache_read_tokens": row.get::<_, Option<i64>>(9)?.unwrap_or(0),
                "cache_write_tokens": row.get::<_, Option<i64>>(10)?.unwrap_or(0),
            })),
            None => None,
        };
        if let Some(usage) = usage {
            value["usage"] = usage;
        }
        if let Some(error) = error {
            value["error"] = json!(error);
        }
        Ok(value)
    })?;
    rows.collect()
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}
