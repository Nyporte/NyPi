//! The four tiers of a campaign (decision D85): a goal in `goals.md` names its project folder
//! in the vault (`vault: 10-projects/<slug>`), whose `plan.md` lays out the workstreams; a task
//! and a schedule carry the goal they serve (`goal`), so a task for a goal sees its plan on every
//! turn (`<project_plan>`); and idle time is spent by two clocks, how near a goal's deadline is
//! and how long it has been left, read here as the Principal's goal digest.

use std::path::Path;

use chrono::{DateTime, NaiveDate, Utc};

use crate::config::Paths;
use crate::goals::{Goal, GoalsFile};
use crate::ledger::Ledger;

/// How much of `plan.md` a task is shown on every turn.
pub const PLAN_SHOWN_BYTES: usize = 6 * 1024;
/// The plan's section a digest reads its items from.
const WORKSTREAMS: &str = "## Workstreams";
/// A deadline this near, or past, is urgent.
const URGENT_DAYS: i64 = 14;
/// Goals listed under the clock of neglect.
const LEFT_LONGEST: usize = 3;

/// A goal's plan, as it reads now.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectPlan {
    pub goal: String,
    /// Relative to the vault: `10-projects/<slug>/plan.md`.
    pub path: String,
    pub text: String,
}

/// The plan of goal `goal_id`, when the goal names a project folder with a `plan.md` in it.
pub fn project_plan(paths: &Paths, goal_id: &str) -> Option<ProjectPlan> {
    let goal = GoalsFile::new(paths.goals()).find(goal_id)?;
    let folder = goal.attribute("vault")?.trim_end_matches('/').to_string();
    let vault = nypi_vault::vault::Vault::open(&paths.vault()).ok()?;
    let path = format!("{folder}/plan.md");
    let text = vault.read(&path).ok()?;
    Some(ProjectPlan { goal: goal.id, path, text })
}

/// The `<project_plan>` section: the plan, cut at `PLAN_SHOWN_BYTES`, above the task's own
/// workspace, so the task is done as a step of the campaign.
pub fn plan_section(plan: &ProjectPlan) -> String {
    let mut text = plan.text.trim().to_string();
    let mut note = String::new();
    if text.len() > PLAN_SHOWN_BYTES {
        let mut cut = PLAN_SHOWN_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        note = format!("\n[plan.md is over {} KB and is cut here; vault_read has the rest.]", PLAN_SHOWN_BYTES / 1024);
    }
    format!(
        "This task serves goal {goal}, whose campaign is {path} in your vault. Work toward its objective; when your \
         task finishes a milestone, say so in your report (the Principal keeps the plan).\n\n{text}{note}",
        goal = plan.goal,
        path = plan.path,
    )
}

/// How far along a plan's item is: `[ ]`, `[~]`, `[!]`, `[x]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Open,
    Doing,
    Blocked,
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanItem {
    pub mark: Mark,
    pub text: String,
}

/// The items of a plan's `## Workstreams & Milestones` section, in order.
pub fn plan_items(plan: &str) -> Vec<PlanItem> {
    let mut items = Vec::new();
    let mut inside = false;
    for line in plan.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("## ") {
            inside = trimmed.starts_with(WORKSTREAMS);
            continue;
        }
        if !inside {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("- [").or_else(|| trimmed.strip_prefix("* [")) else { continue };
        let mut chars = rest.chars();
        let (Some(mark), Some(']')) = (chars.next(), chars.next()) else { continue };
        let mark = match mark {
            ' ' => Mark::Open,
            '~' => Mark::Doing,
            '!' => Mark::Blocked,
            'x' | 'X' => Mark::Done,
            _ => continue,
        };
        items.push(PlanItem { mark, text: chars.as_str().trim().to_string() });
    }
    items
}

/// One goal on the two clocks.
#[derive(Clone, Debug, PartialEq)]
pub struct GoalClock {
    pub id: String,
    pub text: String,
    /// Days until `due`; negative when past.
    pub due_in_days: Option<i64>,
    /// Days since a task last served it, or since it was set when none has.
    pub idle_days: Option<i64>,
    /// The newest task that served it.
    pub last_task: Option<String>,
    pub project: Option<String>,
    /// Items of its plan marked `[!]`.
    pub blocked: Vec<String>,
    /// The first item of its plan under way (`[~]`) or open (`[ ]`).
    pub next_item: Option<String>,
}

/// Every open goal on both clocks, at `now`.
pub fn clocks(paths: &Paths, now: DateTime<Utc>) -> Vec<GoalClock> {
    let today = now.with_timezone(&chrono::Local).date_naive();
    let tasks = Ledger::new(paths.ledger()).load();
    let vault = nypi_vault::vault::Vault::open(&paths.vault()).ok();
    GoalsFile::new(paths.goals())
        .goals()
        .into_iter()
        .filter(|goal| !goal.done)
        .map(|goal| {
            let last = tasks
                .values()
                .filter(|task| task.goal_id.as_deref() == Some(goal.id.as_str()))
                .max_by(|a, b| a.heartbeat_at.cmp(&b.heartbeat_at));
            let last_day = last
                .and_then(|task| DateTime::parse_from_rfc3339(&task.heartbeat_at).ok())
                .map(|at| at.with_timezone(&chrono::Local).date_naive())
                .or_else(|| date(&goal, "since"));
            let project = goal.attribute("vault").map(|folder| folder.trim_end_matches('/').to_string());
            let items = project
                .as_ref()
                .zip(vault.as_ref())
                .and_then(|(folder, vault)| vault.read(&format!("{folder}/plan.md")).ok())
                .map(|plan| plan_items(&plan))
                .unwrap_or_default();
            let next_item = items
                .iter()
                .find(|item| item.mark == Mark::Doing)
                .or_else(|| items.iter().find(|item| item.mark == Mark::Open))
                .map(|item| item.text.clone());
            GoalClock {
                due_in_days: date(&goal, "due").map(|due| (due - today).num_days()),
                idle_days: last_day.map(|day| (today - day).num_days().max(0)),
                last_task: last.map(|task| task.id.0.clone()),
                blocked: items.iter().filter(|item| item.mark == Mark::Blocked).map(|item| item.text.clone()).collect(),
                next_item,
                project,
                id: goal.id,
                text: goal.text,
            }
        })
        .collect()
}

fn date(goal: &Goal, key: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(goal.attribute(key)?.trim(), "%Y-%m-%d").ok()
}

/// What idle time is for, by two clocks (the Principal's `P2` activations): goals whose
/// deadline is near or past and what blocks them (the owner is nudged, through chat), the goals
/// left longest with the next item of their plan (one is advanced with a task), and the inbox
/// waiting to be filed.
pub fn goal_digest(paths: &Paths, now: DateTime<Utc>) -> String {
    let clocks = clocks(paths, now);
    let mut sections = Vec::new();
    let mut urgent: Vec<&GoalClock> =
        clocks.iter().filter(|clock| clock.due_in_days.is_some_and(|days| days <= URGENT_DAYS)).collect();
    urgent.sort_by_key(|clock| clock.due_in_days);
    if !urgent.is_empty() {
        let lines: Vec<String> = urgent
            .iter()
            .map(|clock| {
                let days = clock.due_in_days.unwrap_or_default();
                let when = match days {
                    d if d < 0 => format!("{} days overdue", -d),
                    0 => "due today".to_string(),
                    d => format!("due in {d} days"),
                };
                let blocked = if clock.blocked.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; blocked: {} (nudge the user through stimulate_chat if only they can unblock it)",
                        clock.blocked.join("; ")
                    )
                };
                format!("- {} {}: {when}{blocked}", clock.id, clock.text)
            })
            .collect();
        sections.push(format!("## Deadlines (the urgent clock)\n{}", lines.join("\n")));
    }
    let mut neglected: Vec<&GoalClock> = clocks.iter().filter(|clock| clock.idle_days.is_some()).collect();
    neglected.sort_by_key(|clock| std::cmp::Reverse(clock.idle_days));
    if !neglected.is_empty() {
        let lines: Vec<String> = neglected
            .iter()
            .take(LEFT_LONGEST)
            .map(|clock| {
                let last = clock.last_task.as_ref().map(|task| format!(", last task {task}")).unwrap_or_default();
                let next = match (&clock.next_item, &clock.project) {
                    (Some(item), _) => format!("; next in its plan: {item}"),
                    (None, Some(project)) => format!("; its plan ({project}/plan.md) has no open item"),
                    (None, None) => "; no project folder yet (a plan in 10-projects/ would give it steps)".into(),
                };
                format!("- {} {}: left {} days{last}{next}", clock.id, clock.text, clock.idle_days.unwrap_or_default())
            })
            .collect();
        sections.push(format!(
            "## Left longest (the clock of neglect)\nAdvance one unblocked item with start_task (give its `goal`), so no \
             goal starves:\n{}",
            lines.join("\n")
        ));
    }
    let inbox = waiting_in(&paths.vault().join(nypi_vault::vault::INBOX));
    if inbox > 0 {
        sections.push(format!("## Inbox\n{inbox} note(s) in {}/ wait to be read and filed.", nypi_vault::vault::INBOX));
    }
    if sections.is_empty() {
        "No open goal has a deadline or a history yet, and the inbox is empty.".to_string()
    } else {
        sections.join("\n\n")
    }
}

/// Notes in a folder, not counting hidden files.
fn waiting_in(folder: &Path) -> usize {
    std::fs::read_dir(folder)
        .map(|entries| entries.flatten().filter(|entry| !entry.file_name().to_string_lossy().starts_with('.')).count())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plans_items_are_read_from_its_workstreams_only() {
        let plan = "---\ngoal: g-1\n---\n# Launch\n\n## Objective\n- [ ] not an item here\n\n## Workstreams & Milestones\n\
                    - [x] pick a name\n- [~] write the landing page\n- [!] buy the domain (needs the user's card)\n\
                    - [ ] announce\n\n## Key Decisions\n- [ ] nor here\n";
        let items = plan_items(plan);
        let marks: Vec<Mark> = items.iter().map(|item| item.mark).collect();
        assert_eq!(marks, [Mark::Done, Mark::Doing, Mark::Blocked, Mark::Open]);
        assert_eq!(items[2].text, "buy the domain (needs the user's card)");
    }

    #[test]
    fn a_long_plan_is_cut_and_says_so() {
        let plan = ProjectPlan { goal: "g-1".into(), path: "10-projects/x/plan.md".into(), text: "é".repeat(5000) };
        let section = plan_section(&plan);
        assert!(section.contains("serves goal g-1") && section.contains("is cut here"));
        assert!(section.len() < PLAN_SHOWN_BYTES + 600);
    }
}
