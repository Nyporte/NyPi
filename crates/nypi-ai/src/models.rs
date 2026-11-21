//! Model metadata: the built-in catalog (generated from Pi's catalog by
//! `scripts/nypi-gen-models.py`) plus user-defined models.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::types::Usage;

/// Wire protocol a model is served through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Api {
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "openai-completions")]
    OpenAiCompletions,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "google-generative-ai")]
    GoogleGenerativeAi,
    #[serde(rename = "bedrock-converse-stream")]
    BedrockConverseStream,
    /// Scripted responses for tests.
    #[serde(rename = "faux")]
    Faux,
}

impl Api {
    pub fn as_str(self) -> &'static str {
        match self {
            Api::AnthropicMessages => "anthropic-messages",
            Api::OpenAiCompletions => "openai-completions",
            Api::OpenAiResponses => "openai-responses",
            Api::GoogleGenerativeAi => "google-generative-ai",
            Api::BedrockConverseStream => "bedrock-converse-stream",
            Api::Faux => "faux",
        }
    }
}

/// Requested reasoning effort.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub const ALL: [ThinkingLevel; 7] = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
        ThinkingLevel::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.as_str() == text)
    }
}

/// Price per million tokens, in USD.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

/// Per-model wire quirks. Names follow Pi's `compat` fields.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Compat {
    /// Anthropic: use `thinking: {type: "adaptive"}` + `output_config.effort` instead of a token budget.
    #[serde(default)]
    pub force_adaptive_thinking: bool,
    #[serde(default)]
    pub supports_temperature: Option<bool>,
    /// OpenAI-compatible: how reasoning is requested (`openai`, `openrouter`, `deepseek`, `qwen`, `zai`, `together`, ...).
    #[serde(default)]
    pub thinking_format: Option<String>,
    /// OpenAI-compatible: `max_tokens` or `max_completion_tokens`.
    #[serde(default)]
    pub max_tokens_field: Option<String>,
    #[serde(default)]
    pub supports_developer_role: bool,
    #[serde(default)]
    pub supports_reasoning_effort: bool,
    #[serde(default)]
    pub requires_reasoning_content_on_assistant_messages: bool,
    #[serde(default)]
    pub supports_usage_in_streaming: Option<bool>,
    /// The model accepts system (or developer) messages after the conversation started, so a
    /// prompt change can be appended instead of rewriting the cached head (decision D40). Off
    /// unless verified; then later system messages are folded into the leading one.
    #[serde(default)]
    pub supports_mid_convo_system_messages: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: Api,
    pub provider: String,
    pub base_url: String,
    #[serde(default)]
    pub reasoning: bool,
    /// Accepted input kinds: "text", "image".
    #[serde(default = "default_input")]
    pub input: Vec<String>,
    #[serde(default)]
    pub cost: ModelCost,
    pub context_window: u64,
    pub max_tokens: u64,
    /// Provider-native effort per level; `null` means the level is unsupported.
    #[serde(default)]
    pub thinking_level_map: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub compat: Compat,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Environment variable holding this model's API key, overriding the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

fn default_input() -> Vec<String> {
    vec!["text".to_string()]
}

impl Model {
    pub fn supports_images(&self) -> bool {
        self.input.iter().any(|kind| kind == "image")
    }

    /// `provider/id`, the canonical model reference.
    pub fn reference(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    /// The thinking levels this model accepts (Pi's `getSupportedThinkingLevels`): only off
    /// without reasoning; a level the catalog maps to null is unsupported, and `xhigh` and `max`
    /// need a mapping of their own.
    pub fn supported_thinking_levels(&self) -> Vec<ThinkingLevel> {
        if !self.reasoning {
            return vec![ThinkingLevel::Off];
        }
        ThinkingLevel::ALL
            .into_iter()
            .filter(|level| match self.thinking_level_map.get(level.as_str()) {
                Some(None) => false,
                mapped => !matches!(level, ThinkingLevel::Xhigh | ThinkingLevel::Max) || mapped.is_some(),
            })
            .collect()
    }

    /// `level` if this model accepts it, else the nearest level it does: higher first, then
    /// lower (Pi's `clampThinkingLevel`).
    pub fn clamp_thinking(&self, level: ThinkingLevel) -> ThinkingLevel {
        let supported = self.supported_thinking_levels();
        let higher = ThinkingLevel::ALL.into_iter().filter(|l| *l >= level);
        let lower = ThinkingLevel::ALL.into_iter().rev().filter(|l| *l < level);
        higher.chain(lower).find(|l| supported.contains(l)).unwrap_or(ThinkingLevel::Off)
    }

    /// Provider-native effort string for `level`, clamped to what the model accepts, or None
    /// when thinking should be off.
    pub fn native_effort(&self, level: ThinkingLevel) -> Option<String> {
        let level = self.clamp_thinking(level);
        if !self.reasoning || level == ThinkingLevel::Off {
            return None;
        }
        match self.thinking_level_map.get(level.as_str()) {
            Some(Some(native)) => Some(native.clone()),
            // Unsupported at this level: fall back to the nearest supported higher level, then lower.
            Some(None) => {
                let higher = ThinkingLevel::ALL.iter().filter(|l| **l > level);
                let lower = ThinkingLevel::ALL.iter().rev().filter(|l| **l < level && **l != ThinkingLevel::Off);
                higher.chain(lower).find_map(|l| self.thinking_level_map.get(l.as_str()).cloned().flatten())
            }
            None => Some(level.as_str().to_string()),
        }
    }

    /// False when the catalog marks "off" as unsupported (the model always reasons).
    pub fn thinking_can_be_disabled(&self) -> bool {
        !matches!(self.thinking_level_map.get("off"), Some(None))
    }

    /// Fill in `usage.cost` from this model's prices.
    pub fn apply_cost(&self, usage: &mut Usage) {
        let per = |tokens: u64, price: f64| tokens as f64 * price / 1_000_000.0;
        usage.cost.input = per(usage.input, self.cost.input);
        usage.cost.output = per(usage.output, self.cost.output);
        usage.cost.cache_read = per(usage.cache_read, self.cost.cache_read);
        usage.cost.cache_write = per(usage.cache_write, self.cost.cache_write);
        usage.cost.total = usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    }
}

const BUILTIN_MODELS: &str = include_str!("../data/models.json");

/// Built-in catalog plus user models. User models with the same reference replace built-ins.
#[derive(Clone, Debug)]
pub struct ModelRegistry {
    models: Vec<Model>,
}

impl ModelRegistry {
    /// Built-in catalog only.
    pub fn builtin() -> Self {
        let models: Vec<Model> = serde_json::from_str(BUILTIN_MODELS).expect("built-in model catalog is valid JSON");
        ModelRegistry { models }
    }

    /// Built-in catalog plus the models in `path` (a JSON array of models), if it exists.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut registry = Self::builtin();
        if path.exists() {
            let text = std::fs::read_to_string(path)?;
            let user: Vec<Model> = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("invalid model file {}: {e}", path.display()))?;
            for model in user {
                registry.add(model);
            }
        }
        Ok(registry)
    }

    pub fn add(&mut self, model: Model) {
        self.models.retain(|m| !(m.provider == model.provider && m.id == model.id));
        self.models.push(model);
    }

    pub fn all(&self) -> &[Model] {
        &self.models
    }

    /// Resolve `provider/id`, or a bare `id` (preferring first-party providers).
    pub fn find(&self, reference: &str) -> Option<&Model> {
        if let Some((provider, id)) = reference.split_once('/')
            && let Some(model) = self.models.iter().find(|m| m.provider == provider && m.id == id)
        {
            return Some(model);
        }
        const PREFERRED: [&str; 3] = ["anthropic", "openai", "deepseek"];
        let mut matches: Vec<&Model> = self.models.iter().filter(|m| m.id == reference).collect();
        matches.sort_by_key(|m| PREFERRED.iter().position(|p| *p == m.provider).unwrap_or(PREFERRED.len()));
        matches.into_iter().next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reasoning(map: &[(&str, Option<&str>)]) -> Model {
        let mut model = crate::providers::faux::faux_model();
        model.reasoning = true;
        model.thinking_level_map = map.iter().map(|(k, v)| (k.to_string(), v.map(str::to_string))).collect();
        model
    }

    #[test]
    fn thinking_levels_follow_the_catalog_as_in_pi() {
        use ThinkingLevel::*;
        assert_eq!(crate::providers::faux::faux_model().supported_thinking_levels(), [Off]);
        let plain = reasoning(&[]);
        assert_eq!(plain.supported_thinking_levels(), [Off, Minimal, Low, Medium, High]);
        assert_eq!(plain.clamp_thinking(Max), High, "xhigh and max need a mapping");
        assert_eq!(plain.native_effort(Xhigh).as_deref(), Some("high"));

        let always_on = reasoning(&[("off", None), ("minimal", None), ("xhigh", Some("xhigh"))]);
        assert_eq!(always_on.supported_thinking_levels(), [Low, Medium, High, Xhigh]);
        assert_eq!(always_on.clamp_thinking(Off), Low, "the next higher level first");
        assert_eq!(always_on.clamp_thinking(Max), Xhigh, "then the next lower");
    }

    #[test]
    fn builtin_catalog_loads_and_resolves() {
        let registry = ModelRegistry::builtin();
        assert!(registry.all().len() > 100);
        let model = registry.find("anthropic/claude-sonnet-5-5").expect("sonnet 5.5 in catalog");
        assert_eq!(model.api, Api::AnthropicMessages);
        assert_eq!(registry.find("claude-sonnet-5-5").unwrap().provider, "anthropic");
    }

    #[test]
    fn native_effort_skips_unsupported_levels() {
        let mut model = ModelRegistry::builtin().find("anthropic/claude-sonnet-5-5").unwrap().clone();
        // The catalog marks "off" unsupported (the model always reasons), so off clamps up, as in Pi.
        assert_eq!(model.native_effort(ThinkingLevel::Off).as_deref(), Some("low"));
        assert_eq!(model.native_effort(ThinkingLevel::High).as_deref(), Some("high"));
        // "minimal" maps to null in the catalog, so the next supported level is used.
        assert_eq!(model.native_effort(ThinkingLevel::Minimal).as_deref(), Some("low"));
        model.reasoning = false;
        assert_eq!(model.native_effort(ThinkingLevel::High), None);
    }
}
