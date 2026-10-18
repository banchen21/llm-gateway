//! Rejects configurations that would fail later in a confusing way.

use std::collections::HashMap;

use super::Config;
use crate::error::{Error, Result};

pub fn validate(config: &Config) -> Result<()> {
    aliases_are_unique(config)?;
    non_loopback_requires_token(config)?;
    Ok(())
}

/// Two enabled models sharing an alias would make routing depend on config order.
fn aliases_are_unique(config: &Config) -> Result<()> {
    let mut seen: HashMap<&str, &str> = HashMap::new();
    for provider in config.providers.iter().filter(|provider| provider.enabled) {
        for model in provider.models.iter().filter(|model| model.enabled) {
            let alias = model.alias.trim();
            if alias.is_empty() {
                return Err(Error::config(format!(
                    "provider `{}` has a model with an empty alias",
                    provider.name
                )));
            }
            if let Some(previous) = seen.insert(alias, &provider.name) {
                return Err(Error::config(format!(
                    "model alias `{alias}` is configured by both `{previous}` and `{}`",
                    provider.name
                )));
            }
        }
    }
    Ok(())
}

/// Enabling the web GUI on a reachable address exposes provider API keys and the
/// administrative API, so it may not be done without a token.
fn non_loopback_requires_token(config: &Config) -> Result<()> {
    if !config.web_ui.enabled || is_loopback(&config.server.bind) {
        return Ok(());
    }
    if config
        .web_ui
        .token
        .as_ref()
        .is_some_and(|token| !token.is_empty())
    {
        return Ok(());
    }
    Err(Error::config(format!(
        "binding to `{}` with the web GUI enabled requires `web_ui.token`",
        config.server.bind
    )))
}

fn is_loopback(bind: &str) -> bool {
    let bind = bind.trim();
    bind == "localhost"
        || bind
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}
