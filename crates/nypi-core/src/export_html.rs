//! A session as one self-contained, static HTML page (decision D53): the current branch, with
//! Markdown rendered and tool calls folded, and no script. Pi's export renders the page in the
//! browser with its own JavaScript; NyPi renders it here instead.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::bail;
use nypi_ai::{ContentBlock, Message, ToolDefinition, ToolResultMessage};
use serde_json::Value;

use crate::html::{escape, is_base64, is_image_type, markdown};
use crate::session::{Entry, SessionStore};

/// What the page shows besides the session file.
pub struct ExportContext<'a> {
    pub system_prompt: Option<&'a str>,
    pub tools: &'a [ToolDefinition],
}

/// Write the page for `session` to `output`, or beside the working directory as Pi names it
/// (`nypi-session-<file name>.html`). Returns where it went.
pub fn export(
    session: &SessionStore,
    context: &ExportContext,
    output: Option<&Path>,
    cwd: &Path,
) -> anyhow::Result<PathBuf> {
    if !session.is_persistent() {
        bail!("Cannot export in-memory session to HTML");
    }
    if !session.path.exists() {
        bail!("Nothing to export yet - start a conversation first");
    }
    let path = match output {
        Some(path) => cwd.join(path),
        None => {
            let stem = session.path.file_stem().unwrap_or_default().to_string_lossy();
            cwd.join(format!("nypi-session-{stem}.html"))
        }
    };
    std::fs::write(&path, render(session, context))?;
    Ok(path)
}

/// The page itself.
pub fn render(session: &SessionStore, context: &ExportContext) -> String {
    let title = session.session_name().unwrap_or_else(|| format!("Session {}", session.header.id));
    let mut page = String::new();
    let _ = writeln!(
        page,
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"UTF-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data: https: http:\">\n\
         <title>{}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n<main>\n<header>\n<h1>{}</h1>\n\
         <p class=\"meta\">{} · started {} · <code>{}</code></p>",
        escape(&title),
        escape(&title),
        escape(&session.header.id),
        escape(&session.header.timestamp),
        escape(&session.header.cwd),
    );
    if let Some(prompt) = context.system_prompt.filter(|p| !p.trim().is_empty()) {
        let _ = writeln!(
            page,
            "<details class=\"setup\"><summary>System prompt</summary><pre>{}</pre></details>",
            escape(prompt)
        );
    }
    if !context.tools.is_empty() {
        page.push_str("<details class=\"setup\"><summary>Tools</summary><ul>");
        for tool in context.tools {
            let _ = write!(page, "<li><b>{}</b>: {}</li>", escape(&tool.name), escape(&tool.description));
        }
        page.push_str("</ul></details>\n");
    }
    page.push_str("</header>\n");

    let branch = session.branch();
    let mut results: HashMap<&str, &ToolResultMessage> = HashMap::new();
    let mut edits: HashMap<&str, bool> = HashMap::new();
    for entry in &branch {
        match entry {
            Entry::Message { message: Message::ToolResult(result), .. } => {
                results.insert(result.tool_call_id.as_str(), result);
            }
            Entry::ContextEdit { target_id, replacement, .. } => {
                edits.insert(target_id.as_str(), replacement.is_some());
            }
            _ => {}
        }
    }
    let mut shown_results: Vec<&str> = Vec::new();
    for entry in &branch {
        let note = match edits.get(entry.id()) {
            Some(false) => Some("Left out of the model's context"),
            Some(true) => Some("Shown to the model with edited content"),
            None => None,
        };
        render_entry(&mut page, entry, note, &results, &mut shown_results);
    }
    page.push_str("</main>\n</body>\n</html>\n");
    page
}

fn render_entry<'a>(
    page: &mut String,
    entry: &'a Entry,
    note: Option<&str>,
    results: &HashMap<&'a str, &'a ToolResultMessage>,
    shown_results: &mut Vec<&'a str>,
) {
    let edited = |page: &mut String| {
        if let Some(note) = note {
            let _ = write!(page, "<p class=\"edit\">{note}</p>");
        }
    };
    let class = if note == Some("Left out of the model's context") { " omitted" } else { "" };
    match entry {
        Entry::Message { message: Message::User(user), .. } => {
            let _ = write!(page, "<section class=\"user{class}\"><h2>User</h2>");
            edited(page);
            render_blocks(page, &user.content);
            page.push_str("</section>\n");
        }
        Entry::Message { message: Message::Assistant(assistant), .. } => {
            let _ = write!(
                page,
                "<section class=\"assistant{class}\"><h2>Assistant <span>{}</span></h2>",
                escape(&assistant.model)
            );
            edited(page);
            for block in &assistant.content {
                match block {
                    ContentBlock::ToolCall { id, name, arguments, .. } => {
                        let summary = crate::inspector::call_summary(name, arguments);
                        let _ = write!(
                            page,
                            "<details class=\"tool\"><summary><b>{}</b> {}</summary><pre>{}</pre>",
                            escape(name),
                            escape(&summary),
                            escape(&serde_json::to_string_pretty(arguments).unwrap_or_default()),
                        );
                        if let Some(result) = results.get(id.as_str()) {
                            render_result(page, result);
                            shown_results.push(id.as_str());
                        }
                        page.push_str("</details>");
                    }
                    other => render_blocks(page, std::slice::from_ref(other)),
                }
            }
            if let Some(error) = &assistant.error_message {
                let _ = write!(page, "<p class=\"error\">{:?}: {}</p>", assistant.stop_reason, escape(error));
            }
            page.push_str("</section>\n");
        }
        Entry::Message { message: Message::ToolResult(result), .. } => {
            // Shown under its call; a result without one stands alone.
            if !shown_results.contains(&result.tool_call_id.as_str()) {
                let _ = write!(
                    page,
                    "<section class=\"tool-result{class}\"><details class=\"tool\"><summary><b>{}</b> result</summary>",
                    escape(&result.tool_name)
                );
                render_result(page, result);
                page.push_str("</details></section>\n");
            }
        }
        Entry::Message { message: Message::System(system), .. } => {
            let _ = writeln!(
                page,
                "<details class=\"setup\"><summary>System prompt update</summary><pre>{}</pre></details>",
                escape(&nypi_ai::transcript::render_update(system)),
            );
        }
        Entry::ModelChange { provider, model_id, .. } => {
            let _ = writeln!(page, "<p class=\"note\">Model: {}/{}</p>", escape(provider), escape(model_id));
        }
        Entry::ThinkingLevelChange { thinking_level, .. } => {
            let _ = writeln!(page, "<p class=\"note\">Thinking: {}</p>", escape(thinking_level));
        }
        Entry::Compaction { summary, tokens_before, .. } => {
            let _ = writeln!(
                page,
                "<section class=\"summary\"><h2>Compacted (was ~{tokens_before} tokens)</h2>{}</section>",
                markdown(summary)
            );
        }
        Entry::Other { raw, .. } => render_pi_entry(page, raw, class),
        Entry::ContextEdit { .. } | Entry::SessionInfo { .. } => {}
    }
}

/// Pi entries NyPi keeps as written: the user's shell commands, extension messages, and
/// branch summaries. Labels and other bookkeeping are not shown.
fn render_pi_entry(page: &mut String, raw: &Value, class: &str) {
    let message = &raw["message"];
    match (raw["type"].as_str(), message["role"].as_str()) {
        (Some("message"), Some("bashExecution")) => {
            let hidden = if message["excludeFromContext"] == true { " (kept out of context)" } else { "" };
            let _ = write!(
                page,
                "<section class=\"bash{class}\"><h2>User ran{hidden}</h2><pre>$ {}\n{}</pre>",
                escape(message["command"].as_str().unwrap_or_default()),
                escape(message["output"].as_str().unwrap_or_default()),
            );
            if let Some(code) = message["exitCode"].as_i64().filter(|c| *c != 0) {
                let _ = write!(page, "<p class=\"error\">Exit code {code}</p>");
            }
            page.push_str("</section>\n");
        }
        (Some("branch_summary"), _) | (Some("message"), Some("branchSummary")) => {
            let summary = raw["summary"].as_str().or(message["summary"].as_str()).unwrap_or_default();
            let _ = writeln!(page, "<section class=\"summary\"><h2>Branch summary</h2>{}</section>", markdown(summary));
        }
        (Some("custom_message"), _) | (Some("message"), Some("custom")) => {
            let content = if raw["type"] == "custom_message" { &raw["content"] } else { &message["content"] };
            let text = content.as_str().map(str::to_string).unwrap_or_else(|| {
                content
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            let _ = writeln!(page, "<section class=\"note{class}\">{}</section>", markdown(&text));
        }
        _ => {}
    }
}

fn render_result(page: &mut String, result: &ToolResultMessage) {
    let error = if result.is_error { " error" } else { "" };
    let _ = write!(page, "<div class=\"result{error}\">");
    for block in &result.content {
        match block {
            ContentBlock::Text { text, .. } => {
                let _ = write!(page, "<pre>{}</pre>", escape(text));
            }
            image @ ContentBlock::Image { .. } => render_blocks(page, std::slice::from_ref(image)),
            _ => {}
        }
    }
    page.push_str("</div>");
}

fn render_blocks(page: &mut String, blocks: &[ContentBlock]) {
    for block in blocks {
        match block {
            ContentBlock::Text { text, .. } => page.push_str(&markdown(text)),
            ContentBlock::Thinking { thinking, redacted, .. } if !redacted && !thinking.trim().is_empty() => {
                let _ = write!(
                    page,
                    "<details class=\"thinking\"><summary>Thinking</summary>{}</details>",
                    markdown(thinking)
                );
            }
            ContentBlock::Image { data, mime_type } if is_image_type(mime_type) && is_base64(data) => {
                let _ = write!(page, "<img alt=\"image\" src=\"data:{mime_type};base64,{data}\">");
            }
            ContentBlock::Image { .. } => page.push_str("<p class=\"note\">[image]</p>"),
            _ => {}
        }
    }
}

const STYLE: &str = "
:root { --bg: #fafaf9; --card: #ffffff; --text: #1c1917; --muted: #78716c; --line: #e7e5e4;
  --user: #eff6ff; --error: #b91c1c; --code: #f5f5f4; --link: #1d4ed8; }
@media (prefers-color-scheme: dark) {
  :root { --bg: #18181b; --card: #1f1f23; --text: #e7e5e4; --muted: #a8a29e; --line: #3f3f46;
    --user: #1e293b; --error: #f87171; --code: #27272a; --link: #93c5fd; }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--text);
  font: 15px/1.55 -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; }
main { max-width: 860px; margin: 0 auto; padding: 24px 16px 64px; }
h1 { font-size: 20px; margin: 0 0 4px; }
h2 { font-size: 12px; text-transform: uppercase; letter-spacing: .05em; color: var(--muted); margin: 0 0 8px; }
h2 span { text-transform: none; letter-spacing: 0; }
.meta, .note, .edit { color: var(--muted); font-size: 13px; }
h1, .meta { overflow-wrap: anywhere; }
a { color: var(--link); }
section { background: var(--card); border: 1px solid var(--line); border-radius: 8px; padding: 12px 16px; margin: 12px 0; }
section.user { background: var(--user); }
section.omitted { opacity: .55; }
pre, code { font: 13px/1.45 ui-monospace, SFMono-Regular, Menlo, monospace; }
pre { background: var(--code); padding: 10px; border-radius: 6px; overflow-x: auto; white-space: pre-wrap; word-break: break-word; }
details { margin: 8px 0; }
summary { cursor: pointer; color: var(--muted); }
details.tool summary b { color: var(--text); }
.result.error pre, .error { color: var(--error); }
img { max-width: 100%; border-radius: 6px; }
table { border-collapse: collapse; } td, th { border: 1px solid var(--line); padding: 4px 8px; }
";
