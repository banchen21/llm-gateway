//! Holds the gateway's shared state.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use gateway_core::config::{self, Config};
use gateway_core::router::Router;
use gateway_core::Result;

use crate::logging::RequestLog;

/// A configuration together with everything derived from it.
///
/// The config and its router are replaced as a unit so a request can never see one
/// version's aliases paired with another version's providers.
pub struct Snapshot {
    pub config: Config,
    pub router: Router,
}

impl Snapshot {
    pub fn new(config: Config) -> Self {
        let router = Router::build(&config);
        Snapshot { config, router }
    }
}

pub struct AppState {
    pub http: reqwest::Client,
    pub config_path: PathBuf,
    pub log: RequestLog,
    snapshot: RwLock<Arc<Snapshot>>,
    /// Advances once per request so consecutive calls start at different provider keys
    /// instead of all knocking on the same account first.
    key_cursor: AtomicUsize,
    /// Built on the first connection failure, if the system has a proxy configured.
    proxy_client: OnceLock<reqwest::Client>,
}

impl AppState {
    pub fn new(config: Config, config_path: PathBuf) -> Result<Arc<Self>> {
        let http = build_client(None)?;
        // The request log lives beside the configuration so both travel together.
        let log = RequestLog::new(
            config_path
                .parent()
                .map(|directory| directory.join("logs").join("requests.db")),
        );
        Ok(Arc::new(AppState {
            http,
            config_path,
            log,
            snapshot: RwLock::new(Arc::new(Snapshot::new(config))),
            key_cursor: AtomicUsize::new(0),
            proxy_client: OnceLock::new(),
        }))
    }

    /// A client that reaches the internet through the system proxy, built once.
    ///
    /// Used only after a direct connection fails: the proxy is a rescue path, not the
    /// default, because it would otherwise route domestic upstreams through whatever
    /// tunnel happens to be running.
    pub fn client_via_system_proxy(&self) -> Option<reqwest::Client> {
        if let Some(client) = self.proxy_client.get() {
            return Some(client.clone());
        }
        let url = crate::proxy::system_proxy()?;
        let client = build_client(Some(&url)).ok()?;
        tracing::warn!(proxy = %url, "retrying through the system proxy");
        let _ = self.proxy_client.set(client.clone());
        Some(client)
    }

    pub fn load(config_path: PathBuf) -> Result<Arc<Self>> {
        let config = config::load(&config_path)?;
        AppState::new(config, config_path)
    }

    /// Cheap to call per request: cloning the `Arc` does not copy the configuration.
    /// The key index the next upstream call should start from.
    pub fn next_key_start(&self) -> usize {
        self.key_cursor.fetch_add(1, Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        // A panic while holding the lock must not make the configuration permanently
        // unreadable, so a poisoned lock still yields its last value.
        self.snapshot
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Validates, persists, then publishes a new configuration.
    ///
    /// Persisting first means a rejected configuration is never published, and a failed
    /// write leaves the running gateway on the configuration it already had.
    pub fn replace(&self, config: Config) -> Result<()> {
        config::validate(&config)?;
        let snapshot = Arc::new(Snapshot::new(config));
        config::save(&self.config_path, &snapshot.config)?;
        *self
            .snapshot
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = snapshot;
        Ok(())
    }
}

/// The one place an upstream HTTP client is configured.
fn build_client(proxy: Option<&str>) -> reqwest::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        // HTTP/2 buys nothing here — one request occupies one connection for the length
        // of a stream — and some upstream edges reset HTTP/2 streams with an opaque
        // protocol error while serving the same request over HTTP/1.1.
        .http1_only()
        // Upstream edges close idle keep-alive connections without telling the pool, and
        // a request sent on one of those dies with a connection reset. Retiring our own
        // idle connections well before theirs avoids handing one out.
        .pool_idle_timeout(Duration::from_secs(20))
        // CommandCode connects in 2-6 seconds when it is healthy, so a connect still
        // hanging at ten is a dead path: fail fast and let the replay try a new one
        // instead of stalling the client for thirty seconds first.
        .connect_timeout(Duration::from_secs(10))
        // No overall timeout: a streaming response is expected to stay open for minutes,
        // and an idle upstream is caught by the read timeout instead.
        .read_timeout(Duration::from_secs(300));
    if let Some(url) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(url)?);
    }
    builder.build()
}
