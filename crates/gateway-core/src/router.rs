//! Resolves the model name a client asked for to a provider and that provider's own
//! model id.
use std::collections::HashMap;

use crate::config::{Config, Provider, ProviderModel};
use crate::error::{Error, Result};

/// Where a resolved request should be sent.
pub struct Route<'a> {
    pub provider: &'a Provider,
    pub model: &'a ProviderModel,
}

/// An index over the enabled aliases in a [`Config`].
///
/// Rebuilt whenever the configuration changes; it holds ids rather than references so
/// it can outlive the borrow that produced it.
#[derive(Default)]
pub struct Router {
    /// alias -> (provider id, model id)
    routes: HashMap<String, (String, String)>,
}

impl Router {
    pub fn build(config: &Config) -> Self {
        let mut routes = HashMap::new();
        for provider in config.providers.iter().filter(|provider| provider.enabled) {
            for model in provider.models.iter().filter(|model| model.enabled) {
                routes.insert(model.alias.clone(), (provider.id.clone(), model.id.clone()));
            }
        }
        Router { routes }
    }

    pub fn resolve<'a>(&self, config: &'a Config, alias: &str) -> Result<Route<'a>> {
        if let Some((provider_id, model_id)) = self.routes.get(alias) {
            let provider = config
                .providers
                .iter()
                .find(|provider| &provider.id == provider_id)
                .ok_or_else(|| {
                    Error::not_found(format!("model `{alias}` is no longer configured"))
                })?;
            let model = provider
                .models
                .iter()
                .find(|model| &model.id == model_id)
                .ok_or_else(|| {
                    Error::not_found(format!("model `{alias}` is no longer configured"))
                })?;
            return Ok(Route { provider, model });
        }

        // A client that asks for an unrecognized model still reaches the configured
        // catch-all, which is what makes a gateway usable with clients that hardcode
        // their own model names.
        let Some(default_id) = &config.default_provider_id else {
            return Err(Error::not_found(format!(
                "no provider is configured for model `{alias}`"
            )));
        };
        let provider = config
            .providers
            .iter()
            .find(|provider| &provider.id == default_id && provider.enabled)
            .ok_or_else(|| {
                Error::config(format!(
                    "default_provider_id `{default_id}` does not name an enabled provider"
                ))
            })?;
        let model = provider
            .models
            .iter()
            .find(|model| model.enabled)
            .ok_or_else(|| {
                Error::config(format!(
                    "default provider `{}` has no enabled model",
                    provider.name
                ))
            })?;
        Ok(Route { provider, model })
    }

    /// Every enabled alias, for `/v1/models`.
    pub fn aliases(&self) -> Vec<&str> {
        let mut aliases: Vec<&str> = self.routes.keys().map(String::as_str).collect();
        aliases.sort_unstable();
        aliases
    }
}
