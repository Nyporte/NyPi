//! The default mode network (docs/architecture.md §4.2, requirement R3, decision D37): when
//! nothing is going on, Lucia looks over her state (open goals, stuck or failed tasks, schedules,
//! commitments in memory) and decides whether anything deserves action, the way a mind wanders
//! back to unfinished business. A pass is a task with source `dmn`, run in its own process, and
//! usually ends in silence; when it did something or the user should know something, it sends a
//! short note. It acts unasked and spends money, so it is opt-in and capped per day. Passes are
//! kept in `~/.nypi/dmn/passes.jsonl`, which only the harness writes.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Paths;
use crate::ledger::{Ledger, TaskState};
use crate::memory::{MemoryKind, MemoryStore};
use crate::schedule::ScheduleStore;

/// The reply that means "nothing deserves attention"; the pass then stays silent.
pub const NOTHING: &str = "NOTHING";
const RECENT_TASKS: usize = 10;
const RECENT_MEMORIES: usize = 10;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DmnSettings {
    /// Off unless the user turns it on: it acts and spends without being asked.
    pub enabled: bool,
    /// Run once nothing has happened for this long.
    pub idle_minutes: u64,
    /// And at least this long after the last pass.
    pub min_gap_hours: f64,
    /// Also run after this long even when the user has been busy throughout.
    pub every_hours: f64,
    pub max_passes_per_day: usize,
    /// Tokens (input plus output) all passes of a day may use.
    pub daily_tokens: u64,
    /// Model for passes as `provider/id`; defaults to the agent's.
    pub model: Option<String>,
}

impl Default for DmnSettings {
    fn default() -> Self {
        DmnSettings {
            enabled: false,
            idle_minutes: 30,
            min_gap_hours: 2.0,
            every_hours: 12.0,
            max_passes_per_day: 6,
            daily_tokens: 300_000,
            model: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Why {
    Idle,
    Interval,
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Nothing,
    Note,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pass {
    pub at: String,
    pub why: Why,
    pub outcome: Outcome,
    pub tokens: u64,
    pub cost: f64,
    #[serde(default)]
    pub task: Option<String>,
    /// The note sent to the user, if any.
    #[serde(default)]
    pub note: Option<String>,
}

pub struct PassLog {
    path: PathBuf,
}

impl PassLog {
    pub fn new(path: PathBuf) -> Self {
        PassLog { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, pass: &Pass) -> std::io::Result<()> {
        crate::jsonl::append(&self.path, pass)
    }

    pub fn load(&self) -> Vec<Pass> {
        std::fs::read_to_string(&self.path)
            .map(|t| t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
            .unwrap_or_default()
    }

    /// Passes and tokens since local midnight.
    pub fn today(&self, now: DateTime<Utc>) -> (usize, u64) {
        let midnight = now.with_timezone(&chrono::Local).date_naive();
        let today: Vec<Pass> = self
            .load()
            .into_iter()
            .filter(|p| {
                DateTime::parse_from_rfc3339(&p.at)
                    .is_ok_and(|t| t.with_timezone(&chrono::Local).date_naive() == midnight)
            })
            .collect();
        (today.len(), today.iter().map(|p| p.tokens).sum())
    }
}

/// Whether a pass should start now, and why; or why not.
pub fn due(settings: &DmnSettings, paths: &Paths, now: DateTime<Utc>) -> Result<Why, String> {
    if !settings.enabled {
        return Err("the DMN is off (dmn.enabled)".into());
    }
    let log = PassLog::new(paths.dmn_passes());
    let (passes, tokens) = log.today(now);
    if passes >= settings.max_passes_per_day {
        return Err(format!("{passes} passes today, the most allowed"));
    }
    if tokens >= settings.daily_tokens {
        return Err(format!("{tokens} tokens used today, the daily budget"));
    }
    let last_pass = log.load().last().and_then(|p| parse(&p.at));
    let hours_since = |t: Option<DateTime<Utc>>| t.map_or(f64::INFINITY, |t| (now - t).num_seconds() as f64 / 3600.0);
    // Asked every scheduler round: looked at in place, not copied.
    let (running, last_activity) = Ledger::new(paths.ledger()).read(|tasks| {
        let running =
            tasks.values().any(|t| matches!(t.state, TaskState::Running | TaskState::Planned) && !t.is_interrupted());
        (running, tasks.values().filter_map(|t| parse(&t.heartbeat_at)).max())
    });
    if running {
        return Err("a task is running".into());
    }
    let idle_minutes = hours_since(last_activity) * 60.0;
    if idle_minutes >= settings.idle_minutes as f64 && hours_since(last_pass) >= settings.min_gap_hours {
        return Ok(Why::Idle);
    }
    if hours_since(last_pass) >= settings.every_hours {
        return Ok(Why::Interval);
    }
    if idle_minutes < settings.idle_minutes as f64 {
        Err(format!("idle {idle_minutes:.0} of {} min", settings.idle_minutes))
    } else {
        Err(format!("the last pass was {:.1} h ago, the gap is {} h", hours_since(last_pass), settings.min_gap_hours))
    }
}

fn parse(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text).ok().map(|t| t.with_timezone(&Utc))
}

const INSTRUCTIONS: &str = "This is your idle time: nobody asked you anything. Look over the state below the way a mind \
returns to unfinished business, and decide whether anything deserves action now: a goal whose next step you can \
take, a blocked or failed task worth another look, a commitment coming due, something the user should hear about.\n\
You may act with your tools as you would for the user (advance a goal, set a reminder with schedule, remember, \
delegate), but only what they would expect without asking: nothing sent to other people, nothing irreversible, no \
spending. This run is on a budget, so keep it short.\n\
Most of the time nothing deserves attention: then reply with exactly NOTHING. Otherwise reply with a note of two to \
four lines for the user: what you did, or what they should know. It is sent to them as a notification.";

/// What the vault needs from an idle pass (decision D81): what the harness put away itself,
/// and what is there for Lucia to file, distil, or write up.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Consolidation {
    /// Workspaces of settled tasks moved to the archive now: from, to.
    pub archived: Vec<(String, String)>,
    /// Notes in the inbox nobody filed yet, oldest first.
    pub inbox: Vec<String>,
    /// Lessons recent reflections drew, newest first: the stuff playbooks are made of.
    pub lessons: Vec<String>,
}

/// Inbox notes and lessons shown to a pass; more wait for the next one.
const INBOX_SHOWN: usize = 12;
const LESSONS_SHOWN: usize = 12;

/// The vault's upkeep before a pass. The part that needs no judgement is done here: a task's
/// workspace is moved to `90-archive/YYYY-MM/` once the task has been settled for
/// `archive_after_days`. A workspace no task of the ledger names is left alone, as is one whose
/// task is open or waits on the user. The rest is listed for the pass to work on. Nothing
/// outside the vault is written, and a vault that does not exist yet is not made for this.
pub fn consolidate(paths: &Paths, archive_after_days: u64, now: DateTime<Utc>) -> Consolidation {
    let mut found = Consolidation::default();
    if !paths.vault().is_dir() {
        return found;
    }
    let Ok(vault) = nypi_vault::vault::Vault::open(&paths.vault()) else { return found };
    let tasks = Ledger::new(paths.ledger()).load();
    let workspaces = vault.list(nypi_vault::vault::WORKSPACES).unwrap_or_default();
    for workspace in workspaces.into_iter().filter(|entry| entry.is_dir) {
        let mut of_it =
            tasks.values().filter(|task| task.workspace.as_deref() == Some(workspace.path.as_str())).peekable();
        if of_it.peek().is_none() {
            continue;
        }
        // Every round of the task is over, and the last of them long enough ago.
        let mut newest: Option<DateTime<Utc>> = None;
        let mut settled = true;
        for task in of_it {
            settled &= task.state.is_terminal();
            newest = newest.max(parse(&task.heartbeat_at));
        }
        let old = newest.is_some_and(|at| (now - at).num_days() >= archive_after_days as i64);
        if settled
            && old
            && let Ok(to) = vault.archive(&workspace.path, &newest.unwrap_or(now).format("%Y-%m").to_string())
        {
            found.archived.push((workspace.path, to));
        }
    }
    found.inbox = vault
        .list(nypi_vault::vault::INBOX)
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| !entry.is_dir)
        .map(|entry| entry.path)
        .collect();
    found.inbox.sort();

    // Reflections are files named for their day, newest last.
    let mut reflections: Vec<PathBuf> = std::fs::read_dir(crate::reflection::reflections_dir(paths))
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    reflections.sort();
    for file in reflections.iter().rev() {
        let text = std::fs::read_to_string(file).unwrap_or_default();
        let lessons = text
            .split("\n## ")
            .filter(|section| section.starts_with("Lessons"))
            .flat_map(|section| section.lines().filter_map(|line| line.strip_prefix("- ")))
            .map(|lesson| lesson.trim().to_string());
        found.lessons.extend(lessons);
        if found.lessons.len() >= LESSONS_SHOWN {
            found.lessons.truncate(LESSONS_SHOWN);
            break;
        }
    }
    found
}

const VAULT_INSTRUCTIONS: &str = "Your vault is yours to keep in order, and idle time is when you do it, with the \
vault tools and within this run's budget. Work on what is listed and stop when it is done; with nothing listed, \
leave the vault alone.\n\
- Inbox: file each note where it belongs. What it is about goes into the page of that person, project, or thing \
under 30-entities/ (make the page if there is none, link related pages with [[links]]); what belongs to a task \
under way goes into that task's workspace. A note you have filed leaves the inbox; one you cannot place stays.\n\
- Memory: where the commitments, events, and decisions above keep coming back to the same person, project, or \
thing, write what is durable about it into its page under 30-entities/, with [[links]], so it is found as one \
page and not as scattered sentences.\n\
- Lessons: a lesson that came up more than once, or that proved itself, becomes a draft under 20-playbooks/ \
(begin the note with `status: draft`): when it applies, the steps, how to check it worked. Do not touch skill \
folders or anything outside the vault.";

/// The vault's part of the digest; None when there is nothing to do or to say.
fn vault_section(found: &Consolidation) -> Option<String> {
    if found == &Consolidation::default() {
        return None;
    }
    let mut lines = vec![VAULT_INSTRUCTIONS.to_string()];
    if !found.archived.is_empty() {
        lines.push("Put away just now, since their tasks settled a while ago (nothing for you to do):".into());
        lines.extend(found.archived.iter().map(|(from, to)| format!("- {from} -> {to}")));
    }
    if !found.inbox.is_empty() {
        lines.push(format!("In the inbox, not filed yet ({}):", found.inbox.len()));
        lines.extend(found.inbox.iter().take(INBOX_SHOWN).map(|note| format!("- {note}")));
    }
    if !found.lessons.is_empty() {
        lines.push("Lessons from recent reflections (newest first):".into());
        lines.extend(found.lessons.iter().map(|lesson| format!("- {lesson}")));
    }
    Some(lines.join("\n"))
}

/// The pass's prompt: the instructions and a digest of the current state.
pub fn build_prompt(paths: &Paths, now: DateTime<Utc>) -> String {
    build_prompt_with(paths, now, &Consolidation::default())
}

/// `build_prompt`, with what the vault needs from this pass (`consolidate`).
pub fn build_prompt_with(paths: &Paths, now: DateTime<Utc>, vault: &Consolidation) -> String {
    format!("{INSTRUCTIONS}\n\n{}", state_digest(paths, now, vault))
}

/// The state a pass looks over (the time, goals, recent tasks, schedules, commitments in memory,
/// its last note, the vault's upkeep), without the instructions of a pass run as a task: the
/// Principal's idle activations get it with their own (decision D85).
pub fn state_digest(paths: &Paths, now: DateTime<Utc>, vault: &Consolidation) -> String {
    let mut sections = vec![format!("Now: {}", now.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M %A"))];
    let goals = crate::goals::GoalsFile::new(paths.goals()).prompt_section();
    sections.push(format!("Open goals:\n{}", goals.unwrap_or_else(|| "(none)".into())));

    let mut tasks: Vec<_> = Ledger::new(paths.ledger()).load().into_values().filter(|t| t.parent.is_none()).collect();
    tasks.sort_by(|a, b| b.heartbeat_at.cmp(&a.heartbeat_at));
    let tasks: Vec<String> = tasks
        .iter()
        .take(RECENT_TASKS)
        .map(|t| {
            let goal: String = t.goal.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(120).collect();
            let note = if t.note.is_empty() {
                String::new()
            } else {
                format!(" ({})", t.note.chars().take(100).collect::<String>())
            };
            format!(
                "- {} {:?} {} [{}]: {goal}{note}",
                t.id,
                t.state,
                t.heartbeat_at.get(..16).unwrap_or_default(),
                t.origin
            )
        })
        .collect();
    sections.push(format!("Recent tasks:\n{}", if tasks.is_empty() { "(none)".into() } else { tasks.join("\n") }));

    let schedules: Vec<String> = ScheduleStore::new(paths.schedules())
        .active()
        .iter()
        .map(crate::tools::schedule::line)
        .map(|l| format!("- {l}"))
        .collect();
    sections.push(format!("Schedules:\n{}", if schedules.is_empty() { "(none)".into() } else { schedules.join("\n") }));

    let mut memories: Vec<_> = MemoryStore::new(paths.memory())
        .load()
        .into_iter()
        .filter(|m| matches!(m.kind, MemoryKind::Commitment | MemoryKind::Event | MemoryKind::Decision))
        .collect();
    memories.reverse();
    let memories: Vec<String> = memories
        .iter()
        .take(RECENT_MEMORIES)
        .map(|m| format!("- {} [{}, {}] {}", m.id, m.kind.label(), m.created.get(..10).unwrap_or_default(), m.text))
        .collect();
    sections.push(format!(
        "Commitments, events, and decisions in memory (newest first):\n{}",
        if memories.is_empty() { "(none)".into() } else { memories.join("\n") }
    ));

    let passes = PassLog::new(paths.dmn_passes()).load();
    if let Some(last) = passes.iter().rev().find(|p| p.note.is_some()) {
        sections.push(format!(
            "Your last note to the user ({}), so you do not repeat it:\n{}",
            last.at.get(..16).unwrap_or_default(),
            last.note.as_deref().unwrap_or_default()
        ));
    }
    if let Some(section) = vault_section(vault) {
        sections.push(format!("Vault:\n{section}"));
    }
    sections.join("\n\n")
}

/// What a pass's answer means: silence, or a note for the user.
pub fn read_answer(answer: &str) -> Option<String> {
    let note: Vec<&str> = answer.lines().take_while(|l| crate::status::parse(l).is_none()).collect();
    let note = note.join("\n").trim().to_string();
    let silent = note.is_empty() || note.trim_matches(|c: char| !c.is_alphanumeric()).eq_ignore_ascii_case(NOTHING);
    (!silent).then_some(note)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_wait_for_idle_and_respect_the_budget() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let now = Utc::now();
        let mut settings = DmnSettings::default();
        assert!(due(&settings, &paths, now).is_err(), "off by default");
        settings.enabled = true;
        assert_eq!(due(&settings, &paths, now), Ok(Why::Idle), "nothing has ever happened: idle");

        let log = PassLog::new(paths.dmn_passes());
        let pass = |minutes_ago: i64, tokens: u64| Pass {
            at: (now - chrono::Duration::minutes(minutes_ago)).to_rfc3339(),
            why: Why::Idle,
            outcome: Outcome::Nothing,
            tokens,
            cost: 0.0,
            task: None,
            note: None,
        };
        log.append(&pass(30, 1000)).unwrap();
        assert!(due(&settings, &paths, now).unwrap_err().contains("the gap is 2 h"));
        let later = now + chrono::Duration::hours(3);
        if later.with_timezone(&chrono::Local).date_naive() == now.with_timezone(&chrono::Local).date_naive() {
            assert_eq!(due(&settings, &paths, later), Ok(Why::Idle), "the gap passed");
            log.append(&pass(0, settings.daily_tokens)).unwrap();
            assert!(due(&settings, &paths, later).unwrap_err().contains("budget"));
        }
    }

    #[test]
    fn nothing_means_silence() {
        assert_eq!(read_answer("NOTHING\n\nDone: nothing needed attention."), None);
        assert_eq!(read_answer("Nothing."), None);
        assert_eq!(
            read_answer("Moved goal g-2 forward: drafted the outline.\n\nDone: one goal advanced."),
            Some("Moved goal g-2 forward: drafted the outline.".into())
        );
    }

    #[test]
    fn idle_time_puts_settled_workspaces_away_and_lists_what_needs_filing() {
        use crate::ledger::{LedgerEvent, TaskId};
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let now = Utc::now();
        assert_eq!(consolidate(&paths, 7, now), Consolidation::default(), "no vault: nothing is made for this");
        assert!(!paths.vault().exists());

        let vault = nypi_vault::vault::Vault::open(&paths.vault()).unwrap();
        let write = |path: &str| vault.write(path, "x", nypi_vault::vault::WriteMode::Replace).unwrap();
        for workspace in ["old-done", "old-open", "fresh-done", "nobodys"] {
            write(&format!("10-workspaces/{workspace}/findings.md"));
        }
        write("00-inbox/261119-0900-shared-article.md");
        write("00-inbox/261118-1200-note.md");
        let ledger = Ledger::new(paths.ledger());
        let task = |id: &str, workspace: &str, days_ago: i64, state: TaskState| {
            let at = (now - chrono::Duration::days(days_ago)).to_rfc3339();
            let id = TaskId(id.into());
            let created = LedgerEvent::Created {
                task: id.clone(),
                at: at.clone(),
                goal: "g".into(),
                prompt: "p".into(),
                owner: "lucia".into(),
                origin: "cli".into(),
                parent: None,
                session_id: "s".into(),
                session_path: None,
                cwd: "/".into(),
                remote: None,
                pid: 1,
                pulse: None,
            };
            ledger.append(&created).unwrap();
            let linked = LedgerEvent::Workspace {
                task: id.clone(),
                at: at.clone(),
                workspace: format!("10-workspaces/{workspace}"),
            };
            ledger.append(&linked).unwrap();
            ledger.append(&LedgerEvent::State { task: id, at, state, note: String::new() }).unwrap();
        };
        task("t-1", "old-done", 9, TaskState::Done);
        // A second round of the same task, later: the workspace is as old as its last round.
        task("t-2", "old-done", 8, TaskState::Done);
        task("t-3", "old-open", 30, TaskState::BlockedOnUser);
        task("t-4", "fresh-done", 2, TaskState::Done);
        let reflections = crate::reflection::reflections_dir(&paths);
        std::fs::create_dir_all(&reflections).unwrap();
        let reflection = "# Reflection\n\n## Lessons (added)\n- Run the reproduction before the fix.\n\n## Skill proposals\n- **x**: y\n";
        std::fs::write(reflections.join("2026-11-18-t-1.md"), reflection).unwrap();

        let found = consolidate(&paths, 7, now);
        let month = (now - chrono::Duration::days(8)).format("%Y-%m").to_string();
        assert_eq!(found.archived, [("10-workspaces/old-done".to_string(), format!("90-archive/{month}/old-done"))]);
        assert!(paths.vault().join(format!("90-archive/{month}/old-done/findings.md")).is_file());
        // An open task's workspace, a recent one, and one no task names all stay.
        for kept in ["old-open", "fresh-done", "nobodys"] {
            assert!(paths.vault().join("10-workspaces").join(kept).is_dir(), "{kept}");
        }
        assert_eq!(found.inbox, ["00-inbox/261118-1200-note.md", "00-inbox/261119-0900-shared-article.md"]);
        assert_eq!(found.lessons, ["Run the reproduction before the fix."]);

        let prompt = build_prompt_with(&paths, now, &found);
        assert!(prompt.contains("Vault:") && prompt.contains("- 00-inbox/261118-1200-note.md"), "{prompt}");
        assert!(prompt.contains("10-workspaces/old-done -> 90-archive/"));
        assert!(prompt.contains("- Run the reproduction before the fix.") && prompt.contains("status: draft"));
        // Again: nothing is archived twice, and with nothing to do the pass is not told of the vault.
        assert!(consolidate(&paths, 7, now).archived.is_empty());
        assert!(!build_prompt(&paths, now).contains("Vault:"));
    }
}
