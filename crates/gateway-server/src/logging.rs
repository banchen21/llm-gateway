//! Records every served request, and reads those records back for the GUI.
//!
//! The file is append-only JSONL. It is written for diagnosis and cost accounting, so it
//! stores token counts and prompt shape but never message bodies.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use gateway_core::ir::{ChatRequest, ContentPart, EventStream, ModelEvent, Usage};

/// How much of the log file is read back for the GUI, newest bytes last.
const TAIL_BYTES: u64 = 1024 * 1024;

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
    path: Option<PathBuf>,
    /// Guards appends so concurrent requests cannot interleave within a line.
    append: Arc<tokio::sync::Mutex<()>>,
}

impl RequestLog {
    /// Passing `None` disables recording.
    pub fn new(path: Option<PathBuf>) -> Self {
        RequestLog {
            path,
            append: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub async fn record(&self, record: Record) {
        let Some(path) = &self.path else {
            return;
        };
        let line = match serde_json::to_string(&entry(&record)) {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!(%error, "could not render a request log entry");
                return;
            }
        };
        let _guard = self.append.lock().await;
        if let Err(error) = append(path, &line).await {
            tracing::warn!(path = %path.display(), %error, "could not append to the request log");
        }
    }

    /// The most recent entries, newest first — the order the GUI lists them in.
    pub async fn recent(&self, limit: usize) -> Vec<Value> {
        let Some(path) = &self.path else {
            return Vec::new();
        };
        let text = match read_tail(path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not read the request log");
                return Vec::new();
            }
        };
        let mut entries: Vec<Value> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if entries.len() > limit {
            entries.drain(..entries.len() - limit);
        }
        // The file is append-only, so the newest record sits at the end; the GUI reads
        // top-down, which is why the ordering is flipped here rather than in the page.
        entries.reverse();
        entries
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

fn entry(record: &Record) -> Value {
    let mut value = json!({
        "at": unix_millis(),
        "model": record.alias,
        "provider": record.provider,
        "upstream_model": record.upstream_model,
        "stream": record.stream,
        "duration_ms": record.duration_ms,
        "prompt": {
            "system_chars": record.prompt.system_chars,
            "messages": record.prompt.messages,
            "tool_results": record.prompt.tool_results,
            "tools": record.prompt.tools,
        },
    });
    match &record.outcome {
        Outcome::Served { usage } => {
            value["status"] = json!(200);
            if let Some(usage) = usage {
                value["usage"] = json!({
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "cache_read_tokens": usage.cache_read_tokens,
                    "cache_write_tokens": usage.cache_write_tokens,
                });
            }
        }
        Outcome::Failed { status, message } => {
            value["status"] = json!(status);
            value["error"] = json!(message);
        }
    }
    value
}

async fn append(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(line.as_bytes()).await?;
    file.write_all(b"\n").await?;
    file.flush().await
}

/// Reads the end of the log, so a file that has grown large is still cheap to open.
async fn read_tail(path: &Path) -> std::io::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let mut file = tokio::fs::File::open(path).await?;
    let length = file.metadata().await?.len();
    let mut text = String::new();
    if length <= TAIL_BYTES {
        file.read_to_string(&mut text).await?;
        return Ok(text);
    }
    file.seek(std::io::SeekFrom::Start(length - TAIL_BYTES))
        .await?;
    file.read_to_string(&mut text).await?;
    // The seek lands mid-line, so the first partial entry is dropped.
    match text.find('\n') {
        Some(end) => Ok(text[end + 1..].to_owned()),
        None => Ok(String::new()),
    }
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default()
}
