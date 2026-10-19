//! Defines the gateway configuration and where it lives.

mod validate;

pub use validate::validate;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::upstream::{MaxTokensField, ProviderProtocol};

/// Which inbound protocols the gateway serves.
///
/// The mode gates the gateway's own endpoints only; providers of either protocol may
/// be configured in any mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolMode {
    /// Anthropic Messages only, at `/v1/messages`.
    #[default]
    PrimaryAnthropic,
    /// OpenAI Chat Completions only, at `/v1/chat/completions`.
    PrimaryOpenAi,
    /// Both, at their respective paths.
    Route,
}

impl ProtocolMode {
    pub fn serves_anthropic(&self) -> bool {
        matches!(self, ProtocolMode::PrimaryAnthropic | ProtocolMode::Route)
    }

    pub fn serves_openai(&self) -> bool {
        matches!(self, ProtocolMode::PrimaryOpenAi | ProtocolMode::Route)
    }
}

/// The whole configuration.
///
/// Every field is serialized, including empty ones, because this struct is also the
/// admin API's response body. A shape that varied with whether a map happened to be
/// empty would force every consumer to treat present-but-empty and absent differently.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub protocol_mode: ProtocolMode,
    pub server: ServerConfig,
    pub web_ui: WebUiConfig,
    pub providers: Vec<Provider>,
    /// Catch-all used when a client asks for a model no alias matches.
    pub default_provider_id: Option<String>,
    /// When the provider an alias names refuses the request (quota, credential, plan),
    /// try the other enabled providers before failing the call.
    pub failover: bool,
    /// Abandon a call that has not produced its first byte within this many
    /// milliseconds, mark the route slow, and answer through another provider.
    ///
    /// Zero disables it. This is what turns "working but 14 seconds" into "working":
    /// a provider that is merely slow never fails, so failover alone never sees it.
    pub slow_after_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: String,
    pub port: u16,
    /// When set, inbound requests must present it as `x-api-key` or a bearer token.
    ///
    /// Kept alongside [`Self::api_keys`] because configurations written before named keys
    /// existed carry only this field, and an existing key has to keep working.
    pub api_key: Option<String>,
    /// Named inbound keys, each usable as `x-api-key` or a bearer token.
    ///
    /// One key per client is what makes a key revocable without disturbing the others.
    pub api_keys: Vec<ApiKey>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind: "127.0.0.1".into(),
            port: 8787,
            api_key: None,
            api_keys: Vec::new(),
        }
    }
}

/// One named inbound key.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiKey {
    /// Assigned by the admin API when the entry is created without one.
    pub id: String,
    /// A label for the client the key was issued to, e.g. "Claude Desktop".
    pub name: String,
    pub key: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WebUiConfig {
    /// Serves the GUI over the gateway's port. Off by default.
    pub enabled: bool,
    /// Required for administrative access when enabled.
    pub token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub protocol: ProviderProtocol,
    /// API root without a path, e.g. `https://api.anthropic.com`.
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// Additional keys for the same endpoint, tried in turn when one is rate limited
    /// or rejected. Accounts sold separately each carry their own quota, so rotating
    /// across them is what keeps a plan-wide limit from stopping every request.
    #[serde(default)]
    pub api_keys: Vec<ApiKey>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default)]
    pub custom_headers: BTreeMap<String, String>,
    #[serde(default)]
    pub models: Vec<ProviderModel>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderModel {
    pub id: String,
    /// The name clients call this model by. Must be unique across enabled providers.
    pub alias: String,
    /// The provider's own model id.
    pub upstream_model: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// Caps output when a client omits `max_tokens`, which Anthropic requires.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Which field carries the output limit for an OpenAI-compatible upstream.
    #[serde(default)]
    pub max_tokens_field: MaxTokensField,
    /// Extra fields merged into the upstream request body, for provider-specific
    /// parameters the gateway does not model.
    #[serde(default)]
    pub extra: BTreeMap<String, Value>,
}

fn enabled_by_default() -> bool {
    true
}

/// The configuration file's location in the platform's config directory.
pub fn config_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "llm-gateway")
        .ok_or_else(|| Error::config("could not determine the platform config directory"))?;
    Ok(dirs.config_dir().join("config.json"))
}

/// Reads the configuration, treating a missing file as an empty first run.
pub fn load(path: &Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(error.into()),
    };
    let config: Config = serde_json::from_str(&text).map_err(|error| {
        Error::config(format!("{} is not valid config: {error}", path.display()))
    })?;
    Ok(config)
}

/// Writes the configuration so a reader never observes a partial file.
///
/// The GUI and the admin API can write concurrently, and a truncated config would lose
/// every configured provider, so the new contents land in a temporary file that is then
/// renamed over the target. `rename` replaces the destination atomically on both Unix
/// and Windows (`MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`).
pub fn save(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_string_pretty(config)?)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}
