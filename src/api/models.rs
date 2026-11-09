//! Model discovery and provider-availability filtering.
#[cfg(test)]
use crate::config;
use crate::{
    app_state::{AppState, AuthKeyContext, ProviderEligibilityMode},
    config::AppConfig,
};
use axum::{
    extract::{Extension, State},
    response::IntoResponse,
    Json,
};
use std::{collections::HashMap, sync::Arc};

fn build_model_list(cfg: &AppConfig, cache: &HashMap<String, Vec<String>>) -> serde_json::Value {
    let enabled = cfg
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .map(|provider| provider.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut data = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for provider in cfg.providers.iter().filter(|provider| provider.enabled) {
        for model in &provider.models {
            if seen.insert(model.clone()) {
                data.push(serde_json::json!({
                    "id": model,
                    "object": "model",
                    "owned_by": provider.name
                }));
            }
        }
    }

    for (provider, models) in cache {
        if !enabled.contains(provider.as_str()) {
            continue;
        }
        for model in models {
            if seen.insert(model.clone()) {
                data.push(serde_json::json!({
                    "id": model,
                    "object": "model",
                    "owned_by": provider,
                    "discovered": true
                }));
            }
        }
    }
    serde_json::json!({ "object": "list", "data": data })
}

fn retain_model_owners(model_list: &mut serde_json::Value, available: impl Fn(&str) -> bool) {
    if let Some(models) = model_list
        .get_mut("data")
        .and_then(serde_json::Value::as_array_mut)
    {
        models.retain(|model| {
            model
                .get("owned_by")
                .and_then(serde_json::Value::as_str)
                .is_some_and(&available)
        });
    }
}

pub(crate) async fn models(
    State(state): State<Arc<AppState>>,
    auth_context: Option<Extension<AuthKeyContext>>,
) -> impl IntoResponse {
    let cache = state.models_cache.read().await;
    let mut model_list = build_model_list(&state.cfg, &cache);
    retain_model_owners(&mut model_list, |provider| {
        state.provider_eligible(provider, ProviderEligibilityMode::ReadOnly)
    });
    if let Some(Extension(context)) = auth_context {
        if let Some(allowed) = context.provider_allowlist {
            if let Some(models) = model_list
                .get_mut("data")
                .and_then(serde_json::Value::as_array_mut)
            {
                models.retain(|model| {
                    model
                        .get("owned_by")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|provider| allowed.iter().any(|item| item == provider))
                });
            }
        }
    }
    Json(model_list)
}

#[cfg(test)]
mod model_list_tests {
    use super::*;

    fn provider(name: &str, enabled: bool, models: &[&str]) -> config::ProviderConfig {
        config::ProviderConfig {
            name: name.to_owned(),
            enabled,
            priority: 1,
            timeout_seconds: 30,
            completion_idle_ms: 15_000,
            queue_timeout_seconds: 30,
            session_pool_size: 1,
            min_request_interval_ms: 5_000,
            daily_request_cap: 100,
            models: models.iter().map(|model| (*model).to_owned()).collect(),
            selectors: config::ProviderSelectorOverrides::default(),
        }
    }

    #[test]
    fn model_list_omits_providers_that_are_not_currently_routable() {
        let mut list = serde_json::json!({
            "object": "list",
            "data": [
                { "id": "chatgpt-model", "owned_by": "chatgpt" },
                { "id": "claude-model", "owned_by": "claude" },
                { "id": "unowned-model" }
            ]
        });
        retain_model_owners(&mut list, |provider| provider == "chatgpt");
        assert_eq!(list["data"].as_array().unwrap().len(), 1);
        assert_eq!(list["data"][0]["id"], "chatgpt-model");
    }

    #[test]
    fn excludes_configured_and_discovered_models_for_disabled_providers() {
        let cfg = AppConfig {
            server: config::ServerConfig::default(),
            memory: config::MemoryConfig::default(),
            obscura: config::ObscuraConfig::default(),
            auth: config::AuthConfig::default(),
            providers: vec![
                provider("chatgpt", true, &["gpt-live"]),
                provider("claude", false, &["claude-disabled"]),
            ],
        };
        let cache = HashMap::from([
            ("chatgpt".to_owned(), vec!["gpt-discovered".to_owned()]),
            ("claude".to_owned(), vec!["claude-cached".to_owned()]),
        ]);

        let value = build_model_list(&cfg, &cache);
        let ids = value["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["gpt-live", "gpt-discovered"]);
    }

    #[test]
    fn configured_ids_win_over_duplicate_discovered_ids() {
        let cfg = AppConfig {
            server: config::ServerConfig::default(),
            memory: config::MemoryConfig::default(),
            obscura: config::ObscuraConfig::default(),
            auth: config::AuthConfig::default(),
            providers: vec![provider("chatgpt", true, &["gpt-live"])],
        };
        let cache = HashMap::from([(
            "chatgpt".to_owned(),
            vec!["gpt-live".to_owned(), "gpt-extra".to_owned()],
        )]);

        let value = build_model_list(&cfg, &cache);
        let data = value["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["owned_by"], "chatgpt");
        assert!(data[0].get("discovered").is_none());
        assert_eq!(data[1]["id"], "gpt-extra");
        assert_eq!(data[1]["discovered"], true);
    }
}
