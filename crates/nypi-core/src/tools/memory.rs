//! `memory`: the agent's access to long-term memory (nypi-vault/src/episodic.rs, as `nypi_core::memory`): remember, recall,
//! update, forget. The guard blocks writing the memory log any other way.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nypi_agent::{ExecutionMode, Tool, ToolContext, ToolOutput};
use nypi_ai::ToolDefinition;
use nypi_ai::embeddings::{EmbedKind, Embeddings};
use serde_json::{Value, json};

use crate::ledger::TaskId;
use crate::memory::{
    Emotion, Hit, MemoryEvent, MemoryKind, MemoryStore, NewMemory, Provenance, RetrievalWeights, embedding_text, now,
};

const DEFAULT_RECALL: usize = 8;
const MAX_RECALL: usize = 20;

pub struct MemoryTool {
    pub store: Arc<MemoryStore>,
    pub embeddings: Option<Arc<Embeddings>>,
    pub weights: RetrievalWeights,
    pub agent: String,
    pub session: String,
    pub task: Arc<Mutex<Option<TaskId>>>,
}

#[async_trait]
impl Tool for MemoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "memory".into(),
            description: "Your long-term memory, which outlives sessions. recall: search it (do this before \
                answering anything about the user, the people in their life, past decisions, or earlier work). \
                remember: store a durable fact, preference, decision, person, event, or commitment the user shares, \
                in one self-contained sentence, with tags that will help find it later (synonyms, other languages, \
                names); rate importance, set emotion when it carried feeling (a loss, a fear, a joy), and mark it \
                sensitive when it concerns health, money, loss, or conflict. Recall ranks by relevance first, then \
                importance, emotional intensity, and recent use. Relevant memories are also attached to messages \
                automatically (<recalled-memories>). update or forget a memory that changed or was wrong. Never store passwords, keys, or other \
                secrets."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": { "type": "string", "enum": ["recall", "remember", "update", "forget"] },
                    "query": { "type": "string", "description": "What to look for (recall)" },
                    "limit": { "type": "integer", "description": "How many memories to return (recall, default 8)" },
                    "text": { "type": "string", "description": "The memory, as one self-contained sentence" },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "kind": { "type": "string", "enum": ["fact", "preference", "decision", "event", "person", "commitment"] },
                    "importance": { "type": "number", "description": "0 to 1; 0.5 is ordinary" },
                    "emotion": {
                        "type": "object",
                        "description": "How it felt to the user, when it mattered emotionally (remember, update)",
                        "properties": {
                            "valence": { "type": "number", "description": "-1 painful to 1 joyful" },
                            "arousal": { "type": "number", "description": "0 calm to 1 intense" }
                        }
                    },
                    "sensitive": {
                        "type": "boolean",
                        "description": "Health, money, loss, conflict: never brought up unasked in a light moment"
                    },
                    "id": { "type": "string", "description": "Memory id (update, forget)" },
                    "reason": { "type": "string", "description": "Why it is forgotten (forget)" }
                }
            }),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let text = |key: &str| args[key].as_str().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
        let tags: Option<Vec<String>> =
            args["tags"].as_array().map(|t| t.iter().filter_map(|v| v.as_str().map(str::to_string)).collect());
        let sensitive = args["sensitive"].as_bool();
        let emotion = args["emotion"].is_object().then(|| Emotion {
            valence: args["emotion"]["valence"].as_f64().unwrap_or(0.0),
            arousal: args["emotion"]["arousal"].as_f64().unwrap_or(0.0),
        });
        Ok(match args["action"].as_str().unwrap_or_default() {
            "recall" => {
                let Some(query) = text("query") else { return Ok(ToolOutput::error("recall needs a `query`")) };
                let limit = args["limit"].as_u64().map_or(DEFAULT_RECALL, |n| n as usize).clamp(1, MAX_RECALL);
                let (vector, note) = self.embed(&query, EmbedKind::Query).await;
                let query_embedding = vector.as_ref().map(|(model, v)| (model.as_str(), v.as_slice()));
                let hits = self.store.search(&query, limit, query_embedding, &self.weights);
                let mut out = render(&hits);
                if let Some(note) = note {
                    out.push_str(&format!("\n({note}; matched by keywords only)"));
                }
                ToolOutput::text(out)
            }
            "remember" => {
                let Some(memory) = text("text") else {
                    return Ok(ToolOutput::error("remember needs the memory as `text`"));
                };
                let normalized = memory.to_lowercase();
                if let Some(existing) = self.store.load().into_iter().find(|m| m.text.to_lowercase() == normalized) {
                    return Ok(ToolOutput::text(format!("Already remembered as {}.", existing.id)));
                }
                let tags = tags.unwrap_or_default();
                let kind = args["kind"].as_str().and_then(MemoryKind::parse).unwrap_or_default();
                let importance = args["importance"].as_f64().unwrap_or(0.5);
                let (embedding, note) = self.embed(&embedding_text(&memory, &tags), EmbedKind::Document).await;
                let provenance = Provenance {
                    session: Some(self.session.clone()),
                    task: self.task.lock().unwrap().as_ref().map(|t| t.0.clone()),
                    by: self.agent.clone(),
                };
                let id = self.store.add(NewMemory {
                    text: memory,
                    tags,
                    kind,
                    importance,
                    provenance,
                    emotion,
                    sensitive: sensitive.unwrap_or(false),
                    embedding,
                })?;
                let note = note.map(|n| format!(" ({n}; it is still found by keywords)")).unwrap_or_default();
                ToolOutput::text(format!("Remembered as {id}.{note}"))
            }
            "update" => {
                let Some(id) = text("id") else { return Ok(ToolOutput::error("update needs the memory `id`")) };
                let Some(current) = self.store.get(&id) else {
                    return Ok(ToolOutput::error(format!("No memory {id}")));
                };
                let new_text = text("text");
                let embedding = if new_text.is_some() || tags.is_some() {
                    let text = new_text.as_deref().unwrap_or(&current.text);
                    let all_tags = tags.as_deref().unwrap_or(&current.tags);
                    self.embed(&embedding_text(text, all_tags), EmbedKind::Document).await.0
                } else {
                    None
                };
                self.store.append(&MemoryEvent::Updated {
                    id: id.clone(),
                    at: now(),
                    text: new_text,
                    tags,
                    importance: args["importance"].as_f64(),
                    emotion,
                    sensitive,
                    embedding,
                })?;
                ToolOutput::text(format!("Updated {id}."))
            }
            "forget" => {
                let Some(id) = text("id") else { return Ok(ToolOutput::error("forget needs the memory `id`")) };
                if self.store.get(&id).is_none() {
                    return Ok(ToolOutput::error(format!("No memory {id}")));
                }
                let reason = text("reason").unwrap_or_else(|| "no reason given".into());
                self.store.append(&MemoryEvent::Forgotten { id: id.clone(), at: now(), reason })?;
                ToolOutput::text(format!("Forgot {id}."))
            }
            other => ToolOutput::error(format!("Unknown action: {other}")),
        })
    }
}

impl MemoryTool {
    /// The embedding of `text` with the model that made it, or why there is none.
    async fn embed(&self, text: &str, kind: EmbedKind) -> (Option<(String, Vec<f32>)>, Option<String>) {
        let Some(embeddings) = &self.embeddings else { return (None, None) };
        match embeddings.embed_one(text, kind).await {
            Ok(embedding) => (Some(embedding), None),
            Err(error) => (None, Some(format!("embedding failed: {error:#}"))),
        }
    }
}

fn render(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "No memories match.".into();
    }
    hits.iter()
        .map(|hit| {
            let m = &hit.memory;
            let date = m.created.get(..10).unwrap_or_default();
            let tags = if m.tags.is_empty() { String::new() } else { format!(" (tags: {})", m.tags.join(", ")) };
            let feeling = m
                .emotion
                .filter(|e| e.arousal > 0.0 || e.valence != 0.0)
                .map(|e| format!(" · felt {:+.1}/{:.1}", e.valence, e.arousal))
                .unwrap_or_default();
            let sensitive = if m.is_sensitive() { " · sensitive" } else { "" };
            format!(
                "{} [{} · importance {:.1}{feeling}{sensitive} · {date} · {}] {}{tags}",
                m.id,
                m.kind.label(),
                m.importance,
                hit.factors.explain(),
                m.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
