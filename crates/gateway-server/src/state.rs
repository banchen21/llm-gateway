//! Holds the gateway's shared state.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
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
    /// Routes that recently took too long to answer, and when they may be tried again.
    slow_routes: Mutex<HashMap<String, std::time::Instant>>,
    /// How well the direct and proxy paths have been behaving lately.
    paths: Mutex<PathStats>,
}

/// Which route to the internet a request should take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    Direct,
    Proxy,
}

/// A decaying average of how long each path takes to produce its first byte.
///
/// Recency is the point: the proxy being fast this hour says nothing about the next, and
/// the whole reason this exists is that the answer changes with the network.
#[derive(Default, Clone, Copy)]
struct PathScore {
    ewma_ms: f64,
    samples: u32,
    failures: u32,
}

impl PathScore {
    /// A failure is scored as worse than any normal wait, so it takes a few good samples
    /// to recover.
    const FAILURE_MS: f64 = 30_000.0;
    const WEIGHT: f64 = 0.3;

    fn observe(&mut self, latency: std::time::Duration, ok: bool) {
        let sample = if ok {
            latency.as_millis() as f64
        } else {
            Self::FAILURE_MS
        };
        self.ewma_ms = if self.samples == 0 {
            sample
        } else {
            self.ewma_ms * (1.0 - Self::WEIGHT) + sample * Self::WEIGHT
        };
        if ok {
            self.failures = self.failures.saturating_sub(1);
        } else {
            self.failures = self.failures.saturating_add(1);
        }
        self.samples = self.samples.saturating_add(1);
    }
}

#[derive(Default)]
struct PathStats {
    direct: PathScore,
    proxy: PathScore,
    requests: u64,
}

/// Enough evidence before believing the proxy is better, and the margin that makes a
/// difference worth acting on (a faster path must be 15% faster).
const MIN_PROXY_SAMPLES: u32 = 2;
const PREFER_MARGIN: f64 = 0.85;
/// While the proxy is preferred, direct still gets the occasional request so a network
/// that changed underneath the numbers can win the preference back.
const DIRECT_PROBE_EVERY: u64 = 10;

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
            slow_routes: Mutex::new(HashMap::new()),
            paths: Mutex::new(PathStats::default()),
        }))
    }

    /// Loads the configuration from disk, creating a default one if it is absent.
    pub fn load(config_path: PathBuf) -> Result<Arc<Self>> {
        let config = config::load(&config_path)?;
        AppState::new(config, config_path)
    }

    /// Which path has been behaving better lately.
    ///
    /// Direct wins ties and first contact: it is the right default, and the proxy has to
    /// earn the preference with measurements rather than inherit it.
    pub fn choose_path(&self) -> Path {
        let available = crate::proxy::system_proxy().is_some() && !crate::proxy::from_env();
        let mut stats = self
            .paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stats.requests = stats.requests.wrapping_add(1);
        decide(&stats, available)
    }

    /// Feeds one opening attempt back into the preference.
    pub fn record_path(&self, path: Path, latency: std::time::Duration, ok: bool) {
        let mut stats = self
            .paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match path {
            Path::Direct => stats.direct.observe(latency, ok),
            Path::Proxy => stats.proxy.observe(latency, ok),
        }
    }

    /// The path the next request would start on, without counting as a request.
    pub fn preferred_path(&self) -> Path {
        let available = crate::proxy::system_proxy().is_some() && !crate::proxy::from_env();
        let stats = self
            .paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        decide(&stats, available)
    }

    /// Whether this route answered too slowly recently enough to avoid it.
    pub fn is_slow(&self, route: &str) -> bool {
        let mut slow = self
            .slow_routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match slow.get(route) {
            Some(until) if *until > std::time::Instant::now() => true,
            Some(_) => {
                slow.remove(route);
                false
            }
            None => false,
        }
    }

    /// Keeps this route out of the way for a while after it stalled.
    pub fn mark_slow(&self, route: &str) {
        self.slow_routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                route.to_owned(),
                std::time::Instant::now() + SLOW_ROUTE_COOLDOWN,
            );
    }

    /// A client that reaches the internet through the system proxy, built once.
    ///
    /// Used as an alternative to the direct path, never as a replacement for it: routing
    /// everything through whatever tunnel happens to be running would quietly change how
    /// domestic upstreams are reached.
    pub fn client_via_system_proxy(&self) -> Option<reqwest::Client> {
        if let Some(client) = self.proxy_client.get() {
            return Some(client.clone());
        }
        let url = crate::proxy::system_proxy()?;
        let client = build_client(Some(&url)).ok()?;
        tracing::warn!(proxy = %url, "building a client for the system proxy");
        let _ = self.proxy_client.set(client.clone());
        Some(client)
    }

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

/// The preference, as a pure function so it can be tested without a network.
fn decide(stats: &PathStats, proxy_available: bool) -> Path {
    if !proxy_available || stats.proxy.samples < MIN_PROXY_SAMPLES {
        return Path::Direct;
    }
    let proxy_ahead = stats.direct.failures > 0
        || (stats.direct.samples > 0 && stats.proxy.ewma_ms < stats.direct.ewma_ms * PREFER_MARGIN);
    if proxy_ahead && !stats.requests.is_multiple_of(DIRECT_PROBE_EVERY) {
        return Path::Proxy;
    }
    Path::Direct
}

/// How long a route stays avoided after it misses the first-byte deadline.
const SLOW_ROUTE_COOLDOWN: Duration = Duration::from_secs(120);

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn stats(direct: &[(u128, bool)], proxy: &[(u128, bool)]) -> PathStats {
        let mut state = PathStats::default();
        for (ms, ok) in direct {
            state.direct.observe(Duration::from_millis(*ms as u64), *ok);
        }
        for (ms, ok) in proxy {
            state.proxy.observe(Duration::from_millis(*ms as u64), *ok);
        }
        state
    }

    #[test]
    fn direct_wins_until_the_proxy_has_evidence() {
        let one = stats(&[(400, true)], &[(100, true)]);
        assert_eq!(decide(&one, true), Path::Direct);
    }

    #[test]
    fn a_clearly_faster_proxy_is_preferred() {
        let mut state = stats(&[(800, true), (800, true)], &[(100, true), (100, true)]);
        state.requests = 1;
        assert_eq!(decide(&state, true), Path::Proxy);
    }

    #[test]
    fn a_failing_direct_path_hands_over_to_the_proxy() {
        let mut state = stats(&[(5000, false), (5000, false)], &[(300, true), (300, true)]);
        state.requests = 1;
        assert_eq!(decide(&state, true), Path::Proxy);
    }

    #[test]
    fn the_preferred_path_is_still_probed_now_and_then() {
        let mut state = stats(&[(800, true), (800, true)], &[(100, true), (100, true)]);
        state.requests = DIRECT_PROBE_EVERY;
        assert_eq!(decide(&state, true), Path::Direct);
    }

    #[test]
    fn no_proxy_configured_means_direct() {
        let state = stats(&[(800, true), (800, true)], &[(100, true), (100, true)]);
        assert_eq!(decide(&state, false), Path::Direct);
    }
}
