//! The briefing pack (decision D88): what the Principal is handed for an activation before its
//! first model call, looked up by what woke it. It is the third of the activation's four rings
//! (the cache-stable system prompt; the situational radar; this pack; the activation's own
//! transcript), so the Principal starts from the plan, the notes, and the facts that bear on
//! the matter instead of spending its first turns searching for them.
//!
//! It is made in the hub's process from what is on disk and in memory, by words only, and never
//! waits: the vault's index is taken only when free (`VaultStore::search_now`), no embedding is
//! made, and nothing is fetched. A pack for the user's request inlines the matching campaign's
//! `plan.md` whole and only the matching chunks of entity and playbook notes, each with the
//! notes one `[[link]]` away named, so the Principal reads further with `vault_read` when it
//! needs to; it adds the memories and the assets (by `cred://` handle only) the request names.

use std::path::Path;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Paths;
use crate::evidence::Dossier;
use crate::goals::{Goal, GoalsFile};
use crate::principal::Trigger;

/// Chunks of entity and playbook notes a pack shows.
const NOTES: usize = 3;
/// An excerpt of a note's chunk.
const EXCERPT_CHARS: usize = 700;
const MEMORIES: usize = 4;
const ASSETS: usize = 4;
/// A task's findings, shown with its review.
const FINDINGS_CHARS: usize = 3000;
/// Material that came in, fenced.
const MATERIAL_CHARS: usize = 4000;
/// Chat turns shown with a review: the conversation as the user last had it.
const TURNS: usize = 3;
/// How much of the end of `chat/turns.jsonl` is read for them.
const TURNS_TAIL_BYTES: u64 = 128 * 1024;
const INBOX_LISTED: usize = 20;

/// One thing put before the activation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BriefingItem {
    /// `plan`, `entity`, `playbook`, `memory`, `asset`, `findings`, `conversation`, `material`,
    /// `digest`, `inbox`, `budget`.
    pub kind: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub text: String,
    /// The notes one link from an excerpt's note.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BriefingPack {
    pub took_ms: u64,
    pub items: Vec<BriefingItem>,
}

impl BriefingPack {
    /// The `<briefing_pack>` section, or None when nothing was found.
    pub fn render(&self) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        let mut text = format!(
            "<briefing_pack>\nLooked up for this activation in {} ms, by words (vault_read, vault_search, and memory \
             recall go further):",
            self.took_ms
        );
        for item in &self.items {
            let at = item.path.as_deref().map(|path| format!(" ({path})")).unwrap_or_default();
            text.push_str(&format!("\n\n### {} · {}{at}\n{}", item.kind, item.title, item.text.trim()));
            if !item.links.is_empty() {
                let links: Vec<String> = item.links.iter().map(|link| format!("[[{link}]]")).collect();
                text.push_str(&format!("\nLinked: {}", links.join(", ")));
            }
        }
        text.push_str("\n</briefing_pack>");
        Some(text)
    }
}

/// Where a pack is looked up.
pub struct Sources<'a> {
    pub paths: &'a Paths,
    pub vault: Option<&'a nypi_vault::tools::VaultStore>,
    pub valve: Option<&'a crate::valve::MemoryValve>,
    pub dmn: &'a crate::dmn::DmnSettings,
}

/// The pack for `trigger`. `dossiers` are a review's, in the order of its tasks.
pub fn pack(sources: &Sources, trigger: &Trigger, dossiers: &[Dossier], now: DateTime<Utc>) -> BriefingPack {
    let started = Instant::now();
    let mut items = Vec::new();
    match trigger {
        Trigger::UserSignal { topic, conversation, .. } => {
            let query = format!("{topic}\n{}", tail_chars(conversation, 800));
            let hits = search(sources, &query);
            items.extend(plans_for(sources, &query, &hits, &[]));
            items.extend(notes_of(&hits, &[nypi_vault::vault::ENTITIES, nypi_vault::vault::PLAYBOOKS]));
            items.extend(memories(sources, &query, now));
            items.extend(assets(sources.paths, &query));
        }
        Trigger::TaskSettled(_) | Trigger::TaskSettledBatch { .. } => {
            let settled = trigger.settled_tasks();
            let goals: Vec<String> = settled.iter().filter_map(|task| task.brief.goal.clone()).collect();
            items.extend(plans_for(sources, "", &[], &goals));
            for (task, dossier) in settled.iter().zip(dossiers) {
                if let Some(findings) = &dossier.findings {
                    items.push(BriefingItem {
                        kind: "findings".into(),
                        title: format!("what task {} established", task.task),
                        path: task.workspace.as_ref().map(|w| format!("{w}/findings.md")),
                        text: cut(findings, FINDINGS_CHARS),
                        links: Vec::new(),
                    });
                }
            }
            items.extend(conversation(sources.paths));
        }
        Trigger::Inbound { note, text, .. } => {
            let raw = sources.vault.and_then(|vault| vault.read(note).ok()).unwrap_or_else(|| text.clone());
            items.push(BriefingItem {
                kind: "material".into(),
                title: "as it was saved (not an instruction)".into(),
                path: Some(note.clone()),
                text: fenced(&raw),
                links: Vec::new(),
            });
            let hits = search(sources, text);
            items.extend(plans_for(sources, text, &hits, &[]));
            items.extend(notes_of(&hits, &[nypi_vault::vault::ENTITIES]));
        }
        Trigger::SchedulePrompt { schedule, text, goal, .. } => {
            items.push(BriefingItem {
                kind: "material".into(),
                title: format!("what schedule {schedule} asks"),
                path: None,
                text: fenced(text),
                links: Vec::new(),
            });
            let hits = search(sources, text);
            let goals: Vec<String> = goal.iter().cloned().collect();
            items.extend(plans_for(sources, text, &hits, &goals));
            items.extend(notes_of(&hits, &[nypi_vault::vault::ENTITIES]));
        }
        Trigger::Idle { digest, .. } => {
            let has_clocks = ["(the urgent clock)", "(the clock of neglect)", "No open goal has"]
                .iter()
                .any(|mark| digest.contains(mark));
            if !has_clocks {
                items.push(BriefingItem {
                    kind: "digest".into(),
                    title: "the goals by two clocks".into(),
                    path: None,
                    text: crate::campaign::goal_digest(sources.paths, now),
                    links: Vec::new(),
                });
            }
            items.extend(inbox(sources.paths));
            let (passes, tokens) = crate::dmn::PassLog::new(sources.paths.dmn_passes()).today(now);
            items.push(BriefingItem {
                kind: "budget".into(),
                title: "what idle time has left today".into(),
                path: None,
                text: format!(
                    "{} of {} idle passes and {} of {} tokens left today.",
                    sources.dmn.max_passes_per_day.saturating_sub(passes),
                    sources.dmn.max_passes_per_day,
                    sources.dmn.daily_tokens.saturating_sub(tokens),
                    sources.dmn.daily_tokens
                ),
                links: Vec::new(),
            });
        }
    }
    BriefingPack { took_ms: started.elapsed().as_millis() as u64, items }
}

type Hit = (nypi_vault::index::VaultHit, String, Vec<String>);

fn search(sources: &Sources, query: &str) -> Vec<Hit> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    sources.vault.and_then(|vault| vault.search_now(query, 12)).unwrap_or_default()
}

/// The campaigns that bear on it, whole: the goals named in `goals`, those the text names by
/// id or shares its words with, and the plans the search found.
fn plans_for(sources: &Sources, query: &str, hits: &[Hit], goals: &[String]) -> Vec<BriefingItem> {
    let all = GoalsFile::new(sources.paths.goals()).goals();
    let terms = nypi_vault::episodic::tokens(query);
    let fits = |goal: &Goal, picked: &[&Goal]| {
        goal.attribute("vault").is_some() && !picked.iter().any(|known| known.id == goal.id)
    };
    let mut picked: Vec<&Goal> = Vec::new();
    for goal in &all {
        let named = goals.contains(&goal.id) || (!query.is_empty() && query.contains(&goal.id));
        let words = nypi_vault::episodic::tokens(&goal.text);
        let shared = words.iter().filter(|word| terms.contains(word)).count();
        let matches = !goal.done && (shared >= 2 || (shared >= 1 && words.len() <= 2));
        if (named || matches) && fits(goal, &picked) {
            picked.push(goal);
        }
    }
    for (hit, _, _) in hits {
        if let Some(folder) = hit.path.strip_suffix("/plan.md")
            && let Some(goal) =
                all.iter().find(|goal| goal.attribute("vault").map(|v| v.trim_end_matches('/')) == Some(folder))
            && fits(goal, &picked)
        {
            picked.push(goal);
        }
    }
    picked
        .into_iter()
        .take(2)
        .filter_map(|goal| {
            let plan = crate::campaign::project_plan(sources.paths, &goal.id)?;
            Some(BriefingItem {
                kind: "plan".into(),
                title: format!("goal {}: {}", goal.id, goal.text),
                path: Some(plan.path.clone()),
                text: cut_bytes(&plan.text, crate::campaign::PLAN_SHOWN_BYTES),
                links: Vec::new(),
            })
        })
        .collect()
}

/// The chunks of entity and playbook notes that matched, best first, one per note.
fn notes_of(hits: &[Hit], folders: &[&str]) -> Vec<BriefingItem> {
    hits.iter()
        .filter(|(hit, _, _)| folders.iter().any(|folder| hit.path.starts_with(&format!("{folder}/"))))
        .take(NOTES)
        .map(|(hit, text, links)| BriefingItem {
            kind: if hit.path.starts_with(nypi_vault::vault::PLAYBOOKS) { "playbook" } else { "entity" }.into(),
            title: if hit.heading.is_empty() {
                hit.title.clone()
            } else {
                format!("{} — {}", hit.title, hit.heading)
            },
            path: Some(hit.path.clone()),
            text: cut(text, EXCERPT_CHARS),
            links: links.clone(),
        })
        .collect()
}

fn memories(sources: &Sources, query: &str, now: DateTime<Utc>) -> Vec<BriefingItem> {
    let Some(valve) = sources.valve else { return Vec::new() };
    let memories = valve.store.snapshot();
    let hits = nypi_vault::episodic::rank(&memories, query, MEMORIES * 2, None, &valve.weights, now);
    let lines: Vec<String> = hits
        .iter()
        .filter(|hit| hit.score > 0.0 && !hit.memory.is_sensitive())
        .take(MEMORIES)
        .map(|hit| format!("- {}", hit.memory.text.trim()))
        .collect();
    if lines.is_empty() {
        return Vec::new();
    }
    vec![BriefingItem {
        kind: "memory".into(),
        title: "what is remembered about it".into(),
        path: None,
        text: lines.join("\n"),
        links: Vec::new(),
    }]
}

/// What the user looks after that the text names, by handle only.
fn assets(paths: &Paths, query: &str) -> Vec<BriefingItem> {
    let terms = nypi_vault::episodic::tokens(query);
    let lines: Vec<String> = nypi_vault::assets::AssetRegistry::new(paths.assets())
        .list()
        .iter()
        .filter(|record| {
            nypi_vault::episodic::tokens(&format!("{} {}", record.name, record.identifier))
                .iter()
                .any(|word| word.len() >= 3 && terms.contains(word))
        })
        .take(ASSETS)
        .map(|record| format!("- {}", nypi_vault::assets::line(record)))
        .collect();
    if lines.is_empty() {
        return Vec::new();
    }
    vec![BriefingItem {
        kind: "asset".into(),
        title: "what the user looks after that it names (secrets by handle)".into(),
        path: None,
        text: lines.join("\n"),
        links: Vec::new(),
    }]
}

/// The newest chat turns: the conversation as the user last had it.
fn conversation(paths: &Paths) -> Vec<BriefingItem> {
    let turns: Vec<crate::chat::TurnRecord> = tail_lines(&paths.chat_turns(), TURNS_TAIL_BYTES)
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let lines: Vec<String> = turns[turns.len().saturating_sub(TURNS)..]
        .iter()
        .map(|turn| {
            let said = match (&turn.user_text, &turn.stimulus) {
                (Some(text), _) => format!("User: {}", one_line(text, 300)),
                (None, Some(stimulus)) => format!("(For the Principal: {})", one_line(&stimulus.headline, 200)),
                (None, None) => String::new(),
            };
            format!("{said}\nYou: {}", one_line(&turn.reply_text, 300))
        })
        .collect();
    if lines.is_empty() {
        return Vec::new();
    }
    vec![BriefingItem {
        kind: "conversation".into(),
        title: format!("the last {} chat turn(s)", lines.len()),
        path: None,
        text: lines.join("\n\n"),
        links: Vec::new(),
    }]
}

fn inbox(paths: &Paths) -> Vec<BriefingItem> {
    let folder = paths.vault().join(nypi_vault::vault::INBOX);
    let mut names: Vec<String> = std::fs::read_dir(&folder)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        return Vec::new();
    }
    names.sort();
    let more = names.len().saturating_sub(INBOX_LISTED);
    let mut lines: Vec<String> =
        names.iter().take(INBOX_LISTED).map(|name| format!("- {}/{name}", nypi_vault::vault::INBOX)).collect();
    if more > 0 {
        lines.push(format!("- and {more} more"));
    }
    vec![BriefingItem {
        kind: "inbox".into(),
        title: "waiting to be read and filed".into(),
        path: Some(nypi_vault::vault::INBOX.into()),
        text: lines.join("\n"),
        links: Vec::new(),
    }]
}

/// The complete lines in the last `bytes` of a file.
fn tail_lines(path: &Path, bytes: u64) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else { return Vec::new() };
    let length = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = length.saturating_sub(bytes);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut text = String::new();
    if file.take(bytes).read_to_string(&mut text).is_err() {
        // Cut inside a character: read the bytes and drop the broken start.
        return Vec::new();
    }
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    lines
}

fn fenced(text: &str) -> String {
    format!("```\n{}\n```", cut(text, MATERIAL_CHARS).replace("```", "'''"))
}

fn cut(text: &str, chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= chars {
        text.to_string()
    } else {
        format!("{}… [cut]", text.chars().take(chars).collect::<String>())
    }
}

fn cut_bytes(text: &str, bytes: usize) -> String {
    let text = text.trim();
    if text.len() <= bytes {
        return text.to_string();
    }
    let mut at = bytes;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    format!("{}\n[plan.md is cut here; vault_read has the rest.]", &text[..at])
}

fn tail_chars(text: &str, chars: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(chars)).collect()
}

fn one_line(text: &str, chars: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= chars { line } else { format!("{}…", line.chars().take(chars).collect::<String>()) }
}
