//! Lightweight JSON Schema checking for tool arguments: required fields and primitive types,
//! with the lossless coercions models commonly need ("5" -> 5, "true" -> true).

use serde_json::{Map, Value};

/// Validate `args` against `schema`, coercing in place. Returns a model-readable error.
pub fn validate_arguments(schema: &Value, args: &mut Value) -> Result<(), String> {
    let mut errors = Vec::new();
    check(schema, args, "", &mut errors);
    if errors.is_empty() { Ok(()) } else { Err(format!("Invalid arguments:\n- {}", errors.join("\n- "))) }
}

fn check(schema: &Value, value: &mut Value, path: &str, errors: &mut Vec<String>) {
    let label = if path.is_empty() { "arguments".to_string() } else { path.to_string() };
    let types = schema_types(schema);
    if !types.is_empty() && !types.iter().any(|t| matches_type(t, value)) {
        if let Some(coerced) = types.iter().find_map(|t| coerce(t, value)) {
            *value = coerced;
        } else {
            errors.push(format!("{label}: expected {}, got {}", types.join(" or "), describe(value)));
            return;
        }
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        errors.push(format!("{label}: must be one of {}", Value::Array(allowed.clone())));
    }
    match value {
        Value::Object(object) => check_object(schema, object, path, errors),
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (i, item) in items.iter_mut().enumerate() {
                    check(item_schema, item, &format!("{label}[{i}]"), errors);
                }
            }
        }
        _ => {}
    }
}

fn check_object(schema: &Value, object: &mut Map<String, Value>, path: &str, errors: &mut Vec<String>) {
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(name) {
                errors.push(format!("{}: missing required property", join(path, name)));
            }
        }
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, property_schema) in properties {
            if let Some(value) = object.get_mut(name) {
                // Models often send null for omitted optional fields.
                if value.is_null() && !schema_types(property_schema).contains(&"null") {
                    object.remove(name);
                    continue;
                }
                check(property_schema, value, &join(path, name), errors);
            }
        }
    }
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() { name.to_string() } else { format!("{path}.{name}") }
}

fn schema_types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    }
}

fn matches_type(expected: &str, value: &Value) -> bool {
    match expected {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|f| f.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn coerce(expected: &str, value: &Value) -> Option<Value> {
    let text = value.as_str()?.trim();
    match expected {
        "integer" => text.parse::<i64>().ok().map(Value::from),
        "number" => text.parse::<f64>().ok().and_then(|f| serde_json::Number::from_f64(f).map(Value::Number)),
        "boolean" => match text {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        // Some models send arrays and objects as JSON-encoded strings.
        "array" | "object" => serde_json::from_str::<Value>(text).ok().filter(|v| matches_type(expected, v)),
        _ => None,
    }
}

fn describe(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" },
                "limit": { "type": "integer" },
                "edits": { "type": "array", "items": { "type": "object", "required": ["oldText"], "properties": { "oldText": { "type": "string" } } } }
            }
        })
    }

    #[test]
    fn coerces_numeric_strings_and_drops_null_optionals() {
        let mut args = json!({ "path": "a", "limit": "10", "edits": null });
        validate_arguments(&schema(), &mut args).unwrap();
        assert_eq!(args, json!({ "path": "a", "limit": 10 }));
    }

    #[test]
    fn reports_missing_and_nested_errors() {
        let mut args = json!({ "edits": "[{\"x\":1}]" });
        let error = validate_arguments(&schema(), &mut args).unwrap_err();
        assert!(error.contains("path: missing required property"), "{error}");
        assert!(error.contains("edits[0].oldText: missing required property"), "{error}");
    }
}
