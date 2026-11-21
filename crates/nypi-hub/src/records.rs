//! What a record of a thread means to a person: the lines an interface or a channel shows for
//! it. Shared by the terminal, `nypi send`, and the Telegram channel.

use nypi_ai::Message;
use nypi_core::inspector::call_summary;
use serde_json::Value;

/// What a record means to a person watching: None for records that are only machinery.
/// `replies` shows each assistant message as it ends (a conversation); without it only the
/// final answer is shown, by the caller.
pub fn show(record: &Value, replies: bool) -> Option<String> {
    match record["type"].as_str()? {
        "tool_execution_start" => {
            let tool = record["tool_name"].as_str().unwrap_or("?");
            let summary: String = call_summary(tool, &record["arguments"]).chars().take(120).collect();
            Some(format!("· {tool} {summary}"))
        }
        "tool_execution_end" if record["output"]["isError"] == true => {
            Some(format!("· {} failed", record["tool_name"].as_str().unwrap_or("?")))
        }
        "message_end" if replies => match serde_json::from_value::<Message>(record["message"].clone()) {
            Ok(Message::Assistant(reply)) if !reply.text().trim().is_empty() => Some(reply.text().trim().to_string()),
            _ => None,
        },
        "approval_request" => Some(format!(
            "[approval needed · {}] {}: {}{}",
            record["id"].as_str().unwrap_or("?"),
            record["tool"].as_str().unwrap_or("?"),
            record["summary"].as_str().unwrap_or_default(),
            record["reason"].as_str().map(|why| format!(" ({why})")).unwrap_or_default()
        )),
        "alert" => Some(format!(
            "[{}] {}",
            record["level"].as_str().unwrap_or("alert"),
            record["message"].as_str().unwrap_or_default()
        )),
        "auto_retry_start" => Some("[retrying after a provider error]".into()),
        "compaction_start" => Some("[summarizing older context]".into()),
        "reflection" => Some(format!("[{}]", record["summary"].as_str().unwrap_or_default())),
        "sourced_prompt" => Some(match record["origin"].as_str() {
            Some("report") => "[a specialist's report came back; Lucia is reviewing it]".to_string(),
            Some("timer") => "[a scheduled task is starting]".to_string(),
            Some("goals") => "[a goal review is starting]".to_string(),
            Some(origin) => format!("[a task from {origin} is starting]"),
            None => "[a task nobody typed is starting]".to_string(),
        }),
        "daemon_lagged" => Some(format!("[{} record(s) went by too fast to show]", record["missed"])),
        _ => None,
    }
}

/// How a settled run ended, when the reply itself does not say (an error, or a status the
/// harness had to derive).
pub fn settled_note(record: &Value) -> Option<String> {
    if let Some(error) = record["error"].as_str() {
        return Some(format!("error: {error}"));
    }
    (record["status"]["synthesized"] == true).then(|| {
        format!(
            "[status: {}: {}]",
            record["status"]["kind"].as_str().unwrap_or("done"),
            record["status"]["text"].as_str().unwrap_or_default()
        )
    })
}

/// One-line description of a tool call for display.
pub fn tool_summary(tool: &str, args: &Value) -> String {
    let field = |k: &str| args[k].as_str().unwrap_or_default().to_string();
    let summary = match tool {
        "bash" => field("command"),
        "read" | "write" | "ls" => field("path"),
        "edit" => format!("{} ({} edit(s))", field("path"), args["edits"].as_array().map_or(1, Vec::len)),
        "grep" => format!("/{}/ {}", field("pattern"), field("path")),
        "find" => format!("{} {}", field("pattern"), field("path")),
        _ => args.to_string(),
    };
    let one_line = summary.replace('\n', " ⏎ ");
    if one_line.chars().count() > 200 {
        format!("{}…", one_line.chars().take(200).collect::<String>())
    } else {
        one_line
    }
}
