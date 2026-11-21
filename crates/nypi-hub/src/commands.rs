//! Slash commands shared by interactive and Telegram front ends.

use std::path::Path;

use nypi_ai::{ModelRegistry, ThinkingLevel};
use nypi_core::config::Paths;
use nypi_core::runtime::Runtime;
use nypi_core::session::SessionStore;

pub enum CommandResult {
    Handled,
    Quit,
    /// The command expands into a prompt (for example `/skill:name`).
    Prompt(String),
    /// A `!` command ran in the runtime; what it adds to model context, when anything. A front
    /// end whose user talks to another agent (chat mode) hands that agent this message too.
    Ran(Option<Box<nypi_ai::Message>>),
}

pub const HELP: &str = "Commands:
  /model [provider/id]   show or switch the model
  /models [filter]       list models
  /thinking [level]      show or set thinking (off minimal low medium high xhigh max)
  /new                   start a new session
  /compact               summarize older context now
  /session               show session info
  /sessions              list recent sessions here
  /export [file.html]    write this session as a static HTML page
  /rate <verdict> [note] say how the latest result was: g good, f fixed (you had to push or
                         correct), w wrong, s silent (it stopped without telling you)
  !command               run a shell command; its output goes to the model with your next message
  !!command              run a shell command without showing its output to the model
  /skills                list skills
  /skill:<name> [args]   run with a skill's instructions
  /prompts               list MCP prompt templates
  /mcp:<server>:<prompt> [args]  run an MCP prompt (args as key=value)
  /reload                reload prompt, context files and skills
  /quit                  exit
While a run is active: type to steer, /followup <text> to queue, /abort to stop.";

/// Run a slash command, or a `!` shell command, and return text for the user plus what to do
/// next.
/// `/admit <code> [exec-only]` and `/refuse <code>` (decision D93): the user's answer to a join
/// request, from any place that shows one. None for any other line. It acts at once, whatever
/// runs: a request waits on a link, not on the conversation.
pub fn join_command(paths: &Paths, line: &str) -> Option<String> {
    use crate::pairing::{JoinAnswer, Trust};
    let mut words = line.split_whitespace();
    let verb = words.next()?;
    if verb != "/admit" && verb != "/refuse" {
        return None;
    }
    let rest: Vec<&str> = words.collect();
    let exec_only = rest.iter().any(|word| matches!(*word, "exec-only" | "--exec-only"));
    let code: String = rest.iter().filter(|word| !word.contains("exec-only")).copied().collect();
    if code.is_empty() {
        return Some(format!("{verb} needs the request's six-digit code (nypi devices pending lists them)"));
    }
    let answer = match (verb, exec_only) {
        ("/refuse", _) => JoinAnswer::Refuse,
        (_, true) => JoinAnswer::Admit { trust: Trust::ExecOnly },
        _ => JoinAnswer::Admit { trust: Trust::Personal },
    };
    Some(match crate::pairing::answer(paths, &code, answer) {
        Ok(request) => match answer {
            JoinAnswer::Admit { trust } => format!("Admitted {} as {}.", request.name, trust.label()),
            JoinAnswer::Refuse => format!("Refused {}.", request.name),
        },
        Err(why) => why,
    })
}

pub async fn run_command(runtime: &mut Runtime, line: &str) -> (String, CommandResult) {
    if let Some(command) = line.strip_prefix('!') {
        let (text, context) = user_bash(runtime, command).await;
        return (text, CommandResult::Ran(context.map(Box::new)));
    }
    let (name, arg) = match line.split_once(char::is_whitespace) {
        Some((name, arg)) => (name, arg.trim()),
        None => (line, ""),
    };
    let text = match name {
        "/help" => HELP.to_string(),
        "/quit" | "/exit" => return (String::new(), CommandResult::Quit),
        "/new" => match runtime.new_session() {
            Ok(()) => "Started a new session.".into(),
            Err(e) => format!("error: {e:#}"),
        },
        "/model" if arg.is_empty() => format!("Model: {}", runtime.model().reference()),
        "/model" => match runtime.set_model(arg) {
            Ok(()) => {
                let key = if runtime.has_api_key() { "" } else { " (warning: no API key found)" };
                format!("Model: {}{key}", runtime.model().reference())
            }
            Err(e) => format!("error: {e:#}"),
        },
        "/models" => models_text(&runtime.registry, (!arg.is_empty()).then_some(arg), 40),
        "/thinking" if arg.is_empty() => format!("Thinking: {}", runtime.agent.config.thinking.as_str()),
        "/thinking" => match ThinkingLevel::parse(arg) {
            Some(level) => match runtime.set_thinking(level) {
                Ok(effective) if effective != level => {
                    format!("Thinking: {} ({} is not available for this model)", effective.as_str(), level.as_str())
                }
                Ok(effective) => format!("Thinking: {}", effective.as_str()),
                Err(e) => format!("error: {e:#}"),
            },
            None => format!("Unknown thinking level: {arg}"),
        },
        "/compact" => {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            match runtime.compact("manual", &tx).await {
                Ok(()) => "Context compacted.".into(),
                Err(e) => format!("Not compacted: {e:#}"),
            }
        }
        "/session" => {
            let tokens = nypi_core::compaction::context_tokens(&runtime.agent.messages);
            format!(
                "Session {}\nFile: {}\nMessages: {}\nContext: ~{} / {} tokens",
                runtime.session_id(),
                runtime.session_path().display(),
                runtime.agent.messages.len(),
                tokens,
                runtime.model().context_window
            )
        }
        "/sessions" => sessions_text(&runtime.paths, &runtime.cwd, 10),
        "/export" => match runtime.export_html((!arg.is_empty()).then_some(arg)) {
            Ok(path) => format!("Exported to {}", path.display()),
            Err(e) => format!("Not exported: {e:#}"),
        },
        "/rate" => crate::rating::in_session(&runtime.paths, &runtime.session_id(), arg),
        "/skills" => {
            if runtime.skills.is_empty() {
                "No skills found.".into()
            } else {
                runtime.skills.iter().map(|s| format!("{}: {}", s.name, s.description)).collect::<Vec<_>>().join("\n")
            }
        }
        "/prompts" => {
            if runtime.mcp_prompts().is_empty() {
                "No MCP prompts. Servers that offer prompts list them here.".into()
            } else {
                runtime.mcp_prompts().iter().map(prompt_line).collect::<Vec<_>>().join("\n")
            }
        }
        _ if name.starts_with("/mcp:") => {
            let Some((server, prompt)) = name["/mcp:".len()..].split_once(':') else {
                return ("Use /mcp:<server>:<prompt> [args]".into(), CommandResult::Handled);
            };
            return match runtime.expand_mcp_prompt(server, prompt, arg).await {
                Ok(text) => (String::new(), CommandResult::Prompt(text)),
                Err(error) => (format!("{error:#}"), CommandResult::Handled),
            };
        }
        "/reload" => {
            runtime.refresh_system_prompt();
            "Reloaded.".into()
        }
        _ if name.starts_with("/skill:") => {
            let skill_name = &name["/skill:".len()..];
            return match runtime.skills.iter().find(|s| s.name == skill_name) {
                Some(skill) => match std::fs::read_to_string(&skill.file_path) {
                    Ok(body) => {
                        let mut prompt = format!(
                            "<skill name=\"{}\" location=\"{}\">\n{}\n</skill>",
                            skill.name,
                            skill.file_path.display(),
                            body.trim()
                        );
                        if !arg.is_empty() {
                            prompt.push_str(&format!("\n\n{arg}"));
                        }
                        (String::new(), CommandResult::Prompt(prompt))
                    }
                    Err(e) => (format!("Cannot read skill: {e}"), CommandResult::Handled),
                },
                None => (format!("Unknown skill: {skill_name}"), CommandResult::Handled),
            };
        }
        _ => format!("Unknown command: {name}. Type /help."),
    };
    (text, CommandResult::Handled)
}

/// One line describing an MCP prompt and its arguments.
pub fn prompt_line(prompt: &nypi_core::mcp::McpPrompt) -> String {
    let arguments: Vec<String> =
        prompt.arguments.iter().map(|a| if a.required { a.name.clone() } else { format!("[{}]", a.name) }).collect();
    format!("/mcp:{}:{} {}  {}", prompt.server, prompt.name, arguments.join(" "), prompt.description)
}

pub fn models_text(registry: &ModelRegistry, filter: Option<&str>, limit: usize) -> String {
    let filter = filter.map(str::to_lowercase);
    let rows: Vec<String> = registry
        .all()
        .iter()
        .filter(|m| {
            filter
                .as_ref()
                .is_none_or(|f| m.reference().to_lowercase().contains(f) || m.name.to_lowercase().contains(f))
        })
        .map(|m| {
            format!(
                "{:<50} ctx {:>8}  ${}/{} per M{}",
                m.reference(),
                m.context_window,
                m.cost.input,
                m.cost.output,
                if m.reasoning { "  reasoning" } else { "" }
            )
        })
        .collect();
    let total = rows.len();
    let mut text = rows.into_iter().take(limit).collect::<Vec<_>>().join("\n");
    if total > limit {
        text.push_str(&format!("\n… {} more; narrow with a filter", total - limit));
    }
    if text.is_empty() { "No models match.".into() } else { text }
}

pub fn print_models(registry: &ModelRegistry, filter: Option<&str>, limit: usize) {
    println!("{}", models_text(registry, filter, limit));
}

pub fn sessions_text(paths: &Paths, cwd: &Path, limit: usize) -> String {
    let sessions = SessionStore::list(&paths.sessions(), cwd);
    if sessions.is_empty() {
        return "No sessions for this directory.".into();
    }
    sessions
        .iter()
        .take(limit)
        .map(|s| {
            let when: chrono::DateTime<chrono::Local> = s.modified.into();
            let first: String = s.first_message.chars().take(60).collect::<String>().replace('\n', " ");
            format!(
                "{}  {:>4} msgs  {}\n    {}",
                when.format("%Y-%m-%d %H:%M"),
                s.message_count,
                first,
                s.path.display()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn print_sessions(paths: &Paths, cwd: &Path, limit: usize) {
    println!("{}", sessions_text(paths, cwd, limit));
}

/// The user's own shell command, as Pi's `!command`, or `!!command` to keep its output from the
/// model (decision D54). The agent's abort stops it.
async fn user_bash(runtime: &mut Runtime, command: &str) -> (String, Option<nypi_ai::Message>) {
    let (exclude, command) = match command.strip_prefix('!') {
        Some(rest) => (true, rest.trim()),
        None => (false, command.trim()),
    };
    if command.is_empty() {
        return ("Usage: !command, or !!command to keep the output from the model".into(), None);
    }
    let cancel = runtime.agent.new_abort_scope();
    match runtime.run_user_bash(command, exclude, cancel).await {
        Ok(run) => {
            let context = run.context_message();
            let mut text = if run.output.is_empty() { "(no output)".to_string() } else { run.output };
            if run.cancelled {
                text.push_str("\n[cancelled]");
            } else if let Some(code) = run.exit_code.filter(|c| *c != 0) {
                text.push_str(&format!("\n[exit code {code}]"));
            }
            if let Some(path) = run.full_output_path {
                text.push_str(&format!("\n[full output: {path}]"));
            }
            if exclude {
                text.push_str("\n[kept from the model]");
            }
            (text, context)
        }
        Err(error) => (format!("error: {error:#}"), None),
    }
}

#[cfg(test)]
mod tests {
    use nypi_ai::providers::faux::{FauxProvider, faux_model};
    use nypi_ai::{Message, text_of};
    use nypi_core::runtime::{RuntimeOptions, SessionMode};

    use super::*;

    #[tokio::test]
    async fn bang_commands_run_in_the_shell_and_double_bang_keeps_them_from_the_model() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut runtime = Runtime::new(RuntimeOptions {
            cwd: work.path().to_path_buf(),
            paths: Some(Paths { home: home.path().to_path_buf() }),
            model: Some("faux/faux-1".into()),
            extra_model: Some(faux_model()),
            stream_fn: Some(FauxProvider::new(vec![]).stream_fn()),
            session: SessionMode::Ephemeral,
            ..Default::default()
        })
        .unwrap();
        let (text, result) = run_command(&mut runtime, "!echo shown; exit 4").await;
        assert_eq!(text, "shown\n[exit code 4]");
        assert!(matches!(result, CommandResult::Ran(Some(_))));
        let (text, _) = run_command(&mut runtime, "!! echo private").await;
        assert_eq!(text, "private\n[kept from the model]");
        assert!(run_command(&mut runtime, "!").await.0.starts_with("Usage"));

        let users: Vec<String> = runtime
            .agent
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User(u) => Some(text_of(&u.content)),
                _ => None,
            })
            .collect();
        assert_eq!(users.len(), 1, "{users:?}");
        assert!(users[0].starts_with("Ran `echo shown; exit 4`"));
    }

    #[tokio::test]
    async fn rate_records_the_users_verdict_on_the_run_that_just_ended() {
        use nypi_ai::providers::faux::FauxResponse;
        use nypi_core::ledger::Verdict;

        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let faux = FauxProvider::new(vec![FauxResponse::text("Booked for 10:00.\n\nDone: the room is booked.")]);
        let mut runtime = Runtime::new(RuntimeOptions {
            cwd: work.path().to_path_buf(),
            paths: Some(Paths { home: home.path().to_path_buf() }),
            model: Some("faux/faux-1".into()),
            extra_model: Some(faux_model()),
            stream_fn: Some(faux.stream_fn()),
            session: SessionMode::Ephemeral,
            ..Default::default()
        })
        .unwrap();
        assert!(run_command(&mut runtime, "/rate g").await.0.starts_with("Nothing in this session has finished yet"));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let task = runtime.prompt("book the room", &tx).await.unwrap().task.unwrap();

        let (text, result) = run_command(&mut runtime, "/rate w it booked the wrong day").await;
        assert_eq!(text, format!("Rated {task} wrong: book the room"));
        assert!(matches!(result, CommandResult::Handled));
        let rating = runtime.ledger().get(&task).unwrap().rating.unwrap();
        assert_eq!((rating.verdict, rating.note.as_str()), (Verdict::Wrong, "it booked the wrong day"));
        assert_eq!(faux.requests().len(), 1, "a verdict is the user's; the model is not asked");
        assert!(HELP.contains("/rate <verdict>"));
    }
}
