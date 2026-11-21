//! Layered system prompt: core (read-only), self (agent-editable), situational (project
//! context, skills, environment). See docs/architecture.md, "Prompt layers".

use std::path::{Path, PathBuf};

use crate::config::{Paths, Settings};
use crate::skills::{Skill, format_for_prompt};
use crate::tools::prompt_contribution;

const BUILTIN_CORE: &str = include_str!("../prompts/core.md");

/// Context file names, first match per directory wins (Pi's order).
const CONTEXT_FILE_NAMES: [&str; 3] = ["AGENTS.override.md", "AGENTS.md", "CLAUDE.md"];

#[derive(Clone, Debug)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

/// Global `~/.nypi/AGENTS.md`, then one file per directory from the filesystem root down to `cwd`.
pub fn load_context_files(paths: &Paths, cwd: &Path) -> Vec<ContextFile> {
    let mut dirs: Vec<PathBuf> = cwd.ancestors().map(Path::to_path_buf).collect();
    dirs.reverse();
    dirs.insert(0, paths.home.clone());
    dirs.iter()
        .filter_map(|dir| {
            CONTEXT_FILE_NAMES.iter().find_map(|name| {
                let path = dir.join(name);
                let content = std::fs::read_to_string(&path).ok()?;
                Some(ContextFile { path, content })
            })
        })
        .collect()
}

pub struct PromptInputs<'a> {
    pub settings: &'a Settings,
    pub paths: &'a Paths,
    pub cwd: &'a Path,
    pub tools: &'a [String],
    pub context_files: &'a [ContextFile],
    pub skills: &'a [Skill],
    /// Reachable through `delegate`.
    pub specialists: &'a [crate::specialist::Specialist],
    /// What else the agent is working on (`crate::active_tasks`), for a conversation
    /// of the hub.
    pub active_tasks: Option<&'a str>,
    /// The hub's devices and what each offers, for a runtime that can reach them (decision D79).
    pub devices: Option<&'a str>,
    /// The device `cwd` is on and its operating system, when it is not the machine the agent
    /// runs on.
    pub device: Option<(&'a str, &'a str)>,
    /// The workspace of the task the agent is on, when it has one (decision D78).
    pub workspace: Option<&'a TaskWorkspace>,
    /// The plan of the goal the task serves, when it serves one (decision D85).
    pub project_plan: Option<&'a crate::campaign::ProjectPlan>,
}

/// A task's workspace in the vault, as it reads at this turn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskWorkspace {
    /// Relative to the vault, for example `10-workspaces/261119-atlas-deploy`.
    pub path: String,
    pub brief: String,
    pub findings: String,
}

/// How much of `findings.md` is shown on every turn.
pub const FINDINGS_SHOWN_BYTES: usize = 6 * 1024;

/// The pinned `<task_workspace>` section: the brief and the findings as they are now. It is
/// rebuilt on every turn (decision D47), so what the agent wrote down survives any compaction.
fn workspace_section(workspace: &TaskWorkspace) -> String {
    let mut findings = workspace.findings.trim().to_string();
    let mut note = String::new();
    if findings.len() > FINDINGS_SHOWN_BYTES {
        let mut cut = FINDINGS_SHOWN_BYTES;
        while !findings.is_char_boundary(cut) {
            cut -= 1;
        }
        findings.truncate(cut);
        note = format!(
            "\n[findings.md is over {} KB and is cut here. Move detail into other notes of the workspace and keep \
             findings.md to what must stay in view.]",
            FINDINGS_SHOWN_BYTES / 1024
        );
    }
    if findings.is_empty() {
        findings = "(nothing written yet)".into();
    }
    format!(
        "Your workspace for this task is {path}/ in your vault. brief.md is the contract you work to. findings.md is \
         your working memory: it is shown here on every turn and outlives compaction, so write to it with vault_write \
         as you go. Before you end Done or Blocked, write {path}/report.md (Contract Check; Key Findings & \
         Deliverables; Falsified Paths & Residual Risks, each claim with its [tN]).\n\n## brief.md\n{brief}\n\n## findings.md\n{findings}{note}",
        path = workspace.path,
        brief = workspace.brief.trim(),
    )
}

/// How the portrait is meant to be used; shared with chat's prompt.
pub const PORTRAIT_INTRO: &str = "Who the user is, from their own words (category, times shown). Let it shape how you work \
with them; do not recite it or mention that you keep it.";

/// The system prompt as named sections, in order (decision D40): a changed section can go to
/// the model as an update instead of a new prompt.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PromptParts {
    pub sections: Vec<(String, String)>,
}

impl PromptParts {
    pub fn render(&self) -> String {
        self.sections.iter().map(|(_, text)| text.as_str()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n\n")
    }
}

/// Render the full system prompt.
pub fn build_system_prompt(inputs: &PromptInputs) -> String {
    build_prompt_parts(inputs).render()
}

pub fn build_prompt_parts(inputs: &PromptInputs) -> PromptParts {
    let settings = inputs.settings;
    let mut sections: Vec<(String, String)> = Vec::new();

    // Core layer: a user-maintained override file replaces the built-in text.
    let core = std::fs::read_to_string(inputs.paths.core_prompt_dir().join("core.md"))
        .unwrap_or_else(|_| BUILTIN_CORE.to_string());
    let user_ref = match &settings.user_name {
        Some(name) => format!("{name}'s"),
        None => "the user's".to_string(),
    };
    sections.push((
        "core".to_string(),
        core.replace("{{agent_name}}", &settings.agent_name).replace("{{user_ref}}", &user_ref).trim().to_string(),
    ));

    let mut tool_lines = Vec::new();
    let mut rules: Vec<String> = Vec::new();
    for name in inputs.tools {
        if let Some((snippet, guidelines)) = prompt_contribution(name) {
            tool_lines.push(format!("- {name}: {snippet}"));
            rules.extend(guidelines.iter().map(|g| g.to_string()));
        } else if let Some(command) = settings.command_tools.get(name) {
            let summary = command.description.lines().next().unwrap_or_default();
            tool_lines
                .push(format!("- {name}: {summary} (runs `{}`)", command.command.first().cloned().unwrap_or_default()));
        }
    }
    let mcp_count = inputs.tools.iter().filter(|t| t.starts_with("mcp__")).count();
    if mcp_count > 0 {
        tool_lines.push(format!("- mcp__<server>__<tool>: {mcp_count} tools from connected MCP servers"));
    }
    let has = |n: &str| inputs.tools.iter().any(|t| t == n);
    if has("bash") && !has("grep") && !has("find") && !has("ls") {
        rules.insert(0, "Use bash for file operations like ls, rg, find".into());
    }
    rules.push("Show file paths clearly when working with files".into());
    rules.push(
        "When the request leaves a convention open (rounding, time zones, inclusive ranges, units, what counts) and \
         the choice changes the result, say which one you used and give the result under the other too"
            .into(),
    );
    if !tool_lines.is_empty() {
        rules.push(
            "Each tool result starts with a reference such as [t3]. Cite the results your claims rest on with those \
             references, inline (for example: the tests pass [t3]); cite files as path:line and web pages by URL. The \
             harness checks every reference and marks the ones that match nothing as unverified"
                .into(),
        );
    }
    if has("delegate") && !inputs.specialists.is_empty() {
        tool_lines.push("- delegate: Hand a self-contained task to a specialist and review its report".into());
        rules.push(
            "Professional work in a specialist's domain goes to that specialist through delegate, not to your own \
             general tools (conduct rule 1). You stay accountable: review the report and verify what you can before \
             reporting it. The exception is a change you already understand that is small: about 20 lines or fewer in \
             one or two files, with no design or investigation left. A delegation costs minutes, so make that change \
             yourself and verify it"
                .into(),
        );
    }
    if !tool_lines.is_empty() {
        sections.push(section("tools", &tool_lines.join("\n")));
    }
    if has("goals")
        && let Some(goals) = crate::goals::GoalsFile::new(inputs.paths.goals()).prompt_section()
    {
        sections.push(section(
            "goals",
            &format!(
                "The user's open goals (id, term, goal; next step). Keep advancing them; record progress with goals.\n{goals}"
            ),
        ));
    }
    if settings.portrait.mode != crate::portrait::PortraitMode::Off
        && let Some(portrait) = crate::portrait::Portrait::new(inputs.paths.portrait()).prompt_section()
    {
        sections.push(section("portrait", &format!("{PORTRAIT_INTRO}\n{portrait}")));
    }
    if has("delegate") && !inputs.specialists.is_empty() {
        let roster: Vec<String> = inputs
            .specialists
            .iter()
            .map(|s| {
                let kind = match s.profile.kind {
                    crate::specialist::SpecialistKind::Native => "native",
                    crate::specialist::SpecialistKind::External => "external CLI",
                };
                let domain =
                    if s.profile.domain.is_empty() { String::new() } else { format!(", {}", s.profile.domain) };
                format!("- {} ({kind}{domain}): {}", s.name, s.profile.description)
            })
            .collect();
        sections.push(section("specialists", &roster.join("\n")));
    }
    sections.push(section("rules", &rules.iter().map(|r| format!("- {r}")).collect::<Vec<_>>().join("\n")));

    // Self layer: maintained by the agent itself.
    let self_path = inputs.paths.self_prompt(&settings.agent_name);
    if let Ok(text) = std::fs::read_to_string(&self_path)
        && !text.trim().is_empty()
    {
        sections.push(section("self", text.trim()));
    }

    if let Some(extra) = settings.append_system_prompt.as_deref().filter(|s| !s.trim().is_empty()) {
        sections.push(section("addendum", extra.trim()));
    }
    if !inputs.context_files.is_empty() {
        let files: Vec<String> = inputs
            .context_files
            .iter()
            .map(|f| {
                format!(
                    "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                    f.path.display(),
                    f.content.trim()
                )
            })
            .collect();
        sections.push(section(
            "project_context",
            &format!("Project-specific instructions and guidelines:\n\n{}", files.join("\n\n")),
        ));
    }
    if let Some(plan) = inputs.project_plan {
        sections.push(section("project_plan", &crate::campaign::plan_section(plan)));
    }
    if let Some(workspace) = inputs.workspace {
        sections.push(section("task_workspace", &workspace_section(workspace)));
    }
    if let Some(active) = inputs.active_tasks.filter(|text| !text.trim().is_empty()) {
        sections.push(section("active_tasks", active.trim()));
    }
    if let Some(devices) = inputs.devices.filter(|text| !text.trim().is_empty()) {
        sections.push(section("devices", devices.trim()));
    }
    if has("read") || has("bash") {
        let skills = format_for_prompt(inputs.skills);
        if !skills.is_empty() {
            sections.push(section("skills", &skills));
        }
    }
    sections.push(section(
        "environment",
        &format!(
            "Working directory: {}{}\nPlatform: {}\nDate: {}",
            inputs.cwd.display(),
            // A conversation whose place is another device: the paths are that device's.
            inputs
                .device
                .map(|(device, _)| {
                    format!(
                        " on the device \"{device}\". Your file and shell tools run there, not on the machine \
                         you run on; every path is that device's"
                    )
                })
                .unwrap_or_default(),
            // The system the tools run on: a shell command must suit that one.
            match inputs.device {
                Some((_, system)) if !system.is_empty() => system.to_string(),
                Some(_) => "unknown".to_string(),
                None => format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            },
            chrono::Local::now().format("%Y-%m-%d")
        ),
    ));
    PromptParts { sections }
}

fn section(name: &str, content: &str) -> (String, String) {
    (name.to_string(), tagged(name, content))
}

fn tagged(name: &str, content: &str) -> String {
    format!("<{name}>\n{content}\n</{name}>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_has_identity_conduct_tools_and_context() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let settings = Settings { agent_name: "Ada".into(), ..Default::default() };
        let self_path = paths.self_prompt("Ada");
        std::fs::create_dir_all(self_path.parent().unwrap()).unwrap();
        std::fs::write(&self_path, "I prefer terse answers.").unwrap();
        let tools: Vec<String> = vec!["read".into(), "bash".into()];
        let files = vec![ContextFile { path: "/p/AGENTS.md".into(), content: "Use tabs.".into() }];
        let prompt = build_system_prompt(&PromptInputs {
            settings: &settings,
            paths: &paths,
            cwd: Path::new("/p"),
            tools: &tools,
            context_files: &files,
            skills: &[],
            specialists: &[],
            active_tasks: None,
            devices: None,
            device: None,
            workspace: None,
            project_plan: None,
        });
        assert!(prompt.starts_with("You are Ada, the user's chief of staff"));
        assert!(prompt.contains("<conduct>"));
        assert!(prompt.contains("<partnership>") && prompt.contains("Inquire before you escalate"));
        assert!(prompt.contains("- read: Read file contents"));
        assert!(prompt.contains("<self>\nI prefer terse answers.\n</self>"));
        assert!(prompt.contains("Use tabs."));
        assert!(prompt.contains("Working directory: /p"));
    }
}
