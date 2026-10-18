//! Holds the gateway's shared state.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
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
}

impl AppState {
    pub fn new(config: Config, config_path: PathBuf) -> Result<Arc<Self>> {
        let http = reqwest::Client::builder()
            // HTTP/2 buys nothing here — one request occupies one connection for the
            // length of a stream — and some upstream edges reset HTTP/2 streams with an
            // opaque protocol error while serving the same request over HTTP/1.1.
            .http1_only()
            // Upstream edges close idle keep-alive connections without telling the pool,
            // and a request sent on one of those dies with a connection reset. Retiring
            // our own idle connections well before theirs avoids handing one out.
            .pool_idle_timeout(Duration::from_secs(20))
            // CommandCode connects in 2-6 seconds when it is healthy, so a connect still
            // hanging at ten is a dead path: fail fast and let the replay try a new one
            // instead of stalling the client for thirty seconds first.
            .connect_timeout(Duration::from_secs(10))
            // No overall timeout: a streaming response is expected to stay open for
            // minutes, and an idle upstream is caught by the read timeout instead.
            .read_timeout(Duration::from_secs(300))
            .build()?;
        // The request log lives beside the configuration so both travel together.
        let log = RequestLog::new(
            config_path
                .parent()
                .map(|directory| directory.join("logs").join("requests.jsonl")),
        );
        Ok(Arc::new(AppState {
            http,
            config_path,
            log,
            snapshot: RwLock::new(Arc::new(Snapshot::new(config))),
        }))
    }

    pub fn load(config_path: PathBuf) -> Result<Arc<Self>> {
        let config = config::load(&config_path)?;
        AppState::new(config, config_path)
    }

    /// Cheap to call per request: cloning the `Arc` does not copy the configuration.
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
