//! Reads each account's CommandCode quota so key rotation can prefer an account that
//! can actually answer.
//!
//! The provider's base URL points at the inference path (`…/provider`); the account
//! endpoints live at the host root. They are the same numbers the CommandCode dashboard
//! shows: remaining monthly credits and the five-hour and weekly windows, including when
//! each window resets.
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::join_all;
use gateway_core::config::Provider;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::state::AppState;

/// How long one account endpoint may take. A quota read must never hold up anything
/// else, and a local gateway has no reason to wait longer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// How often quotas are re-read. Windows reset on the minute, not the second, and every
/// request would be wasteful for numbers that only decide the order of a list.
const REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Gives the gateway a moment to start answering before its first outbound probe.
const FIRST_REFRESH_DELAY: Duration = Duration::from_secs(2);

const USER_AGENT: &str = concat!("llm-gateway/", env!("CARGO_PKG_VERSION"));

/// Below this many credits CommandCode answers "insufficient credits" even for a small
/// request, so such an account is treated as unusable until it is topped up or the
/// period rolls over.
const MIN_USABLE_CREDITS: f64 = 1.0;

/// One rate-limit window as the provider reports it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct WindowUsage {
    pub used: f64,
    pub cap: f64,
    pub exceeded: bool,
    /// Milliseconds since the Unix epoch; the API reports it in that unit.
    pub reset_at: Option<u64>,
}

/// What one API key's account looks like right now.
#[derive(Clone, Debug, Default, Serialize)]
pub struct KeyQuota {
    /// The key this entry describes. Never serialized: the admin API should say which
    /// account a row belongs to, not repeat the credential.
    #[serde(skip)]
    pub key: String,
    /// The credential's last four characters, so a row can be matched to a key field.
    pub hint: String,
    pub account: Option<String>,
    pub email: Option<String>,
    /// The subscription's plan id, e.g. `individual-goat`.
    pub plan: Option<String>,
    pub plan_status: Option<String>,
    pub period_end: Option<String>,
    pub monthly_credits: Option<f64>,
    pub purchased_credits: Option<f64>,
    pub free_credits: Option<f64>,
    pub five_hour: Option<WindowUsage>,
    pub weekly: Option<WindowUsage>,
    /// When this reading was taken, in milliseconds since the Unix epoch.
    pub fetched_at: u64,
    /// Why the quota could not be read, when it could not. An entry with an error is
    /// treated as unknown rather than exhausted.
    pub error: Option<String>,
}

impl KeyQuota {
    /// Everything the account can still spend, or `None` when the API said nothing.
    pub fn credits_remaining(&self) -> Option<f64> {
        let values: Vec<f64> = [
            self.monthly_credits,
            self.purchased_credits,
            self.free_credits,
        ]
        .into_iter()
        .flatten()
        .collect();
        if values.is_empty() {
            None
        } else {
            Some(values.iter().sum())
        }
    }

    /// Whether a rate-limit window is closed right now.
    ///
    /// A cached `exceeded` flag stops applying once its reset time has passed, so the
    /// account is usable again even before the next reading confirms it.
    pub fn is_exhausted(&self, now_ms: u64) -> bool {
        window_exhausted(&self.five_hour, now_ms) || window_exhausted(&self.weekly, now_ms)
    }
}

fn window_exhausted(window: &Option<WindowUsage>, now_ms: u64) -> bool {
    window
        .as_ref()
        .is_some_and(|window| window.exceeded && window.reset_at.is_none_or(|reset| reset > now_ms))
}

/// One provider's reading, keyed by the provider it belongs to.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ProviderQuota {
    pub provider_id: String,
    /// False for endpoints that do not expose a quota this gateway understands.
    pub supported: bool,
    pub fetched_at: u64,
    pub keys: Vec<KeyQuota>,
}

/// The CommandCode API root behind an inference base URL.
///
/// `https://api.commandcode.ai/provider` becomes `https://api.commandcode.ai`; a
/// deployment on another host gets no quota support rather than a request to the wrong
/// place.
pub fn api_root(base_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(base_url.trim()).ok()?;
    let host = url.host_str()?;
    if host != "commandcode.ai" && !host.ends_with(".commandcode.ai") {
        return None;
    }
    let mut root = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        root.push(':');
        root.push_str(&port.to_string());
    }
    Some(root)
}

/// Reads every key of `provider`, stores the result, and returns it.
///
/// Keys are read concurrently because they are independent accounts; each read is
/// bounded by [`REQUEST_TIMEOUT`] per HTTP call.
pub async fn refresh(state: &AppState, provider: &Provider) -> ProviderQuota {
    let Some(root) = api_root(&provider.base_url) else {
        return ProviderQuota {
            provider_id: provider.id.clone(),
            supported: false,
            ..ProviderQuota::default()
        };
    };
    let keys = crate::api::provider_keys(provider);
    let entries = join_all(keys.iter().map(|key| probe_key(state, &root, key))).await;
    let quota = ProviderQuota {
        // An unsaved draft has no id yet; its base URL keeps the cache entry distinct.
        provider_id: if provider.id.trim().is_empty() {
            provider.base_url.trim().to_owned()
        } else {
            provider.id.clone()
        },
        supported: true,
        fetched_at: now_ms(),
        keys: entries,
    };
    state.store_quota(quota.clone());
    quota
}

/// Re-reads every enabled, supported provider's quota for the life of the process.
///
/// This is what makes the feature automatic: a key that runs out is skipped on the next
/// request, and one whose window reset comes back without anyone pressing anything.
pub fn spawn_refresh(state: Arc<AppState>) {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_REFRESH_DELAY).await;
        loop {
            let snapshot = state.snapshot();
            for provider in snapshot.config.providers.iter().filter(|p| p.enabled) {
                if api_root(&provider.base_url).is_none() {
                    continue;
                }
                if crate::api::provider_keys(provider).is_empty() {
                    continue;
                }
                let quota = refresh(&state, provider).await;
                if let Some(error) = quota.keys.iter().find_map(|key| key.error.as_deref()) {
                    tracing::warn!(
                        provider = %provider.name,
                        %error,
                        "could not read an account quota"
                    );
                }
            }
            tokio::time::sleep(REFRESH_INTERVAL).await;
        }
    });
}

async fn probe_key(state: &AppState, root: &str, key: &str) -> KeyQuota {
    let mut entry = KeyQuota {
        key: key.to_owned(),
        hint: key_hint(key),
        fetched_at: now_ms(),
        ..KeyQuota::default()
    };

    // The account name is a label; failing to read it must not invalidate the quota.
    let org = match get_json(state, root, "/alpha/whoami", &[], key).await {
        Ok(value) => {
            entry.account = string_at(&value, &["user", "userName"])
                .or_else(|| string_at(&value, &["user", "name"]));
            entry.email = string_at(&value, &["user", "email"]);
            string_at(&value, &["org", "id"])
        }
        Err(_) => None,
    };

    let scope: Vec<(&str, &str)> = org
        .as_deref()
        .map(|id| vec![("orgId", id)])
        .unwrap_or_default();
    match get_json(state, root, "/alpha/billing/credits", &scope, key).await {
        Ok(value) => {
            if let Err(error) = apply_credits(&value, &mut entry) {
                entry.error = Some(format!("额度响应无法解析: {error}"));
            }
        }
        // This is the load-bearing call: without it the key is still usable, but it is
        // ranked as unknown rather than preferred.
        Err(error) => entry.error = Some(error),
    }

    if let Ok(value) = get_json(state, root, "/alpha/billing/subscriptions", &scope, key).await {
        apply_subscription(&value, &mut entry);
    }

    entry
}

/// Reads one JSON document with the key, falling back to the system proxy when the
/// direct path cannot reach the host.
async fn get_json(
    state: &AppState,
    root: &str,
    path: &str,
    query: &[(&str, &str)],
    key: &str,
) -> Result<Value, String> {
    let mut url = reqwest::Url::parse(&format!("{root}{path}"))
        .map_err(|error| format!("bad quota URL: {error}"))?;
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    let build = |client: &reqwest::Client| {
        client
            .get(url.clone())
            .timeout(REQUEST_TIMEOUT)
            .bearer_auth(key)
            .header("accept", "application/json")
            .header("user-agent", USER_AGENT)
    };

    let response = match build(&state.http).send().await {
        Ok(response) => response,
        Err(error) => match state.client_via_system_proxy() {
            Some(client) => build(&client)
                .send()
                .await
                .map_err(|retry| format!("{error}; via proxy: {retry}"))?,
            None => return Err(transport_message(&error)),
        },
    };
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| transport_message(&error))?;
    if !status.is_success() {
        let body: String = body.chars().take(200).collect();
        return Err(format!("HTTP {}: {body}", status.as_u16()));
    }
    serde_json::from_str(&body).map_err(|error| format!("not JSON: {error}"))
}

fn transport_message(error: &reqwest::Error) -> String {
    let mut message = format!("upstream transport error: {error}");
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        message.push_str(" -> ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for part in path {
        current = current.get(*part)?;
    }
    current.as_str().map(str::to_owned)
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawCredits {
    credits: Option<RawBalances>,
    window_limits: Option<RawWindows>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawBalances {
    monthly_credits: Option<f64>,
    purchased_credits: Option<f64>,
    free_credits: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawWindows {
    /// "weekly" or "five_hour" when the provider reports the limit at the top level.
    exceeded: Option<String>,
    five_hour: Option<RawWindow>,
    weekly: Option<RawWindow>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawWindow {
    used: Option<f64>,
    cap: Option<f64>,
    exceeded: Option<bool>,
    reset_at: Option<u64>,
}

impl From<RawWindow> for WindowUsage {
    fn from(raw: RawWindow) -> Self {
        WindowUsage {
            used: raw.used.unwrap_or(0.0),
            cap: raw.cap.unwrap_or(0.0),
            exceeded: raw.exceeded.unwrap_or(false),
            reset_at: raw.reset_at,
        }
    }
}

fn apply_credits(value: &Value, entry: &mut KeyQuota) -> Result<(), String> {
    let raw: RawCredits =
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    if let Some(balances) = raw.credits {
        entry.monthly_credits = balances.monthly_credits;
        entry.purchased_credits = balances.purchased_credits;
        entry.free_credits = balances.free_credits;
    }
    if let Some(windows) = raw.window_limits {
        entry.five_hour = windows.five_hour.map(WindowUsage::from);
        entry.weekly = windows.weekly.map(WindowUsage::from);
        match windows.exceeded.as_deref() {
            Some("weekly") => {
                if let Some(window) = &mut entry.weekly {
                    window.exceeded = true;
                }
            }
            Some("five_hour") | Some("fiveHour") => {
                if let Some(window) = &mut entry.five_hour {
                    window.exceeded = true;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn apply_subscription(value: &Value, entry: &mut KeyQuota) {
    let data = value.get("data").unwrap_or(value);
    // Some deployments wrap the subscription in a one-element list.
    let data = match data {
        Value::Array(items) => match items.first() {
            Some(item) => item,
            None => return,
        },
        other => other,
    };
    if data.is_null() {
        return;
    }
    entry.plan = string_at(data, &["planId"]);
    entry.plan_status = string_at(data, &["status"]);
    entry.period_end = string_at(data, &["currentPeriodEnd"]);
}

/// The last four characters, which is enough to match a row to a key field.
fn key_hint(key: &str) -> String {
    let tail: String = key.chars().rev().take(4).collect();
    let tail: String = tail.chars().rev().collect();
    format!("…{tail}")
}

/// How strongly a key should be preferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    /// Known to have credit and no closed window.
    Preferred,
    /// Nothing is known; leave the configured order alone.
    Unknown,
    /// Out of credits, inside a closed window, or recently refused.
    Blocked,
}

/// Orders keys so an account that can answer is tried first.
///
/// Returns whether any key was actually ranked. When nothing is known about a provider,
/// the caller should keep rotating through the keys rather than pinning every request to
/// the first one. Blocked keys stay in the list: if every account is out, trying one is
/// still better than refusing locally.
pub fn rank_keys(
    keys: Vec<String>,
    quota: Option<&ProviderQuota>,
    cooling: &HashSet<String>,
    now_ms: u64,
) -> (Vec<String>, bool) {
    let Some(quota) = quota.filter(|quota| quota.supported && !quota.keys.is_empty()) else {
        return (keys, false);
    };
    let mut informed = false;
    let mut scored: Vec<(Tier, f64, usize, String)> = keys
        .into_iter()
        .enumerate()
        .map(|(index, key)| {
            let entry = quota.keys.iter().find(|entry| entry.key == key);
            let (tier, sorting) = match entry {
                Some(entry) => {
                    let known = entry.credits_remaining().is_some();
                    informed |= known;
                    let credits = entry.credits_remaining().unwrap_or(0.0);
                    if !known {
                        (Tier::Unknown, 0.0)
                    } else if entry.is_exhausted(now_ms) || credits < MIN_USABLE_CREDITS {
                        (Tier::Blocked, -credits)
                    } else {
                        (Tier::Preferred, -credits)
                    }
                }
                None => (Tier::Unknown, 0.0),
            };
            (tier, sorting, index, key)
        })
        .collect();

    for (tier, _, _, key) in &mut scored {
        if cooling.contains(key) {
            *tier = Tier::Blocked;
        }
    }
    scored.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then(
                left.1
                    .partial_cmp(&right.1)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(left.2.cmp(&right.2))
    });
    (
        scored.into_iter().map(|(_, _, _, key)| key).collect(),
        informed,
    )
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(key: &str, monthly: f64, weekly_exceeded: bool) -> KeyQuota {
        KeyQuota {
            key: key.into(),
            monthly_credits: Some(monthly),
            weekly: Some(WindowUsage {
                used: 1.0,
                cap: 35.0,
                exceeded: weekly_exceeded,
                reset_at: Some(now_ms() + 60_000),
            }),
            ..KeyQuota::default()
        }
    }

    #[test]
    fn the_account_endpoints_sit_at_the_host_root() {
        assert_eq!(
            api_root("https://api.commandcode.ai/provider").as_deref(),
            Some("https://api.commandcode.ai")
        );
        assert_eq!(
            api_root("https://api.commandcode.ai/provider/").as_deref(),
            Some("https://api.commandcode.ai")
        );
        assert_eq!(api_root("https://api.deepseek.com/anthropic"), None);
        assert_eq!(api_root("not a url"), None);
    }

    #[test]
    fn credits_parse_from_the_dashboard_shape() {
        let mut entry = KeyQuota::default();
        apply_credits(
            &json!({
                "credits": {"belowThreshold": false, "monthlyCredits": 55.8, "purchasedCredits": 0, "freeCredits": 1.5},
                "windowLimits": {
                    "limited": true,
                    "exceeded": "weekly",
                    "fiveHour": {"used": 12.3, "cap": 14, "exceeded": false, "resetAt": 1792530941688u64},
                    "weekly": {"used": 14.2, "cap": 35, "exceeded": false, "resetAt": 1793094634502u64}
                }
            }),
            &mut entry,
        )
        .unwrap();
        assert_eq!(entry.monthly_credits, Some(55.8));
        assert_eq!(entry.purchased_credits, Some(0.0));
        assert_eq!(entry.credits_remaining(), Some(57.3));
        let five_hour = entry.five_hour.as_ref().unwrap();
        assert_eq!(five_hour.cap, 14.0);
        assert_eq!(five_hour.reset_at, Some(1792530941688));
        // The top-level `exceeded: "weekly"` marks the matching window even when the
        // window object itself carries no flag.
        assert!(entry.weekly.as_ref().unwrap().exceeded);
    }

    #[test]
    fn an_exceeded_window_expires_at_its_reset_time() {
        let entry = KeyQuota {
            weekly: Some(WindowUsage {
                used: 35.0,
                cap: 35.0,
                exceeded: true,
                reset_at: Some(1_000),
            }),
            ..KeyQuota::default()
        };
        assert!(entry.is_exhausted(999));
        assert!(!entry.is_exhausted(1_000));
        assert!(!entry.is_exhausted(1_001));
    }

    #[test]
    fn the_best_account_comes_first_and_exhausted_ones_last() {
        let keys = vec!["exceeded".into(), "small".into(), "big".into()];
        let quota = ProviderQuota {
            provider_id: "p".into(),
            supported: true,
            fetched_at: 0,
            keys: vec![
                entry("exceeded", 34.9, true),
                entry("small", 2.0, false),
                entry("big", 55.8, false),
            ],
        };
        let (ranked, informed) = rank_keys(keys, Some(&quota), &HashSet::new(), now_ms());
        assert!(informed);
        assert_eq!(ranked, vec!["big", "small", "exceeded"]);
    }

    #[test]
    fn an_account_too_close_to_zero_is_not_preferred() {
        let keys = vec!["almost-empty".into(), "funded".into()];
        let quota = ProviderQuota {
            provider_id: "p".into(),
            supported: true,
            fetched_at: 0,
            keys: vec![
                entry("almost-empty", 0.107, false),
                entry("funded", 5.0, false),
            ],
        };
        let (ranked, informed) = rank_keys(keys, Some(&quota), &HashSet::new(), now_ms());
        assert!(informed);
        assert_eq!(ranked, vec!["funded", "almost-empty"]);
    }

    #[test]
    fn a_recently_refused_key_is_blocked() {
        let keys = vec!["a".into(), "b".into()];
        let quota = ProviderQuota {
            provider_id: "p".into(),
            supported: true,
            fetched_at: 0,
            keys: vec![entry("a", 100.0, false), entry("b", 50.0, false)],
        };
        let cooling = HashSet::from(["a".to_owned()]);
        let (ranked, _) = rank_keys(keys, Some(&quota), &cooling, now_ms());
        assert_eq!(ranked, vec!["b", "a"]);
    }

    #[test]
    fn no_known_quota_leaves_the_order_alone() {
        let keys = vec!["a".into(), "b".into()];
        let (ranked, informed) = rank_keys(keys.clone(), None, &HashSet::new(), now_ms());
        assert_eq!(ranked, keys);
        assert!(!informed);

        let unknown = ProviderQuota {
            provider_id: "p".into(),
            supported: true,
            fetched_at: 0,
            keys: vec![KeyQuota {
                key: "a".into(),
                error: Some("unreachable".into()),
                ..KeyQuota::default()
            }],
        };
        let (ranked, informed) = rank_keys(keys.clone(), Some(&unknown), &HashSet::new(), now_ms());
        assert_eq!(ranked, keys);
        assert!(!informed);
    }

    #[test]
    fn a_single_known_best_account_is_not_rotated_away() {
        // The caller uses `informed` to decide between starting at the first key or
        // rotating; only one known-usable account should not be skipped half the time.
        let quota = ProviderQuota {
            provider_id: "p".into(),
            supported: true,
            fetched_at: 0,
            keys: vec![entry("only", 10.0, false)],
        };
        let (ranked, informed) = rank_keys(
            vec!["other".into(), "only".into()],
            Some(&quota),
            &HashSet::new(),
            now_ms(),
        );
        assert!(informed);
        assert_eq!(ranked, vec!["only", "other"]);
    }
}
