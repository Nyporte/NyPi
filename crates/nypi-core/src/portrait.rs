//! The user portrait (docs/architecture.md §4.4, requirement R7, decision D35): who the user is,
//! as lasting traits (preferences, values, style, goals, dislikes, identity) learned from their
//! own words. After a turn, one background, tool-free model call reads the user's new messages
//! and proposes changes; every change must quote the user verbatim, and the harness keeps only
//! changes whose quotes it finds in those messages, so each trait links back to the sentences
//! that show it (A4). The portrait is a short section of the situational prompt. Specific facts
//! and anything sensitive belong in memory, not here. Only the harness writes the log.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nypi_ai::{Model, StreamFn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::inspector::Inspector;
use crate::memory::now;
use crate::pulse::{PulseBus, PulseKind, PulsePayload, PulseSource, Subscription};

/// Traits kept at most; past this, new ones wait until the user removes some.
const MAX_TRAITS: usize = 40;
/// Evidence kept per trait (the newest).
const MAX_EVIDENCE: usize = 8;
/// The prompt section never grows past this.
const PROMPT_CHARS: usize = 2500;
/// A quote shorter than this proves nothing.
const MIN_QUOTE_CHARS: usize = 4;
const MESSAGE_CHARS: usize = 2000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortraitMode {
    /// After turns where the user said enough; scripted test models only with `always`.
    #[default]
    Auto,
    Always,
    Off,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PortraitSettings {
    pub mode: PortraitMode,
    /// The user's new messages must hold at least this many words in all.
    pub min_words: usize,
    /// Model for the portrait call as `provider/id`; defaults to the agent's.
    pub model: Option<String>,
}

impl Default for PortraitSettings {
    fn default() -> Self {
        PortraitSettings { mode: PortraitMode::Auto, min_words: 6, model: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Identity,
    Value,
    Goal,
    Style,
    Preference,
    Dislike,
}

impl Category {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.trim().to_lowercase().as_str() {
            "identity" => Category::Identity,
            "value" => Category::Value,
            "goal" => Category::Goal,
            "style" => Category::Style,
            "preference" => Category::Preference,
            "dislike" => Category::Dislike,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Category::Identity => "identity",
            Category::Value => "value",
            Category::Goal => "goal",
            Category::Style => "style",
            Category::Preference => "preference",
            Category::Dislike => "dislike",
        }
    }
}

/// Where a trait was shown: the user's exact words, and the message they came from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    pub quote: String,
    /// The `userMessage` pulse, for `nypi trace`.
    pub pulse: String,
    pub session: String,
    pub at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum PortraitEvent {
    Added { id: String, at: String, category: Category, text: String, evidence: Vec<Evidence> },
    Revised { id: String, at: String, text: Option<String>, evidence: Vec<Evidence> },
    Removed { id: String, at: String, reason: String, by: String, evidence: Vec<Evidence> },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Trait {
    pub id: String,
    pub category: Category,
    pub text: String,
    pub evidence: Vec<Evidence>,
    /// Times shown, including evidence no longer kept.
    pub shown: usize,
    pub created: String,
    pub updated: String,
}

pub struct Portrait {
    path: PathBuf,
    write: Mutex<()>,
}

impl Portrait {
    pub fn new(path: PathBuf) -> Self {
        Portrait { path, write: Mutex::new(()) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, event: &PortraitEvent) -> std::io::Result<()> {
        let _guard = self.write.lock().unwrap();
        crate::jsonl::append(&self.path, event)
    }

    /// Live traits, most shown first.
    pub fn load(&self) -> Vec<Trait> {
        let mut traits: Vec<Trait> = Vec::new();
        let Ok(text) = std::fs::read_to_string(&self.path) else { return traits };
        for line in text.lines() {
            let Ok(event) = serde_json::from_str::<PortraitEvent>(line) else { continue };
            match event {
                PortraitEvent::Added { id, at, category, text, evidence } => traits.push(Trait {
                    id,
                    category,
                    text,
                    shown: evidence.len(),
                    evidence,
                    created: at.clone(),
                    updated: at,
                }),
                PortraitEvent::Revised { id, at, text, evidence } => {
                    if let Some(t) = traits.iter_mut().find(|t| t.id == id) {
                        if let Some(text) = text {
                            t.text = text;
                        }
                        t.shown += evidence.len();
                        t.evidence.extend(evidence);
                        let excess = t.evidence.len().saturating_sub(MAX_EVIDENCE);
                        t.evidence.drain(..excess);
                        t.updated = at;
                    }
                }
                PortraitEvent::Removed { id, .. } => traits.retain(|t| t.id != id),
            }
        }
        traits.sort_by(|a, b| b.shown.cmp(&a.shown).then(a.created.cmp(&b.created)));
        traits
    }

    pub fn get(&self, id: &str) -> Option<Trait> {
        self.load().into_iter().find(|t| t.id == id)
    }

    /// The situational prompt section, or None while the portrait is empty.
    pub fn prompt_section(&self) -> Option<String> {
        let traits = self.load();
        if traits.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        let mut size = 0;
        for t in &traits {
            let line = format!("- {} ({}, shown {}×)", t.text, t.category.label(), t.shown);
            size += line.len() + 1;
            if size > PROMPT_CHARS {
                break;
            }
            lines.push(line);
        }
        Some(lines.join("\n"))
    }
}

/// One of the user's messages, as the portrait call sees it.
#[derive(Clone, Debug)]
pub struct Said {
    pub pulse: String,
    pub text: String,
}

#[derive(Debug, Default, Deserialize)]
struct Proposal {
    #[serde(default)]
    changes: Vec<Change>,
}

#[derive(Debug, Deserialize)]
struct Change {
    op: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    evidence: Vec<String>,
}

/// What one portrait pass did.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Applied {
    pub added: Vec<String>,
    pub revised: Vec<String>,
    pub removed: Vec<String>,
    /// Changes dropped, and why (a quote not found in the user's words, an unknown id, ...).
    pub rejected: Vec<String>,
}

impl Applied {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.revised.is_empty() && self.removed.is_empty()
    }
}

const SYSTEM_PROMPT: &str = "You keep a portrait of the user: the lasting traits that help an assistant work well with \
them. You read what they said and update the portrait. Reply with JSON only.";

const INSTRUCTIONS: &str = "Say what the user's new messages show about them, if anything. A trait is one short sentence \
in the third person (\"Prefers ...\", \"Values ...\", \"Writes in ...\") in one category:\n\
- identity: role, expertise, languages they use\n\
- value: what they care about\n\
- goal: what they keep working toward\n\
- style: how they communicate and decide\n\
- preference: how they like things done\n\
- dislike: what they want avoided\n\
Rules:\n\
- Only lasting traits. One-off requests and specific facts (names, dates, appointments, devices) belong to memory, not here.\n\
- Nothing sensitive: no health, money, relationships, beliefs, or private life, and no secrets.\n\
- Every change quotes the user: copy their exact words into evidence, verbatim and short. A change whose quote is not in \
their messages is discarded.\n\
- When a message shows an existing trait again, revise it by id with the new evidence (and better wording if the \
message refines it). Revise only when the meaning stays the same. When the user contradicts a trait or changes their \
mind, remove the old trait with the contradicting quote and add the new one: never revise a trait into its opposite, \
since its evidence would then argue against it.\n\
- Usually there is nothing new: return no changes.\n\
Reply with this JSON and nothing else:\n\
{\"changes\": [{\"op\": \"add\", \"category\": \"style\", \"text\": \"...\", \"evidence\": [\"...\"]}, \
{\"op\": \"revise\", \"id\": \"u-...\", \"text\": \"... (optional)\", \"evidence\": [\"...\"]}, \
{\"op\": \"remove\", \"id\": \"u-...\", \"evidence\": [\"...\"]}]}";

pub fn build_prompt(traits: &[Trait], said: &[Said]) -> String {
    let portrait = if traits.is_empty() {
        "(empty)".to_string()
    } else {
        traits
            .iter()
            .map(|t| format!("- {} [{}] {} (shown {}×)", t.id, t.category.label(), t.text, t.shown))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let messages = said
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}. {}", i + 1, clip(&s.text, MESSAGE_CHARS)))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{INSTRUCTIONS}\n\nThe portrait now:\n{portrait}\n\nThe user's new messages:\n{messages}")
}

/// Lowercase, whitespace collapsed, quotes and outer punctuation trimmed: how quotes are matched.
fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .trim_matches(|c: char| c.is_ascii_punctuation() || "“”‘’「」『』。，！？…".contains(c) || c.is_whitespace())
        .to_string()
}

/// Each quote found in the user's messages, with the message it came from; the rest are dropped.
fn checked(quotes: &[String], said: &[Said], session: &str) -> (Vec<Evidence>, usize) {
    let mut evidence = Vec::new();
    let mut missing = 0;
    for quote in quotes {
        let needle = normalize(quote);
        let found = (needle.chars().count() >= MIN_QUOTE_CHARS)
            .then(|| said.iter().find(|s| normalize(&s.text).contains(&needle)))
            .flatten();
        match found {
            Some(s) => evidence.push(Evidence {
                quote: quote.trim().to_string(),
                pulse: s.pulse.clone(),
                session: session.to_string(),
                at: now(),
            }),
            None => missing += 1,
        }
    }
    (evidence, missing)
}

/// Check a model's proposal against what the user said and record what holds.
pub fn apply(portrait: &Portrait, reply: &str, said: &[Said], session: &str) -> anyhow::Result<Applied> {
    let object = crate::oneshot::json_object(reply).ok_or_else(|| anyhow::anyhow!("no JSON object in the reply"))?;
    let proposal: Proposal = serde_json::from_str(object)?;
    let traits = portrait.load();
    let mut applied = Applied::default();
    let mut count = traits.len();
    for change in proposal.changes {
        let (evidence, missing) = checked(&change.evidence, said, session);
        let label = change.text.clone().or(change.id.clone()).unwrap_or_default();
        if evidence.is_empty() {
            let why = if missing > 0 { "its quote is not in the user's messages" } else { "no evidence" };
            applied.rejected.push(format!("{} {label}: {why}", change.op));
            continue;
        }
        let existing = change.id.as_deref().and_then(|id| traits.iter().find(|t| t.id == id));
        match (change.op.as_str(), existing) {
            ("add", _) => {
                let (Some(category), Some(text)) = (
                    change.category.as_deref().and_then(Category::parse),
                    change.text.filter(|t| !t.trim().is_empty()),
                ) else {
                    applied.rejected.push(format!("add {label}: needs a known category and text"));
                    continue;
                };
                if let Some(same) = traits.iter().find(|t| normalize(&t.text) == normalize(&text)) {
                    portrait.append(&PortraitEvent::Revised {
                        id: same.id.clone(),
                        at: now(),
                        text: None,
                        evidence,
                    })?;
                    applied.revised.push(same.id.clone());
                    continue;
                }
                if count >= MAX_TRAITS {
                    applied.rejected.push(format!("add {text}: the portrait is full ({MAX_TRAITS} traits)"));
                    continue;
                }
                let id = format!("u-{}", &uuid::Uuid::now_v7().simple().to_string()[20..]);
                portrait.append(&PortraitEvent::Added {
                    id: id.clone(),
                    at: now(),
                    category,
                    text: text.trim().into(),
                    evidence,
                })?;
                applied.added.push(id);
                count += 1;
            }
            ("revise", Some(t)) => {
                let text = change.text.map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && *s != t.text);
                portrait.append(&PortraitEvent::Revised { id: t.id.clone(), at: now(), text, evidence })?;
                applied.revised.push(t.id.clone());
            }
            ("remove", Some(t)) => {
                let reason = format!("contradicted: \"{}\"", evidence[0].quote);
                portrait.append(&PortraitEvent::Removed {
                    id: t.id.clone(),
                    at: now(),
                    reason,
                    by: "portrait".into(),
                    evidence,
                })?;
                applied.removed.push(t.id.clone());
                count -= 1;
            }
            (op, _) => applied.rejected.push(format!("{op} {label}: unknown operation or trait id")),
        }
    }
    Ok(applied)
}

/// Collects what the user says (their `userMessage` pulses, from every front end and from chat)
/// and, after a turn, runs a portrait pass on it in the background.
pub struct PortraitKeeper {
    portrait: Arc<Portrait>,
    settings: PortraitSettings,
    model: Model,
    stream_fn: StreamFn,
    api_key: Option<String>,
    retry: nypi_ai::retry::RetryPolicy,
    session: String,
    inspector: Arc<Inspector>,
    heard: Mutex<Subscription>,
    pending: Mutex<Vec<Said>>,
    running: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

pub struct KeeperOptions {
    pub portrait: Arc<Portrait>,
    pub settings: PortraitSettings,
    pub model: Model,
    pub stream_fn: StreamFn,
    pub api_key: Option<String>,
    pub retry: nypi_ai::retry::RetryPolicy,
    pub session: String,
    pub inspector: Arc<Inspector>,
}

impl PortraitKeeper {
    pub fn new(bus: &PulseBus, options: KeeperOptions) -> Self {
        PortraitKeeper {
            portrait: options.portrait,
            settings: options.settings,
            model: options.model,
            stream_fn: options.stream_fn,
            api_key: options.api_key,
            retry: options.retry,
            session: options.session,
            inspector: options.inspector,
            heard: Mutex::new(bus.subscribe("portrait", &[PulseKind::UserMessage])),
            pending: Mutex::new(Vec::new()),
            running: Mutex::new(Vec::new()),
        }
    }

    /// Take in what the user said since the last turn; when it is enough, look at it in the
    /// background. One pass at a time: messages heard meanwhile wait for the next turn.
    pub fn after_turn(&self) {
        {
            let mut heard = self.heard.lock().unwrap();
            let mut pending = self.pending.lock().unwrap();
            while let Some(pulse) = heard.try_recv() {
                if let (PulseSource::UserInput { .. }, PulsePayload::UserMessage { text, .. }) =
                    (&pulse.source, &pulse.payload)
                    && !text.trim().is_empty()
                    && !text.trim_start().starts_with('/')
                {
                    pending.push(Said { pulse: pulse.id.0.clone(), text: text.clone() });
                }
            }
        }
        let scripted = self.model.api == nypi_ai::Api::Faux;
        if self.settings.mode == PortraitMode::Off || (scripted && self.settings.mode != PortraitMode::Always) {
            self.pending.lock().unwrap().clear();
            return;
        }
        let mut running = self.running.lock().unwrap();
        running.retain(|h| !h.is_finished());
        if !running.is_empty() {
            return;
        }
        let words: usize = self.pending.lock().unwrap().iter().map(|s| words(&s.text)).sum();
        if words < self.settings.min_words.max(1) {
            return;
        }
        let said = std::mem::take(&mut *self.pending.lock().unwrap());
        let (portrait, model, stream_fn, api_key, retry, session, inspector) = (
            self.portrait.clone(),
            self.model.clone(),
            self.stream_fn.clone(),
            self.api_key.clone(),
            self.retry.clone(),
            self.session.clone(),
            self.inspector.clone(),
        );
        running.push(tokio::spawn(async move {
            let prompt = build_prompt(&portrait.load(), &said);
            let result = match crate::oneshot::complete(
                &model,
                &stream_fn,
                api_key,
                SYSTEM_PROMPT,
                prompt,
                nypi_ai::ThinkingLevel::Off,
                &retry,
                CancellationToken::new(),
            )
            .await
            {
                Ok(reply) => apply(&portrait, &reply, &said, &session),
                Err(error) => Err(error),
            };
            match result {
                Ok(applied) => inspector.record("portrait", json!({ "messages": said.len(), "applied": applied })),
                Err(error) => inspector.record("portrait", json!({ "error": format!("{error:#}") })),
            }
        }));
    }

    /// Wait up to `limit` for a running pass (a process about to exit).
    pub async fn wait(&self, limit: std::time::Duration) {
        let handles = std::mem::take(&mut *self.running.lock().unwrap());
        let deadline = tokio::time::Instant::now() + limit;
        for handle in handles {
            let _ = tokio::time::timeout_at(deadline, handle).await;
        }
    }
}

fn words(text: &str) -> usize {
    // CJK text has no spaces: count its characters, roughly a word each.
    text.split_whitespace().count() + text.chars().filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c)).count()
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() > limit {
        format!("{}…", text.chars().take(limit).collect::<String>())
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(pulse: &str, text: &str) -> Said {
        Said { pulse: pulse.into(), text: text.into() }
    }

    #[test]
    fn keeps_only_changes_the_users_words_support() {
        let dir = tempfile::tempdir().unwrap();
        let portrait = Portrait::new(dir.path().join("portrait.jsonl"));
        let first = [
            said("p-1", "Please keep it SHORT,   and always end with the next step."),
            said("p-2", "我更喜欢用中文讨论设计。"),
        ];
        let reply = r#"Here: {"changes": [
            {"op": "add", "category": "style", "text": "Prefers short answers that end with the next step", "evidence": ["keep it short, and always end with the next step"]},
            {"op": "add", "category": "identity", "text": "Discusses design in Chinese", "evidence": ["更喜欢用中文讨论设计"]},
            {"op": "add", "category": "value", "text": "Values honesty above all", "evidence": ["I value honesty above all"]},
            {"op": "add", "category": "mood", "text": "Is cheerful", "evidence": ["Please keep it"]}
        ]}"#;
        let applied = apply(&portrait, reply, &first, "s-1").unwrap();
        assert_eq!(applied.added.len(), 2, "{applied:?}");
        assert!(applied.rejected[0].contains("not in the user's messages"), "a made-up quote is dropped: {applied:?}");
        assert!(applied.rejected[1].contains("known category"), "{applied:?}");
        let style = portrait.load().into_iter().find(|t| t.category == Category::Style).unwrap();
        assert_eq!((style.evidence[0].pulse.as_str(), style.evidence[0].session.as_str()), ("p-1", "s-1"));

        let second = [said("p-3", "Again: short answers please. Actually, I'm fine with English for design too.")];
        let identity = portrait.load().into_iter().find(|t| t.category == Category::Identity).unwrap();
        let reply = format!(
            r#"{{"changes": [
                {{"op": "add", "category": "style", "text": "prefers short answers that end with the next step", "evidence": ["short answers please"]}},
                {{"op": "remove", "id": "{}", "evidence": ["I'm fine with English for design too"]}},
                {{"op": "revise", "id": "u-missing", "evidence": ["short answers please"]}}
            ]}}"#,
            identity.id
        );
        let applied = apply(&portrait, &reply, &second, "s-2").unwrap();
        assert_eq!((applied.revised, applied.removed.len()), (vec![style.id.clone()], 1), "the same text strengthens");
        assert!(applied.rejected[0].contains("unknown"));
        let traits = portrait.load();
        assert_eq!((traits.len(), traits[0].shown), (1, 2));
        let section = portrait.prompt_section().unwrap();
        assert_eq!(section, "- Prefers short answers that end with the next step (style, shown 2×)");
    }
}
