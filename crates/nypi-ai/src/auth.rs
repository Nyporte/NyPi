//! API key resolution: environment variables first, then `~/.nypi/auth.json`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// Environment variable Pi uses for each provider.
pub fn env_var_for(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "google" => "GEMINI_API_KEY",
        // A Bedrock API key; without one the adapter signs requests with AWS credentials.
        "amazon-bedrock" => "AWS_BEARER_TOKEN_BEDROCK",
        "openai" => "OPENAI_API_KEY",
        "azure" => "AZURE_OPENAI_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "vercel-ai-gateway" => "AI_GATEWAY_API_KEY",
        "groq" => "GROQ_API_KEY",
        "cerebras" => "CEREBRAS_API_KEY",
        "xai" => "XAI_API_KEY",
        "nvidia" => "NVIDIA_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        "huggingface" => "HF_TOKEN",
        "fireworks" => "FIREWORKS_API_KEY",
        "together" => "TOGETHER_API_KEY",
        "baseten" => "BASETEN_API_KEY",
        "zai" => "ZAI_API_KEY",
        "zai-coding-cn" => "ZAI_CODING_CN_API_KEY",
        "minimax" => "MINIMAX_API_KEY",
        "minimax-cn" => "MINIMAX_CN_API_KEY",
        "moonshotai" | "moonshotai-cn" => "MOONSHOT_API_KEY",
        "kimi-coding" => "KIMI_API_KEY",
        "opencode" | "opencode-go" => "OPENCODE_API_KEY",
        "qwen-token-plan" | "qwen-token-plan-individual" => "QWEN_TOKEN_PLAN_API_KEY",
        "qwen-token-plan-cn" => "QWEN_TOKEN_PLAN_CN_API_KEY",
        "ant-ling" => "ANT_LING_API_KEY",
        "xiaomi" => "XIAOMI_API_KEY",
        "xiaomi-token-plan-cn" => "XIAOMI_TOKEN_PLAN_CN_API_KEY",
        "xiaomi-token-plan-ams" => "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
        "xiaomi-token-plan-sgp" => "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
        _ => return None,
    })
}

/// Resolve the key for `model`: its own `apiKeyEnv` first, then the provider lookup.
pub fn resolve_model_key(model: &crate::models::Model, auth_file: &Path) -> Option<String> {
    model.api_key_env.as_deref().and_then(lookup_env).or_else(|| resolve_api_key(&model.provider, auth_file))
}

fn env_file_values() -> &'static Mutex<HashMap<String, String>> {
    static VALUES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    VALUES.get_or_init(Default::default)
}

/// Load `KEY=value` lines from `path` into a private lookup table. Values are deliberately
/// not exported to the process environment, so tool subprocesses (bash) cannot read
/// unrelated secrets that happen to live in the same file. Missing files are ignored.
pub fn load_env_file(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let mut values = env_file_values().lock().unwrap();
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'');
        values.entry(key.to_string()).or_insert_with(|| value.to_string());
    }
}

/// A variable from the process environment, else from loaded env files. Empty values count as unset.
pub fn lookup_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .or_else(|| env_file_values().lock().unwrap().get(name).cloned())
        .filter(|value| !value.is_empty())
}

/// Resolve the key for `provider`: `<PROVIDER>` env var (Pi's names, then `NYPI_<PROVIDER>_API_KEY`),
/// then the `auth.json` map `{ "provider": "key" }`. Returns None when no key is configured.
pub fn resolve_api_key(provider: &str, auth_file: &Path) -> Option<String> {
    let generic = format!("NYPI_{}_API_KEY", provider.to_uppercase().replace('-', "_"));
    let from_env = env_var_for(provider).and_then(lookup_env).or_else(|| lookup_env(&generic));
    if from_env.is_some() {
        return from_env;
    }
    let text = std::fs::read_to_string(auth_file).ok()?;
    let map: BTreeMap<String, String> = serde_json::from_str(&text).ok()?;
    map.get(provider).cloned().filter(|key| !key.is_empty())
}
