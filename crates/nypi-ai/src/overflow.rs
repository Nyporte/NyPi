//! Whether a reply failed because the request outgrew the context window (Pi's
//! `utils/overflow.ts`). The patterns are Pi's, plus the `HTTP <status>:` prefix NyPi's
//! providers put on error bodies (provider.rs), which Pi's SDK-shaped patterns never match.

use std::sync::OnceLock;

use regex::Regex;

use crate::types::{AssistantMessage, StopReason};

/// Provider wording for a request over the context window or the request size limit.
const OVERFLOW: &[&str] = &[
    r"prompt (?:is )?too long",               // Anthropic, z.ai
    r"prompt exceeds max length",             // z.ai CN endpoint
    r"request_too_large",                     // Anthropic byte-size limit
    r"input is too long for requested model", // Amazon Bedrock
    r"exceeds the context window",            // OpenAI
    r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))", // LiteLLM and others
    r"context window exceeded",               // LiteLLM pre-call check
    r"input token count.*exceeds the maximum", // Google Gemini
    r"maximum prompt length is \d+",          // xAI
    r"reduce the length of the messages",     // Groq
    r"maximum context length is \d+ tokens",  // OpenRouter
    r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?", // OpenRouter/Poolside
    r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)", // Together AI
    r"exceeds the limit of \d+",              // GitHub Copilot
    r"exceeds the available context size",    // llama.cpp
    r"greater than the context length",       // LM Studio
    r"context window exceeds limit",          // MiniMax
    r"exceeded model token limit",            // Kimi For Coding
    r"too large for model with \d+ maximum context length", // Mistral
    r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?", // DS4
    r"model_context_window_exceeded",         // z.ai finish reason as error
    r"prompt too long; exceeded (?:max )?context length", // Ollama
    r"range of input length should be",       // DashScope / Qwen
    r"context[_ ]length[_ ]exceeded",
    r"too many tokens",
    r"token limit exceeded",
    // HTTP 413 is a request-size overflow whatever the body says: Pi's SDK prefixes, and NyPi's.
    r"^(?:[\w -]+ API error \(413\):|413(?:\s|:|$)|HTTP 413:)",
];

/// Errors that match an overflow pattern by accident, such as Bedrock's "Too many tokens, please
/// wait" throttling.
const NOT_OVERFLOW: &[&str] = &[r"^(?:Throttling error|Service unavailable):", r"rate limit", r"too many requests"];

/// Cerebras answers an oversized request with a 400 or 413 and no body.
const CEREBRAS_BODYLESS: &str = r"^(?:4(?:00|13)\s*(?:status code)?\s*\(no body\)|HTTP 4(?:00|13):\s*$)";

fn pattern(cell: &'static OnceLock<Regex>, alternatives: &[&str]) -> &'static Regex {
    cell.get_or_init(|| Regex::new(&format!("(?i){}", alternatives.join("|"))).expect("overflow patterns compile"))
}

/// Whether `message` shows a context overflow:
/// - an error whose text says so;
/// - a completed reply whose input exceeds `context_window` (providers that accept an oversized
///   request silently, such as z.ai);
/// - a length stop with no output and the window full (providers that truncate the input to fit,
///   such as Xiaomi MiMo).
///
/// Without a context window, only the error text is checked.
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    static YES: OnceLock<Regex> = OnceLock::new();
    static NOT: OnceLock<Regex> = OnceLock::new();
    static CEREBRAS: OnceLock<Regex> = OnceLock::new();
    if message.stop_reason == StopReason::Error
        && let Some(error) = message.error_message.as_deref().filter(|e| !e.is_empty())
        && !pattern(&NOT, NOT_OVERFLOW).is_match(error)
        && (pattern(&YES, OVERFLOW).is_match(error)
            || (message.provider == "cerebras" && pattern(&CEREBRAS, &[CEREBRAS_BODYLESS]).is_match(error)))
    {
        return true;
    }
    let Some(window) = context_window.filter(|w| *w > 0) else { return false };
    let input = message.usage.input + message.usage.cache_read;
    match message.stop_reason {
        StopReason::Stop => input > window,
        StopReason::Length => message.usage.output == 0 && input as f64 >= window as f64 * 0.99,
        _ => false,
    }
}

/// Whether a length stop ended below the output limit the request wanted: context pressure or
/// the provider cut it short, so one compact-and-retry may get the whole reply.
/// `desired_max_output` is the limit before any clamping to the context left.
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
    message.stop_reason == StopReason::Length && desired_max_output > 0 && message.usage.output < desired_max_output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(error: &str) -> AssistantMessage {
        AssistantMessage {
            stop_reason: StopReason::Error,
            error_message: Some(error.into()),
            ..AssistantMessage::empty("openai-completions", "gateway", "m")
        }
    }

    fn reply(stop_reason: StopReason, input: u64, output: u64) -> AssistantMessage {
        let mut message = AssistantMessage { stop_reason, ..AssistantMessage::empty("openai-completions", "z", "m") };
        message.usage.input = input;
        message.usage.output = output;
        message
    }

    #[test]
    fn provider_overflow_errors_are_recognized() {
        for error in [
            "HTTP 400: prompt is too long: 213462 tokens > 200000 maximum",
            "HTTP 400: Your input exceeds the context window of this model",
            "HTTP 400: Requested token count exceeds the model's maximum context length of 131072 tokens",
            "HTTP 400: The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)",
            "HTTP 400: This endpoint's maximum context length is 128000 tokens. However, you requested about 140000",
            "HTTP 413: {\"error\":\"payload\"}",
            "HTTP 400: context_length_exceeded",
        ] {
            assert!(is_context_overflow(&failed(error), None), "{error}");
        }
    }

    #[test]
    fn throttling_and_other_errors_are_not_overflow() {
        for error in [
            "Throttling error: Too many tokens, please wait before trying again.",
            "HTTP 429: rate limit reached: too many tokens per minute",
            "HTTP 400: invalid request",
            "HTTP 500: request id 413 failed",
        ] {
            assert!(!is_context_overflow(&failed(error), None), "{error}");
        }
        let mut cerebras = failed("HTTP 400: ");
        assert!(!is_context_overflow(&cerebras, None));
        cerebras.provider = "cerebras".into();
        assert!(is_context_overflow(&cerebras, None));
    }

    #[test]
    fn silent_and_truncating_overflow_need_the_window() {
        let silent = reply(StopReason::Stop, 140_000, 50);
        assert!(!is_context_overflow(&silent, None));
        assert!(is_context_overflow(&silent, Some(128_000)));
        assert!(!is_context_overflow(&reply(StopReason::Stop, 100_000, 50), Some(128_000)));

        assert!(is_context_overflow(&reply(StopReason::Length, 127_500, 0), Some(128_000)));
        assert!(!is_context_overflow(&reply(StopReason::Length, 127_500, 10), Some(128_000)));
    }

    #[test]
    fn a_length_stop_below_the_desired_limit_is_recoverable() {
        assert!(is_recoverable_length(&reply(StopReason::Length, 1000, 900), 4096));
        assert!(!is_recoverable_length(&reply(StopReason::Length, 1000, 4096), 4096));
        assert!(!is_recoverable_length(&reply(StopReason::Stop, 1000, 900), 4096));
        assert!(!is_recoverable_length(&reply(StopReason::Length, 1000, 900), 0));
    }
}
