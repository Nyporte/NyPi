//! The goals file (principles B4 and B6): the user's short-, medium-, and long-term goals in
//! `~/.nypi/goals.md`, plain Markdown the user edits freely. One goal per line under its term:
//!
//! `- [ ] goal text · next: the next step · due: 2026-12-01 · dir: ~/Projects/x · vault: 10-projects/x <!-- g-1a2b3c -->`
//!
//! `vault` names the goal's project folder in Lucia's vault, whose `plan.md` is the campaign
//! a task for the goal sees (decision D85).
//!
//! The agent sees the open goals in its prompt and changes them only through the `goals` tool,
//! which edits single lines (everything else in the file is left as the user wrote it) and
//! saves the previous version with a reason, as `self_prompt` does.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use serde::Serialize;

/// Goals listed in the prompt; the file may hold more.
const PROMPT_GOALS: usize = 25;
const ATTRIBUTES: [&str; 7] = ["next", "due", "dir", "vault", "since", "done", "note"];
const SEPARATOR: &str = " · ";

pub const TEMPLATE: &str = "# Goals

What Lucia keeps working toward. Edit freely: one goal per line under its term.
Format: - [ ] goal · next: next step · due: YYYY-MM-DD · dir: project folder · vault: 10-projects/<name>

## Short term (days to weeks)

## Medium term (months)

## Long term (a year or more)
";

/// The prompt for a goal review run (`nypi goals review`, or the bridge on a schedule).
pub const REVIEW_PROMPT: &str = "Goal review. Look at the open goals in your prompt. Pick the one where progress matters \
most and whose next step you can take now without the user, and take that step: do the work, or delegate it when it is \
specialist work. Then update the goal with the goals tool: what moved, and the new next step (or mark it done). Advance \
one goal well rather than many a little. If no goal can move without the user, say exactly what you need from them \
(Blocked:). Report what you did, with evidence, and end with the status line.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Term {
    Short,
    Medium,
    Long,
}

impl Term {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.to_lowercase();
        if text.contains("short") || text.contains("短期") {
            Some(Term::Short)
        } else if text.contains("medium") || text.contains("mid") || text.contains("中期") {
            Some(Term::Medium)
        } else if text.contains("long") || text.contains("长期") {
            Some(Term::Long)
        } else {
            None
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Term::Short => "short",
            Term::Medium => "medium",
            Term::Long => "long",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Goal {
    pub id: String,
    pub term: Term,
    pub done: bool,
    pub text: String,
    /// `next`, `due`, `dir`, `since`, `done`, `note`, in file order.
    pub attributes: Vec<(String, String)>,
    /// Zero-based line in the file.
    #[serde(skip)]
    pub line: usize,
}

impl Goal {
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn render(&self) -> String {
        let mut line = format!("- [{}] {}", if self.done { "x" } else { " " }, self.text.trim());
        for (key, value) in &self.attributes {
            line.push_str(&format!("{SEPARATOR}{key}: {value}"));
        }
        format!("{line} <!-- {} -->", self.id)
    }

    fn set(&mut self, key: &str, value: Option<&str>) {
        self.attributes.retain(|(k, _)| k != key);
        if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
            self.attributes.push((key.to_string(), value.to_string()));
        }
    }
}

/// Parse every goal line under a short, medium, or long term heading.
pub fn parse(text: &str) -> Vec<Goal> {
    let mut goals = Vec::new();
    let mut term = None;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("## ") {
            term = Term::parse(heading);
            continue;
        }
        let Some(term) = term else { continue };
        let (done, rest) = if let Some(rest) = trimmed.strip_prefix("- [ ] ") {
            (false, rest)
        } else if let Some(rest) = trimmed.strip_prefix("- [x] ").or_else(|| trimmed.strip_prefix("- [X] ")) {
            (true, rest)
        } else {
            continue;
        };
        let (body, id) = match rest.rsplit_once("<!--") {
            Some((body, comment)) => {
                let id = comment.trim_end_matches("-->").trim();
                (body.trim(), id.starts_with("g-").then(|| id.to_string()))
            }
            None => (rest.trim(), None),
        };
        let mut parts = body.split(SEPARATOR);
        let mut text = parts.next().unwrap_or_default().trim().to_string();
        let mut attributes = Vec::new();
        for part in parts {
            match part.split_once(':') {
                Some((key, value)) if ATTRIBUTES.contains(&key.trim()) => {
                    attributes.push((key.trim().to_string(), value.trim().to_string()));
                }
                _ => text.push_str(&format!("{SEPARATOR}{part}")),
            }
        }
        // A goal the user wrote by hand has no id yet; it gets a stable one from its text.
        let id = id.unwrap_or_else(|| format!("g-{:06x}", fnv1a(&text) & 0xff_ffff));
        goals.push(Goal { id, term, done, text, attributes, line: index });
    }
    goals
}

pub struct GoalsFile {
    pub path: PathBuf,
}

/// One change to the goals file.
#[derive(Clone, Debug, Default)]
pub struct Change {
    pub text: Option<String>,
    pub next: Option<String>,
    pub due: Option<String>,
    pub dir: Option<String>,
    /// The project folder in the vault, `10-projects/<slug>`, checked by the caller.
    pub vault: Option<String>,
    pub note: Option<String>,
    pub done: Option<bool>,
}

impl GoalsFile {
    pub fn new(path: PathBuf) -> Self {
        GoalsFile { path }
    }

    pub fn read(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_default()
    }

    pub fn goals(&self) -> Vec<Goal> {
        parse(&self.read())
    }

    /// Add a goal; returns its id.
    pub fn add(&self, term: Term, text: &str, change: &Change, reason: &str) -> anyhow::Result<String> {
        let text = text.trim();
        if text.is_empty() {
            bail!("a goal needs text");
        }
        let current = self.read();
        let base = if current.trim().is_empty() { TEMPLATE.to_string() } else { current.clone() };
        let mut goal = Goal {
            id: format!("g-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]),
            term,
            done: false,
            text: text.to_string(),
            attributes: Vec::new(),
            line: 0,
        };
        goal.set("next", change.next.as_deref());
        goal.set("due", change.due.as_deref());
        goal.set("dir", change.dir.as_deref());
        goal.set("vault", change.vault.as_deref());
        goal.set("since", Some(&chrono::Local::now().format("%Y-%m-%d").to_string()));
        let mut lines: Vec<String> = base.lines().map(str::to_string).collect();
        let at = insertion_point(&lines, term).unwrap_or_else(|| {
            lines.push(String::new());
            lines.push(format!("## {} term", capitalized(term)));
            lines.len()
        });
        lines.insert(at, goal.render());
        self.save(&current, &lines, reason)?;
        Ok(goal.id)
    }

    /// Change one goal's line; returns the goal as it is now.
    pub fn update(&self, id: &str, change: &Change, reason: &str) -> anyhow::Result<Goal> {
        let current = self.read();
        let mut goal = parse(&current).into_iter().find(|g| g.id == id).ok_or_else(|| {
            anyhow!("No goal {id}. The open goals and their ids are in your prompt, or use action list.")
        })?;
        if let Some(text) = change.text.as_deref().filter(|t| !t.trim().is_empty()) {
            goal.text = text.trim().to_string();
        }
        if change.next.is_some() {
            goal.set("next", change.next.as_deref());
        }
        if change.due.is_some() {
            goal.set("due", change.due.as_deref());
        }
        if change.dir.is_some() {
            goal.set("dir", change.dir.as_deref());
        }
        if change.vault.is_some() {
            goal.set("vault", change.vault.as_deref());
        }
        if change.note.is_some() {
            goal.set("note", change.note.as_deref());
        }
        if let Some(done) = change.done {
            goal.done = done;
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            goal.set("done", done.then_some(today.as_str()));
            if done {
                goal.set("next", None);
            }
        }
        let mut lines: Vec<String> = current.lines().map(str::to_string).collect();
        lines[goal.line] = goal.render();
        self.save(&current, &lines, reason)?;
        Ok(goal)
    }

    fn save(&self, previous: &str, lines: &[String], reason: &str) -> anyhow::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if !previous.is_empty() {
            let history = self.history_dir();
            std::fs::create_dir_all(&history)?;
            let suffix = &uuid::Uuid::new_v4().simple().to_string()[..6];
            let name = format!("{}-{suffix}.md", chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ"));
            let header = format!(
                "<!-- replaced at {} | reason: {} -->\n",
                chrono::Utc::now().to_rfc3339(),
                reason.replace("-->", "")
            );
            std::fs::write(history.join(name), format!("{header}{previous}"))?;
        }
        let mut text = lines.join("\n");
        text.push('\n');
        std::fs::write(&self.path, text)?;
        Ok(())
    }

    pub fn history_dir(&self) -> PathBuf {
        self.path.with_file_name("goals-history")
    }

    /// The open goals for the system prompt, or None when there are none.
    pub fn prompt_section(&self) -> Option<String> {
        let open: Vec<Goal> = self.goals().into_iter().filter(|g| !g.done).collect();
        if open.is_empty() {
            return None;
        }
        let mut lines: Vec<String> = open
            .iter()
            .take(PROMPT_GOALS)
            .map(|g| {
                let mut line = format!("- {} [{}] {}", g.id, g.term.label(), g.text);
                for key in ["next", "due", "dir", "vault"] {
                    if let Some(value) = g.attribute(key) {
                        line.push_str(&format!("; {key}: {value}"));
                    }
                }
                line
            })
            .collect();
        if open.len() > PROMPT_GOALS {
            lines.push(format!("({} more in {})", open.len() - PROMPT_GOALS, self.path.display()));
        }
        Some(lines.join("\n"))
    }
}

impl GoalsFile {
    /// The goal `id`, open or done.
    pub fn find(&self, id: &str) -> Option<Goal> {
        self.goals().into_iter().find(|goal| goal.id == id)
    }
}

/// A goal's `vault` value as it may be written: one folder under `10-projects/`, inside the
/// vault at `vault_root` (`Vault::resolve` judges it). Empty clears it.
pub fn check_project(vault_root: &Path, value: &str) -> Result<String, String> {
    let value = value.trim().trim_end_matches('/');
    if value.is_empty() {
        return Ok(String::new());
    }
    let projects = nypi_vault::vault::PROJECTS;
    let slug = value.strip_prefix(&format!("{projects}/")).unwrap_or_default();
    if slug.is_empty() || slug.contains('/') || slug.starts_with('.') {
        return Err(format!("`vault` is a project folder in the vault: {projects}/<name>, one folder deep"));
    }
    let vault = nypi_vault::vault::Vault::open(vault_root).map_err(|error| error.to_string())?;
    vault.resolve(value).map_err(|error| error.to_string())?;
    Ok(value.to_string())
}

/// After the last goal of `term`'s section, or right after its heading.
fn insertion_point(lines: &[String], term: Term) -> Option<usize> {
    let heading =
        lines.iter().position(|l| l.trim().strip_prefix("## ").is_some_and(|h| Term::parse(h) == Some(term)))?;
    let mut at = heading + 1;
    let mut index = heading + 1;
    while index < lines.len() && !lines[index].trim().starts_with("## ") {
        if lines[index].trim().starts_with("- [") {
            at = index + 1;
        }
        index += 1;
    }
    // Under an empty section, leave one blank line after the heading.
    if at == heading + 1 && lines.get(at).is_some_and(|l| l.trim().is_empty()) {
        at += 1;
    }
    Some(at)
}

fn capitalized(term: Term) -> &'static str {
    match term {
        Term::Short => "Short",
        Term::Medium => "Medium",
        Term::Long => "Long",
    }
}

/// FNV-1a: a stable hash, so an id derived from a goal's text survives restarts.
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
}

/// Where the goals file lives for a NyPi home.
pub fn goals_path(home: &Path) -> PathBuf {
    home.join("goals.md")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hand_written_goals_and_edits_single_lines() {
        let dir = tempfile::tempdir().unwrap();
        let file = GoalsFile::new(dir.path().join("goals.md"));
        std::fs::write(
            &file.path,
            "# Goals\nMy own notes stay.\n\n## 短期\n- [ ] Ship NyPi M3 · next: commit goals\n\n## Long term\n- [x] Learn Rust · done: 2026-01-01\n",
        )
        .unwrap();
        let goals = file.goals();
        assert_eq!(goals.len(), 2);
        assert_eq!(
            (goals[0].term, goals[0].text.as_str(), goals[0].attribute("next")),
            (Term::Short, "Ship NyPi M3", Some("commit goals"))
        );
        assert!(goals[1].done);
        let id = goals[0].id.clone();
        assert_eq!(file.goals()[0].id, id, "ids of hand-written goals are stable");

        let added = file
            .add(
                Term::Medium,
                "Daily use of Lucia",
                &Change { next: Some("pair Telegram".into()), ..Default::default() },
                "user asked",
            )
            .unwrap();
        let updated = file
            .update(&id, &Change { next: Some("write the docs".into()), ..Default::default() }, "progress")
            .unwrap();
        assert_eq!(updated.attribute("next"), Some("write the docs"));
        file.update(&added, &Change { done: Some(true), ..Default::default() }, "finished").unwrap();

        let text = file.read();
        assert!(text.contains("My own notes stay."));
        assert!(text.contains(&format!("- [ ] Ship NyPi M3 · next: write the docs <!-- {id} -->")));
        assert!(text.contains("## Medium term"), "a missing section is added:\n{text}");
        let medium = file.goals().into_iter().find(|g| g.id == added).unwrap();
        assert!(medium.done && medium.attribute("next").is_none() && medium.attribute("done").is_some());
        assert_eq!(
            std::fs::read_dir(file.history_dir()).unwrap().count(),
            3,
            "every change keeps the previous version"
        );
        let section = file.prompt_section().unwrap();
        assert!(
            section.contains(&format!("- {id} [short] Ship NyPi M3; next: write the docs"))
                && !section.contains("Learn Rust")
        );
    }

    #[test]
    fn a_new_file_starts_from_the_template() {
        let dir = tempfile::tempdir().unwrap();
        let file = GoalsFile::new(dir.path().join("goals.md"));
        assert!(file.prompt_section().is_none());
        let id = file.add(Term::Long, "Lucia runs my week", &Change::default(), "first goal").unwrap();
        let text = file.read();
        assert!(text.starts_with("# Goals"));
        let long = text.find("## Long term").unwrap();
        assert!(text[long..].contains(&id), "{text}");
        assert!(file.update("g-nope", &Change::default(), "x").is_err());
    }
}
