//! Post-task reflection (principle A8): after a task that taught something (many tool calls, a
//! failed tool, a failed or blocked end, or the user correcting it mid-run), one tool-free model
//! call reviews the run. Durable lessons are appended to the agent's own self-prompt (versioned,
//! like `self_prompt`); skill changes are only proposed, written to the reflection journal in
//! `~/.nypi/reflections/` for the user to review. Most tasks teach nothing new, and the prompt
//! says so: an empty reflection is the common, correct result.

use std::path::{Path, PathBuf};

use anyhow::bail;
use nypi_ai::{Message, Model, StreamFn, text_of};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::config::Paths;
use crate::inspector::call_summary;
use crate::ledger::TaskId;
use crate::status::{RunStatus, StatusKind};

/// The self-prompt never grows past this through reflection; condensing it is the user's call.
const MAX_SELF_PROMPT_BYTES: usize = 16 * 1024;
const TRANSCRIPT_CHARS: usize = 24_000;
const RESULT_CHARS: usize = 300;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReflectionMode {
    /// Only after tasks that are likely to teach something.
    #[default]
    Auto,
    Always,
    Off,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReflectionSettings {
    pub mode: ReflectionMode,
    /// `auto` reflects after a task with at least this many tool calls.
    pub min_tool_calls: usize,
    /// Model for the reflection call as `provider/id`; defaults to the agent's.
    pub model: Option<String>,
}

impl Default for ReflectionSettings {
    fn default() -> Self {
        ReflectionSettings { mode: ReflectionMode::Auto, min_tool_calls: 5, model: None }
    }
}

const SYSTEM_PROMPT: &str = "You review a task you just finished, to get better at future ones. You write lessons, not \
summaries. Reply with JSON only.";

const INSTRUCTIONS: &str = "Write only lessons that will change how you work on future tasks: a mistake to avoid, a \
better method you found, a preference the user showed by correcting you, a quirk of a tool or system. Each lesson is \
one short imperative sentence that stands on its own. Leave out anything specific to this one task, anything already \
in your self-prompt, and anything you would do anyway. Usually there is nothing new: then return empty lists.\n\
Propose a skill change only when a skill would have made this task clearly better (a missing procedure, a wrong \
instruction); say which skill (existing or new), what to change, and why.\n\
Reply with this JSON and nothing else:\n\
{\"lessons\": [\"...\"], \"skill_proposals\": [{\"skill\": \"name\", \"proposal\": \"what to change and why\"}]}";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SkillProposal {
    pub skill: String,
    pub proposal: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Reflection {
    #[serde(default)]
    pub lessons: Vec<String>,
    #[serde(default)]
    pub skill_proposals: Vec<SkillProposal>,
}

/// What a finished run looked like, for the trigger and the prompt.
pub struct RunReview<'a> {
    pub task: &'a TaskId,
    pub goal: &'a str,
    pub status: &'a RunStatus,
    pub answer: &'a str,
    /// The messages the run added.
    pub messages: &'a [Message],
}

impl RunReview<'_> {
    fn tool_calls(&self) -> usize {
        self.messages.iter().filter(|m| matches!(m, Message::ToolResult(_))).count()
    }

    fn tool_failures(&self) -> usize {
        self.messages.iter().filter(|m| matches!(m, Message::ToolResult(r) if r.is_error)).count()
    }

    /// User messages after the prompt: steering, which usually means a correction.
    fn steering(&self) -> Vec<String> {
        self.messages
            .iter()
            .filter_map(|m| match m {
                Message::User(u) => Some(text_of(&u.content)),
                _ => None,
            })
            .skip(1)
            .collect()
    }
}

pub fn qualifies(settings: &ReflectionSettings, review: &RunReview) -> bool {
    match settings.mode {
        ReflectionMode::Off => false,
        ReflectionMode::Always => true,
        ReflectionMode::Auto => {
            review.tool_calls() >= settings.min_tool_calls.max(1)
                || review.tool_failures() > 0
                || matches!(review.status.kind, StatusKind::Failed | StatusKind::Blocked)
                || !review.steering().is_empty()
        }
    }
}

/// The reflection request: goal, outcome, corrections, a condensed transcript, and the current self-prompt.
pub fn build_prompt(review: &RunReview, self_prompt: &str) -> String {
    let mut steps = Vec::new();
    let mut calls = std::collections::HashMap::new();
    for message in review.messages {
        match message {
            Message::System(_) => {}
            Message::Assistant(assistant) => {
                let text = assistant.text();
                if !text.trim().is_empty() {
                    steps.push(format!("You: {}", clip(&text, RESULT_CHARS)));
                }
                for call in assistant.tool_calls() {
                    calls.insert(
                        call.id.clone(),
                        format!("{} {}", call.name, call_summary(&call.name, &call.arguments)),
                    );
                }
            }
            Message::ToolResult(result) => {
                let call = calls.get(&result.tool_call_id).cloned().unwrap_or_else(|| result.tool_name.clone());
                let mark = if result.is_error { "FAILED" } else { "ok" };
                steps.push(format!("Tool {call} -> {mark}: {}", clip(&text_of(&result.content), RESULT_CHARS)));
            }
            Message::User(user) => steps.push(format!("User: {}", clip(&text_of(&user.content), RESULT_CHARS))),
        }
    }
    let mut transcript = steps.join("\n");
    if transcript.len() > TRANSCRIPT_CHARS {
        // Keep the end: corrections and the outcome matter most.
        let cut = transcript.len() - TRANSCRIPT_CHARS;
        let start = (cut..transcript.len()).find(|i| transcript.is_char_boundary(*i)).unwrap_or(cut);
        transcript = format!("[earlier steps omitted]\n{}", &transcript[start..]);
    }
    let steering = review.steering();
    let corrections = if steering.is_empty() {
        "none".to_string()
    } else {
        steering.iter().map(|s| format!("- {}", clip(s, RESULT_CHARS))).collect::<Vec<_>>().join("\n")
    };
    let self_prompt = if self_prompt.trim().is_empty() { "(empty)" } else { self_prompt.trim() };
    format!(
        "<task id=\"{}\">\n{}\n</task>\n<outcome>{}</outcome>\n<user_corrections>\n{corrections}\n</user_corrections>\n\
         <steps>\n{transcript}\n</steps>\n<final_report>\n{}\n</final_report>\n<your_self_prompt>\n{self_prompt}\n\
         </your_self_prompt>\n\n{INSTRUCTIONS}",
        review.task,
        clip(review.goal, 2000),
        review.status,
        clip(review.answer, 3000),
    )
}

fn clip(text: &str, limit: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > limit { format!("{}…", line.chars().take(limit).collect::<String>()) } else { line }
}

/// Make the reflection call.
pub async fn reflect(
    model: &Model,
    stream_fn: &StreamFn,
    api_key: Option<String>,
    prompt: String,
    retry: &nypi_ai::retry::RetryPolicy,
    cancel: CancellationToken,
) -> anyhow::Result<Reflection> {
    match crate::oneshot::complete(
        model,
        stream_fn,
        api_key,
        SYSTEM_PROMPT,
        prompt,
        nypi_ai::ThinkingLevel::Off,
        retry,
        cancel,
    )
    .await
    {
        Ok(reply) => parse(&reply),
        Err(error) => bail!("Reflection failed: {error:#}"),
    }
}

/// The JSON object in a reply, fenced or not.
pub fn parse(reply: &str) -> anyhow::Result<Reflection> {
    let Some(object) = crate::oneshot::json_object(reply) else { bail!("Reflection reply has no JSON object") };
    let mut reflection: Reflection = serde_json::from_str(object)?;
    reflection.lessons.retain(|l| !l.trim().is_empty());
    reflection.skill_proposals.retain(|p| !p.proposal.trim().is_empty());
    Ok(reflection)
}

/// What applying a reflection did.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Applied {
    pub lessons_saved: usize,
    pub proposals: usize,
    pub journal: Option<PathBuf>,
    /// Set when lessons were not saved because the self-prompt is full.
    pub skipped: Option<String>,
}

impl Applied {
    pub fn summary(&self, task: &TaskId) -> Option<String> {
        if self.lessons_saved == 0 && self.proposals == 0 && self.skipped.is_none() {
            return None;
        }
        let mut parts = Vec::new();
        if self.lessons_saved > 0 {
            parts.push(format!("{} lesson(s) added to the self-prompt", self.lessons_saved));
        }
        if self.proposals > 0 {
            parts.push(format!("{} skill proposal(s) to review", self.proposals));
        }
        if let Some(skipped) = &self.skipped {
            parts.push(skipped.clone());
        }
        Some(format!("Reflection on task {task}: {} (nypi reflections)", parts.join("; ")))
    }
}

/// Save lessons to the self-prompt and everything to the journal.
pub async fn apply(
    paths: &Paths,
    agent_name: &str,
    task: &TaskId,
    goal: &str,
    reflection: &Reflection,
) -> anyhow::Result<Applied> {
    let mut applied = Applied::default();
    if reflection.lessons.is_empty() && reflection.skill_proposals.is_empty() {
        return Ok(applied);
    }
    let date = chrono::Local::now().format("%Y-%m-%d");
    if !reflection.lessons.is_empty() {
        let path = paths.self_prompt(agent_name);
        let lock = crate::tools::file_lock(&path);
        let _guard = lock.lock().await;
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        let mut addition = String::new();
        if !current.contains("## Lessons") {
            addition.push_str(if current.trim().is_empty() { "## Lessons\n" } else { "\n## Lessons\n" });
        }
        for lesson in &reflection.lessons {
            addition.push_str(&format!("- {} ({date}, task {task})\n", lesson.trim()));
        }
        let separator = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
        let updated = format!("{current}{separator}{addition}");
        if updated.len() > MAX_SELF_PROMPT_BYTES {
            applied.skipped = Some("lessons not saved: the self-prompt is full; condense it with self_prompt".into());
        } else {
            save_version(&path, &current, &format!("reflection on task {task}"))?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, updated)?;
            applied.lessons_saved = reflection.lessons.len();
        }
    }
    applied.proposals = reflection.skill_proposals.len();
    applied.journal = Some(write_journal(paths, agent_name, task, goal, reflection, &applied)?);
    Ok(applied)
}

fn save_version(path: &Path, previous: &str, reason: &str) -> anyhow::Result<()> {
    let dir = path.with_file_name("history");
    std::fs::create_dir_all(&dir)?;
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..6];
    let file = dir.join(format!("{}-{suffix}.md", chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ")));
    std::fs::write(
        file,
        format!("<!-- replaced at {} | reason: {reason} -->\n{previous}", chrono::Utc::now().to_rfc3339()),
    )?;
    Ok(())
}

fn write_journal(
    paths: &Paths,
    agent_name: &str,
    task: &TaskId,
    goal: &str,
    reflection: &Reflection,
    applied: &Applied,
) -> anyhow::Result<PathBuf> {
    let dir = reflections_dir(paths);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}-{task}.md", chrono::Local::now().format("%Y-%m-%d")));
    let mut text = format!("# Reflection on task {task} ({agent_name})\n\nGoal: {}\n", clip(goal, 400));
    if !reflection.lessons.is_empty() {
        let saved = if applied.lessons_saved > 0 { "added to the self-prompt" } else { "not saved" };
        text.push_str(&format!("\n## Lessons ({saved})\n"));
        for lesson in &reflection.lessons {
            text.push_str(&format!("- {}\n", lesson.trim()));
        }
    }
    if !reflection.skill_proposals.is_empty() {
        text.push_str("\n## Skill proposals (not applied; edit the skill if you agree)\n");
        for proposal in &reflection.skill_proposals {
            text.push_str(&format!("- **{}**: {}\n", proposal.skill.trim(), proposal.proposal.trim()));
        }
    }
    std::fs::write(&path, text)?;
    Ok(path)
}

pub fn reflections_dir(paths: &Paths) -> PathBuf {
    paths.home.join("reflections")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(kind: StatusKind) -> RunStatus {
        RunStatus { kind, text: "x".into(), synthesized: false }
    }

    #[test]
    fn triggers_and_parsing() {
        let task = TaskId("t-1".into());
        let quiet = [Message::user_text("hi")];
        let done = status(StatusKind::Done);
        let failed = status(StatusKind::Failed);
        let settings = ReflectionSettings::default();
        let r = RunReview { task: &task, goal: "g", status: &done, answer: "a", messages: &quiet };
        assert!(!qualifies(&settings, &r), "a plain chat teaches nothing");
        let r = RunReview { task: &task, goal: "g", status: &failed, answer: "a", messages: &quiet };
        assert!(qualifies(&settings, &r));
        let corrected = [Message::user_text("do x"), Message::user_text("no, use y")];
        let r = RunReview { task: &task, goal: "g", status: &done, answer: "a", messages: &corrected };
        assert!(qualifies(&settings, &r), "a correction is worth learning from");
        assert!(build_prompt(&r, "").contains("<user_corrections>\n- no, use y"));
        assert!(!qualifies(&ReflectionSettings { mode: ReflectionMode::Off, ..settings }, &r));

        let parsed = parse(
            "Sure:\n```json\n{\"lessons\": [\"Run tests before reporting.\", \" \"], \"skill_proposals\": []}\n```",
        )
        .unwrap();
        assert_eq!(parsed.lessons, ["Run tests before reporting."]);
        assert!(parse("no json").is_err());
    }

    #[tokio::test]
    async fn lessons_go_to_the_self_prompt_and_proposals_to_the_journal() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        std::fs::create_dir_all(paths.self_prompt("Lucia").parent().unwrap()).unwrap();
        std::fs::write(paths.self_prompt("Lucia"), "I prefer short answers.").unwrap();
        let task = TaskId("t-9".into());
        let reflection = Reflection {
            lessons: vec!["Check the build before claiming a fix.".into()],
            skill_proposals: vec![SkillProposal { skill: "rust-review".into(), proposal: "Add cargo clippy.".into() }],
        };
        let applied = apply(&paths, "Lucia", &task, "fix the build", &reflection).await.unwrap();
        assert_eq!((applied.lessons_saved, applied.proposals), (1, 1));
        let text = std::fs::read_to_string(paths.self_prompt("Lucia")).unwrap();
        assert!(
            text.starts_with("I prefer short answers.\n\n## Lessons\n- Check the build before claiming a fix. ("),
            "{text}"
        );
        assert!(text.trim_end().ends_with("task t-9)"));
        let journal = std::fs::read_to_string(applied.journal.as_ref().unwrap()).unwrap();
        assert!(journal.contains("**rust-review**: Add cargo clippy."));
        assert_eq!(std::fs::read_dir(paths.self_prompt("Lucia").with_file_name("history")).unwrap().count(), 1);
        assert!(applied.summary(&task).unwrap().contains("1 lesson(s) added"));
        let nothing = apply(&paths, "Lucia", &task, "g", &Reflection::default()).await.unwrap();
        assert!(nothing.summary(&task).is_none() && nothing.journal.is_none());
    }
}
