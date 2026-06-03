use std::time::Duration;

use keyring_core::Entry;
use reqwest::blocking::Client;
use serde::Serialize;
use tiktoken_rs::get_bpe_from_model;

pub const SERVICE_NAME: &str = "com.caguabot.minimonitor";

#[derive(Clone, Serialize)]
pub struct ProviderState {
    pub provider: &'static str,
    pub connected: bool,
    pub configured: bool,
    pub status: String,
    pub requires_sign_in: bool,
}

#[derive(Clone, Serialize)]
pub struct TokenEstimateResult {
    pub model: String,
    pub token_count: usize,
    pub mode: String,
}

pub fn estimate_tokens(model: &str, text: &str) -> TokenEstimateResult {
    let model = if model.trim().is_empty() {
        "gpt-4o-mini".to_owned()
    } else {
        model.to_owned()
    };

    if model.to_ascii_lowercase().contains("claude") {
        TokenEstimateResult {
            model,
            token_count: anthropic_estimate(text),
            mode: "estimated".to_owned(),
        }
    } else {
        let token_count = get_bpe_from_model(&model)
            .map(|bpe| bpe.encode_with_special_tokens(text).len())
            .unwrap_or_else(|_| anthropic_estimate(text));
        TokenEstimateResult {
            model,
            token_count,
            mode: "openai-compatible".to_owned(),
        }
    }
}

fn anthropic_estimate(text: &str) -> usize {
    (text.chars().count() / 4).max(1)
}

pub fn provider_key(provider: &str) -> Option<String> {
    Entry::new(SERVICE_NAME, provider)
        .ok()
        .and_then(|entry| entry.get_password().ok())
        .filter(|value| !value.is_empty())
}

pub fn normalize_provider(provider: &str) -> &'static str {
    if provider.eq_ignore_ascii_case("anthropic") {
        "anthropic"
    } else {
        "openai"
    }
}

pub fn set_provider_key(provider: &str, key: &str) -> Result<(), String> {
    let entry = Entry::new(SERVICE_NAME, provider).map_err(|e| e.to_string())?;
    entry.set_password(key).map_err(|e| e.to_string())
}

pub fn clear_provider_key(provider: &str) -> Result<(), String> {
    let entry = Entry::new(SERVICE_NAME, provider).map_err(|e| e.to_string())?;
    entry.delete_credential().map_err(|e| e.to_string())
}

pub fn validate_provider(provider: &str) -> Result<reqwest::StatusCode, String> {
    let api_key =
        provider_key(provider).ok_or_else(|| format!("No {provider} key configured"))?;
    let client = Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| e.to_string())?;

    let response = match provider {
        "openai" => client
            .get("https://api.openai.com/v1/models")
            .bearer_auth(api_key)
            .send(),
        "anthropic" => client
            .get("https://api.anthropic.com/v1/models")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .send(),
        _ => return Err("unknown provider".into()),
    };

    response.map(|r| r.status()).map_err(|e| e.to_string())
}

pub fn provider_state(provider: &'static str) -> ProviderState {
    let configured = provider_key(provider).is_some();
    let status = if configured {
        "Key stored in Keychain".to_owned()
    } else {
        "Disconnected".to_owned()
    };

    ProviderState {
        provider,
        connected: configured,
        configured,
        status,
        requires_sign_in: true,
    }
}
