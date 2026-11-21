//! `nypi trace`: show recent pulses, or the causal chain and effects of one, read from the audit log.

use nypi_core::config::Paths;
use nypi_core::trace;
use serde_json::Value;

pub fn run(paths: &Paths, id: Option<&str>, days: usize) -> anyhow::Result<i32> {
    let audit = paths.audit();
    let Some(id) = id else {
        let pulses = trace::recent(&audit, days, 30);
        if pulses.is_empty() {
            nypi_hub::outln!("No pulses in the audit log of the last {days} days ({}).", audit.display());
        }
        for record in pulses.iter().rev() {
            nypi_hub::outln!("{}", pulse_line(record));
        }
        return Ok(0);
    };
    let Some(found) = trace::find(&audit, id, days) else {
        nypi_hub::errln!("No pulse {id} in the audit log of the last {days} days. Try a larger --days.");
        return Ok(1);
    };
    nypi_hub::outln!("Causal chain:");
    for (depth, record) in found.chain.iter().enumerate() {
        nypi_hub::outln!("{}{}", "  ".repeat(depth + 1), pulse_line(record));
    }
    if !found.children.is_empty() {
        nypi_hub::outln!("\nCaused pulses:");
        for record in &found.children {
            nypi_hub::outln!("  {}", pulse_line(record));
        }
    }
    let effects: Vec<String> = found.effects.iter().filter_map(effect_line).collect();
    if !effects.is_empty() {
        nypi_hub::outln!("\nRecorded while it was the cause:");
        for line in effects {
            nypi_hub::outln!("  {line}");
        }
    }
    Ok(0)
}

fn pulse_line(record: &Value) -> String {
    let pulse = &record["payload"];
    let source = match pulse["source"]["type"].as_str().unwrap_or("?") {
        "userInput" | "userTyping" => format!("user/{}", pulse["source"]["channel"].as_str().unwrap_or("?")),
        "module" => pulse["source"]["module"].as_str().unwrap_or("?").to_string(),
        other => other.to_string(),
    };
    let payload = &pulse["payload"];
    let kind = payload["kind"].as_str().unwrap_or("?");
    let detail = match kind {
        "userMessage" | "delegated" => {
            format!("({}) {}", payload["delivery"].as_str().unwrap_or("?"), excerpt(&payload["text"]))
        }
        "reply" => excerpt(&payload["text"]),
        "toolStarted" => format!("{} {}", str_of(&payload["tool"]), str_of(&payload["callId"])),
        "toolFinished" => format!(
            "{} {}{}",
            str_of(&payload["tool"]),
            if payload["isError"] == true { "failed" } else { "ok" },
            payload["durationMs"].as_u64().map(|ms| format!(" in {ms} ms")).unwrap_or_default()
        ),
        "runSettled" => format!("{}: {}", str_of(&payload["status"]["kind"]), excerpt(&payload["status"]["text"])),
        "alert" => format!("{}: {}", str_of(&payload["alert"]["level"]), excerpt(&payload["alert"]["message"])),
        _ => excerpt(&Value::String(payload.to_string())),
    };
    // Which conversation of the hub it was in: its folder's name. The hub's own pulses have none.
    let place = pulse["thread"].as_str().map(thread_name).map(|name| format!("  [{name}]")).unwrap_or_default();
    format!("{}  {}{place}  {source}  {kind}  {detail}", str_of(&pulse["id"]), time(&pulse["at"]))
}

/// A thread's name as a person reads it: the folder's own name, with a job's origin kept in front.
fn thread_name(thread: &str) -> String {
    let (origin, path) = match thread.split_once(':') {
        Some((origin, path)) if !origin.contains('/') => (Some(origin), path),
        _ => (None, thread),
    };
    let folder = std::path::Path::new(path).file_name().map_or(path.to_string(), |n| n.to_string_lossy().into_owned());
    match origin {
        Some(origin) => format!("{origin} in {folder}"),
        None => folder,
    }
}

/// Agent events reduced to messages and tool calls; streaming bookkeeping is left out.
fn effect_line(record: &Value) -> Option<String> {
    let payload = &record["payload"];
    let source = record["source"].as_str().unwrap_or("?");
    let detail = match source {
        "agent" | "chat" => match payload["type"].as_str()? {
            "message_end" => {
                let message = &payload["message"];
                let blocks = message["content"].as_array().cloned().unwrap_or_default();
                let text: Vec<&str> = blocks.iter().filter_map(|b| b["text"].as_str()).collect();
                let calls: Vec<&str> =
                    blocks.iter().filter(|b| b["type"] == "toolCall").filter_map(|b| b["name"].as_str()).collect();
                let mut line = excerpt(&Value::String(text.join(" ")));
                if !calls.is_empty() {
                    line = format!("{line} [calls {}]", calls.join(", ")).trim_start().to_string();
                }
                format!("{}: {line}", str_of(&message["role"]))
            }
            "tool_execution_start" => format!("tool {} started", str_of(&payload["tool_name"])),
            "tool_execution_end" => format!(
                "tool {} {}",
                str_of(&payload["tool_name"]),
                if payload["output"]["isError"] == true { "failed" } else { "finished" }
            ),
            _ => return None,
        },
        _ => excerpt(&Value::String(payload.to_string())),
    };
    Some(format!("{}  {source}  {detail}", time(&record["ts"])))
}

fn str_of(value: &Value) -> &str {
    value.as_str().unwrap_or("?")
}

/// Month, day, and time of an RFC 3339 timestamp, in local time.
fn time(value: &Value) -> String {
    value
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&chrono::Local).format("%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "?".into())
}

fn excerpt(value: &Value) -> String {
    let text = value.as_str().unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > 100 { format!("{}…", text.chars().take(100).collect::<String>()) } else { text }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pulse_line_names_the_conversation_it_was_in() {
        assert_eq!(thread_name("/Users/x/Projects/site"), "site");
        assert_eq!(thread_name("timer:/Users/x/Projects/site"), "timer in site");
        let pulse = |thread: Value| {
            serde_json::json!({ "payload": {
                "id": "ab-1", "at": "2026-11-18T08:47:06Z", "thread": thread,
                "source": { "type": "timer", "schedule": "s-1" },
                "payload": { "kind": "runSettled", "status": { "kind": "done", "text": "ok" } },
            }})
        };
        assert!(
            pulse_line(&pulse(serde_json::json!("/Users/x/site"))).contains("  [site]  timer  runSettled  done: ok")
        );
        assert!(!pulse_line(&pulse(Value::Null)).contains('['), "the hub's own pulse belongs to no conversation");
    }
}
