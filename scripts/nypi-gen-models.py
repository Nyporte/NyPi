#!/usr/bin/env python3
"""Build crates/nypi-ai/data/models.json from Pi's generated model catalog.

Usage:
  (cd ../Pi/packages/ai && npm run generate-model-catalog)   # writes ../Pi/.artifacts/model-catalog/models.json
  python3 scripts/nypi-gen-models.py

Pi is the sibling checkout ../Pi; set PI_DIR to use another one.

Keeps only models whose wire API NyPi implements: anthropic-messages, openai-completions,
openai-responses, google-generative-ai, and bedrock-converse-stream.
"""

import json
import os
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
PI_DIR = pathlib.Path(os.environ.get("PI_DIR", ROOT.parent / "Pi"))
SRC = PI_DIR / ".artifacts/model-catalog/models.json"
DST = ROOT / "crates/nypi-ai/data/models.json"

SUPPORTED_APIS = {
    "anthropic-messages",
    "openai-completions",
    "openai-responses",
    "google-generative-ai",
    "bedrock-converse-stream",
}
# Providers that need OAuth or account-specific URL templating NyPi does not implement yet.
SKIP_PROVIDERS = {"github-copilot", "cloudflare-workers-ai", "cloudflare-ai-gateway", "radius"}
COMPAT_KEYS = {
    "forceAdaptiveThinking",
    "supportsMidConvoSystemMessages",
    "supportsTemperature",
    "thinkingFormat",
    "maxTokensField",
    "supportsDeveloperRole",
    "supportsReasoningEffort",
    "requiresReasoningContentOnAssistantMessages",
    "supportsUsageInStreaming",
    "supportsStrictMode",
}


def convert(model: dict) -> dict | None:
    api = model["api"]
    provider = model["provider"]
    if provider in SKIP_PROVIDERS:
        return None
    compat = {k: v for k, v in (model.get("compat") or {}).items() if k in COMPAT_KEYS}
    if api not in SUPPORTED_APIS:
        return None
    out = {
        "id": model["id"],
        "name": model["name"],
        "api": api,
        "provider": provider,
        "baseUrl": model["baseUrl"],
        "reasoning": bool(model.get("reasoning")),
        "input": model.get("input", ["text"]),
        "cost": model["cost"],
        "contextWindow": model["contextWindow"],
        "maxTokens": model["maxTokens"],
    }
    if model.get("thinkingLevelMap"):
        out["thinkingLevelMap"] = model["thinkingLevelMap"]
    if compat:
        out["compat"] = compat
    if model.get("headers"):
        out["headers"] = model["headers"]
    return out


def main() -> None:
    catalog = json.loads(SRC.read_text())
    models = []
    for provider_models in catalog.values():
        for model in provider_models.values():
            converted = convert(model)
            if converted:
                models.append(converted)
    models.sort(key=lambda m: (m["provider"], m["id"]))
    DST.parent.mkdir(parents=True, exist_ok=True)
    DST.write_text(json.dumps(models, separators=(",", ":")) + "\n")
    print(f"wrote {len(models)} models to {DST.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
