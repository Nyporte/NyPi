//! The memory valve (docs/architecture.md §4.4, decision D32): before a prompt reaches the model,
//! recall from the message with no model call and decide whether to attach what was found. Task
//! messages open it easily; casual ones only for a strong match; in a light-hearted moment a
//! sensitive memory (loss, health, conflict) is never brought up unasked. What it attaches goes
//! on the user message, not the system prompt, so the cached prompt prefix stays stable and the
//! session records exactly what the model saw (A4). The agent can still recall anything itself.
//!
//! While the user types, the same recall runs on the draft (decision D34): its query vector is
//! cached, so the valve does not embed again when the message is sent, and memories that clear
//! the threshold are published as a hint the front end shows under the input.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use nypi_ai::embeddings::{EmbedKind, Embeddings};
use nypi_ai::{ContentBlock, Message, text_of};
use serde::{Deserialize, Serialize};

use crate::memory::{Factors, Hit, Memory, MemoryKind, MemoryStore, RetrievalWeights, rank};
use crate::pulse::{PulseBus, PulseKind, PulsePayload, PulseSource};

const OPEN_TAG: &str = "<recalled-memories>";
const CLOSE_TAG: &str = "</recalled-memories>";
/// Recall on the hot path must not hold up the answer when the embedding endpoint is slow.
const EMBED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// A local model still loading is waited for this long, once per process: without it recall is
/// keywords only and misses memories in another language.
const LOAD_WAIT: std::time::Duration = std::time::Duration::from_secs(15);
/// Extra candidates looked at, so held-back ones do not crowd out the rest.
const EXTRA_CANDIDATES: usize = 8;
/// Drafts shorter than this say too little to recall from.
const MIN_DRAFT_CHARS: usize = 3;
/// Query vectors of recent drafts kept for the message that follows.
const PREFETCH_KEPT: usize = 8;
/// Hints shown under the input, at most.
const HINT_LIMIT: usize = 2;
/// Messages at least this long are also recalled from their keyword form.
const KEYWORD_MIN_WORDS: usize = 8;
/// Words that carry no topic; dropped from the keyword form. Negations stay: "not spicy" is the topic.
const FILLER_WORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "of", "to", "in", "on", "at", "for", "with", "by", "from", "as", "about",
    "into", "is", "are", "was", "were", "be", "been", "am", "it", "its", "this", "that", "these", "those", "there",
    "here", "i", "me", "my", "you", "your", "we", "our", "us", "they", "their", "he", "she", "his", "her", "do",
    "does", "did", "can", "could", "would", "will", "shall", "may", "might", "must", "some", "any", "each", "every",
    "one", "so", "just", "also", "very", "really", "if", "then", "than", "what", "why", "how", "when", "where",
    "which", "who", "please", "s", "d", "ll", "ve", "re", "m",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ValveSettings {
    pub enabled: bool,
    /// Lowest score that opens the valve for a task (a request, a question about doing something).
    pub task_threshold: f64,
    /// Lowest score that opens it in casual conversation.
    pub casual_threshold: f64,
    /// At most this many memories per message.
    pub limit: usize,
}

impl Default for ValveSettings {
    fn default() -> Self {
        ValveSettings { enabled: true, task_threshold: 0.25, casual_threshold: 0.45, limit: 3 }
    }
}

/// What kind of moment a message is, read from its wording.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Situation {
    /// A request or a question about getting something done, rather than chat.
    pub task: bool,
    /// Joking, laughing, playful.
    pub light: bool,
}

const TASK_WORDS: &[&str] = &[
    "please",
    "help",
    "write",
    "fix",
    "find",
    "check",
    "make",
    "create",
    "build",
    "run",
    "plan",
    "schedule",
    "remind",
    "book",
    "draft",
    "send",
    "compare",
    "summarize",
    "summarise",
    "explain",
    "review",
    "update",
    "which",
    "should",
    "remember",
    "recall",
    "need",
    "todo",
    "deadline",
    "buy",
    "order",
    "suggest",
    "recommend",
    "list",
    "give",
    "tell",
    "show",
    "pick",
    "choose",
    "prepare",
    "organize",
    "organise",
];
const TASK_PHRASES: &[&str] = &[
    "can you",
    "could you",
    "would you",
    "help me",
    "how do",
    "how to",
    "how can",
    "what should",
    "do you know",
    "帮我",
    "请",
    "能不能",
    "可以帮",
    "怎么",
    "如何",
    "安排",
    "提醒",
    "计划",
    "查一下",
    "写",
    "改一下",
    "需要",
    "记得",
    "哪个",
    "应该",
    "买",
    "推荐",
    "建议",
    "列出",
    "给我",
];
const LIGHT_WORDS: &[&str] = &["haha", "hahaha", "hehe", "lol", "lmao", "rofl", "xd", "jk", "kidding", "joke", "funny"];
const LIGHT_PHRASES: &[&str] =
    &["哈哈", "嘿嘿", "笑死", "开玩笑", "好玩", "😂", "🤣", "😄", "😆", "😁", "😜", "😝", "😹", "🙃", "😋"];

/// Classify a message by its wording. Crude on purpose: it costs nothing on the hot path, and a
/// wrong guess only moves the threshold, since the agent can always recall for itself.
pub fn situation(text: &str) -> Situation {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let has_word = |list: &[&str]| words.iter().any(|w| list.contains(w));
    let has_phrase = |list: &[&str]| list.iter().any(|p| lower.contains(p));
    let looks_technical = lower.contains('`') || lower.contains("://") || lower.contains(".rs") || lower.contains('/');
    Situation {
        task: has_word(TASK_WORDS) || has_phrase(TASK_PHRASES) || looks_technical || lower.trim_end().ends_with('?'),
        light: has_word(LIGHT_WORDS) || has_phrase(LIGHT_PHRASES),
    }
}

/// The message without its filler words, when it is long enough for filler to matter.
pub fn keyword_form(text: &str) -> Option<String> {
    let words: Vec<&str> = text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    if words.len() < KEYWORD_MIN_WORDS {
        return None;
    }
    let kept: Vec<&str> = words.into_iter().filter(|w| !FILLER_WORDS.contains(&w.to_lowercase().as_str())).collect();
    let form = kept.join(" ");
    (!kept.is_empty() && form != text.trim()).then_some(form)
}

/// A memory the valve attached.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Surfaced {
    pub id: String,
    pub kind: MemoryKind,
    pub text: String,
    pub created: String,
    pub score: f64,
    pub factors: Factors,
}

/// Why a matching memory was not attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HoldReason {
    BelowThreshold,
    /// Sensitive, and the moment is light-hearted.
    Sensitive,
    /// Already attached earlier in the conversation the model still sees.
    AlreadyShown,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Held {
    pub id: String,
    pub score: f64,
    pub reason: HoldReason,
}

/// The valve's decision on one message, kept in the audit log as a pulse.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValveDecision {
    pub situation: Situation,
    pub threshold: f64,
    pub surfaced: Vec<Surfaced>,
    pub held: Vec<Held>,
}

impl ValveDecision {
    pub fn is_open(&self) -> bool {
        !self.surfaced.is_empty()
    }

    /// One line for front ends.
    pub fn summary(&self) -> String {
        if self.is_open() {
            let ids: Vec<&str> = self.surfaced.iter().map(|s| s.id.as_str()).collect();
            format!("recalled {} memor{}: {}", ids.len(), if ids.len() == 1 { "y" } else { "ies" }, ids.join(", "))
        } else {
            format!("memory valve closed ({} held back)", self.held.len())
        }
    }
}

/// Apply the valve's rules to ranked hits, best first.
pub fn decide(
    hits: Vec<Hit>,
    situation: Situation,
    settings: &ValveSettings,
    shown: &HashSet<String>,
) -> ValveDecision {
    let threshold = if situation.task { settings.task_threshold } else { settings.casual_threshold };
    let mut decision = ValveDecision { situation, threshold, surfaced: Vec::new(), held: Vec::new() };
    for hit in hits {
        let reason = if shown.contains(&hit.memory.id) {
            Some(HoldReason::AlreadyShown)
        } else if situation.light && hit.memory.is_sensitive() {
            Some(HoldReason::Sensitive)
        } else if hit.score < threshold {
            Some(HoldReason::BelowThreshold)
        } else if decision.surfaced.len() >= settings.limit {
            continue;
        } else {
            None
        };
        match reason {
            Some(reason) => decision.held.push(Held { id: hit.memory.id, score: hit.score, reason }),
            None => decision.surfaced.push(Surfaced {
                id: hit.memory.id,
                kind: hit.memory.kind,
                text: hit.memory.text,
                created: hit.memory.created,
                score: hit.score,
                factors: hit.factors,
            }),
        }
    }
    decision
}

/// A draft's query vector, computed while the user typed.
struct Prefetched {
    text: String,
    reference: String,
    vector: Vec<f32>,
}

#[derive(Clone)]
pub struct MemoryValve {
    pub store: Arc<MemoryStore>,
    pub embeddings: Option<Arc<Embeddings>>,
    pub weights: RetrievalWeights,
    pub settings: ValveSettings,
    /// Shared by every clone (runtime, chat, the typing recall).
    prefetched: Arc<Mutex<VecDeque<Prefetched>>>,
}

impl MemoryValve {
    pub fn new(
        store: Arc<MemoryStore>,
        embeddings: Option<Arc<Embeddings>>,
        weights: RetrievalWeights,
        settings: ValveSettings,
    ) -> Self {
        MemoryValve { store, embeddings, weights, settings, prefetched: Default::default() }
    }

    /// The query vector for `text`: from a draft typed earlier when it matches, else computed now.
    async fn query_vector(&self, text: &str) -> Option<(String, Vec<f32>)> {
        let text = text.trim();
        let cached = self
            .prefetched
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|p| p.text == text)
            .map(|p| (p.reference.clone(), p.vector.clone()));
        if cached.is_some() {
            return cached;
        }
        let embeddings = self.embeddings.as_ref()?;
        let _ = tokio::time::timeout(LOAD_WAIT, embeddings.ready()).await;
        let vector =
            tokio::time::timeout(EMBED_TIMEOUT, embeddings.embed_one(text, EmbedKind::Query)).await.ok()?.ok()?;
        let mut prefetched = self.prefetched.lock().unwrap();
        prefetched.push_back(Prefetched {
            text: text.to_string(),
            reference: vector.0.clone(),
            vector: vector.1.clone(),
        });
        if prefetched.len() > PREFETCH_KEPT {
            prefetched.pop_front();
        }
        Some(vector)
    }

    fn ranked(&self, memories: &[Memory], text: &str, vector: Option<&(String, Vec<f32>)>) -> Vec<Hit> {
        let query_embedding = vector.map(|(model, v)| (model.as_str(), v.as_slice()));
        let limit = self.settings.limit + EXTRA_CANDIDATES;
        rank(memories, text, limit, query_embedding, &self.weights, chrono::Utc::now())
    }

    /// Hits for `text`, best first. A long message is also ranked by its keyword form and each
    /// memory keeps its better score: the filler of a long sentence dilutes its embedding (an
    /// English request scored 0.23 against a Chinese memory it was about, its keywords 0.30).
    async fn recall(&self, memories: &[Memory], text: &str) -> Vec<Hit> {
        let keywords = keyword_form(text);
        let (vector, keyword_vector) = tokio::join!(self.query_vector(text), async {
            match &keywords {
                Some(keywords) => self.query_vector(keywords).await,
                None => None,
            }
        });
        let mut hits = self.ranked(memories, text, vector.as_ref());
        let Some(keywords) = keywords else { return hits };
        for hit in self.ranked(memories, &keywords, keyword_vector.as_ref()) {
            match hits.iter_mut().find(|h| h.memory.id == hit.memory.id) {
                Some(better) if better.score >= hit.score => {}
                Some(worse) => *worse = hit,
                None => hits.push(hit),
            }
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits
    }

    /// Recall for a draft: what the valve would attach if it were sent now. Nothing is recorded
    /// as used, since the user has not sent it.
    pub async fn preview(&self, draft: &str) -> Vec<Surfaced> {
        if !self.settings.enabled || draft.trim().chars().count() < MIN_DRAFT_CHARS {
            return Vec::new();
        }
        let memories = self.store.snapshot();
        if memories.is_empty() {
            return Vec::new();
        }
        let hits = self.recall(&memories, draft).await;
        let mut decision = decide(hits, situation(draft), &self.settings, &HashSet::new());
        decision.surfaced.truncate(HINT_LIMIT);
        decision.surfaced
    }

    /// Decide for `text`, given the conversation the model will see. Records a recall for each
    /// memory attached (it is being used), not for the ones held back.
    pub async fn consider(&self, text: &str, transcript: &[Message]) -> Option<ValveDecision> {
        if !self.settings.enabled || text.trim().is_empty() {
            return None;
        }
        let memories = self.store.snapshot();
        if memories.is_empty() {
            return None;
        }
        let hits = self.recall(&memories, text).await;
        if hits.is_empty() {
            return None;
        }
        let decision = decide(hits, situation(text), &self.settings, &shown_in(transcript));
        self.store.record_recalls(decision.surfaced.iter().map(|s| s.id.as_str()));
        Some(decision)
    }
}

/// Recall on the user's drafts: each `typing` pulse (the newest one, when several are waiting)
/// gets a preview, published as a `memoryHint` when it changes. The task holds the bus, so only
/// a front end that publishes typing pulses starts it, once, for the life of the process.
pub fn spawn_typing_recall(bus: &PulseBus, valve: MemoryValve) -> tokio::task::JoinHandle<()> {
    let mut typing = bus.subscribe("memory", &[PulseKind::Typing]);
    let bus = bus.clone();
    tokio::spawn(async move {
        let mut last: Vec<String> = Vec::new();
        while let Some(mut pulse) = typing.recv().await {
            while let Some(newer) = typing.try_recv() {
                pulse = newer;
            }
            let PulsePayload::Typing { draft, .. } = &pulse.payload else { continue };
            let memories = valve.preview(draft).await;
            let ids: Vec<String> = memories.iter().map(|m| m.id.clone()).collect();
            if ids != last {
                last = ids;
                bus.publish(
                    PulseSource::module("memory"),
                    Some(pulse.id.clone()),
                    PulsePayload::MemoryHint { memories },
                );
            }
        }
    })
}

/// Add the surfaced memories to the user's message as a block of their own.
pub fn attach(message: &mut Message, decision: &ValveDecision) {
    let Message::User(user) = message else { return };
    if !decision.is_open() {
        return;
    }
    let lines: Vec<String> = decision
        .surfaced
        .iter()
        .map(|s| format!("- {} ({}, {}): {}", s.id, s.kind.label(), s.created.get(..10).unwrap_or_default(), s.text))
        .collect();
    user.content.push(ContentBlock::text(format!(
        "{OPEN_TAG}\nRecalled automatically because they may bear on this message. Use them where they help; do \
         not recite them or mention this note.\n{}\n{CLOSE_TAG}",
        lines.join("\n")
    )));
}

/// Memory ids the valve already attached in this conversation. Rebuilt from the transcript, so
/// a memory compacted away can come back when it matters again.
pub fn shown_in(messages: &[Message]) -> HashSet<String> {
    let mut shown = HashSet::new();
    for message in messages {
        let Message::User(user) = message else { continue };
        let text = text_of(&user.content);
        let Some(start) = text.find(OPEN_TAG) else { continue };
        let block = &text[start..text[start..].find(CLOSE_TAG).map_or(text.len(), |e| start + e)];
        for line in block.lines() {
            if let Some(id) = line.strip_prefix("- ").and_then(|l| l.split_whitespace().next())
                && id.starts_with("m-")
            {
                shown.insert(id.to_string());
            }
        }
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Emotion, Memory, Provenance};

    #[test]
    fn reads_the_situation_from_wording() {
        let s = |t: &str| {
            let s = situation(t);
            (s.task, s.light)
        };
        assert_eq!(s("Can you book the dentist for Friday?"), (true, false));
        assert_eq!(s("帮我安排一下下周的会议"), (true, false));
        assert_eq!(s("Fix crates/nypi-core/src/lib.rs"), (true, false));
        assert_eq!(s("had a lovely walk today"), (false, false));
        assert_eq!(s("haha my cat just fell off the sofa"), (false, true));
        assert_eq!(s("哈哈哈今天太好笑了"), (false, true));
        assert_eq!(s("lol which one should I pick?"), (true, true));
        assert!(!situation("scandal").task, "words match whole, not inside other words");
        assert_eq!(s("Suggest three dinner dishes for my sister's visit."), (true, false));
        assert_eq!(s("给我推荐几道菜"), (true, false));
    }

    #[test]
    fn long_messages_get_a_keyword_form() {
        assert_eq!(
            keyword_form("Suggest three dinner dishes I could cook at home for my sister's visit this weekend.")
                .as_deref(),
            Some("Suggest three dinner dishes cook home sister visit weekend")
        );
        assert_eq!(keyword_form("dinner for my sister"), None, "short messages are their own keywords");
        assert_eq!(
            keyword_form("it is not that I do not like it at all").as_deref(),
            Some("not not like all"),
            "negations stay"
        );
    }

    fn hit(id: &str, score: f64, emotion: Option<Emotion>) -> Hit {
        Hit {
            memory: Memory {
                id: id.into(),
                text: format!("memory {id}"),
                tags: vec![],
                kind: MemoryKind::Fact,
                importance: 0.5,
                provenance: Provenance::default(),
                created: "2026-11-01T00:00:00Z".into(),
                last_recalled: None,
                recall_count: 0,
                emotion,
                sensitive: false,
                recalls: vec![],
                embedding: None,
            },
            score,
            factors: Factors::default(),
        }
    }

    #[test]
    fn opens_by_situation_and_holds_back_by_rule() {
        let settings = ValveSettings::default();
        let grief = Some(Emotion { valence: -0.9, arousal: 0.7 });
        let hits = || vec![hit("m-a", 0.5, grief), hit("m-b", 0.35, None), hit("m-c", 0.1, None)];
        let ids = |d: &ValveDecision| d.surfaced.iter().map(|s| s.id.clone()).collect::<Vec<_>>();
        let none = HashSet::new();

        let task = decide(hits(), Situation { task: true, light: false }, &settings, &none);
        assert_eq!(ids(&task), ["m-a", "m-b"]);
        assert_eq!(task.held, [Held { id: "m-c".into(), score: 0.1, reason: HoldReason::BelowThreshold }]);

        let casual = decide(hits(), Situation { task: false, light: false }, &settings, &none);
        assert_eq!(ids(&casual), ["m-a"], "casual chat needs a strong match");

        let light = decide(hits(), Situation { task: true, light: true }, &settings, &none);
        assert_eq!(ids(&light), ["m-b"]);
        assert_eq!(light.held[0].reason, HoldReason::Sensitive, "no grief in a joke");

        let shown: HashSet<String> = ["m-a".to_string()].into();
        let again = decide(hits(), Situation { task: true, light: false }, &settings, &shown);
        assert_eq!((ids(&again), again.held[0].reason), (vec!["m-b".to_string()], HoldReason::AlreadyShown));

        let one = ValveSettings { limit: 1, ..Default::default() };
        assert_eq!(ids(&decide(hits(), Situation { task: true, light: false }, &one, &none)), ["m-a"]);
    }

    #[test]
    fn attached_blocks_are_found_again() {
        let decision = decide(
            vec![hit("m-a", 0.5, None)],
            Situation { task: true, light: false },
            &Default::default(),
            &HashSet::new(),
        );
        let mut message = Message::user_text("When is it?");
        attach(&mut message, &decision);
        let text = text_of(match &message {
            Message::User(u) => &u.content,
            _ => unreachable!(),
        });
        assert!(text.contains("- m-a (fact, 2026-11-01): memory m-a"), "{text}");
        assert!(text.starts_with("When is it?"), "the user's words come first");
        assert_eq!(shown_in(std::slice::from_ref(&message)), ["m-a".to_string()].into());
    }
}
