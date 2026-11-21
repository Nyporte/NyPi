//! What NyPi keeps (decision D77): the stores a long-lived agent thinks with, below the runtime
//! that uses them. Three tiers, apart on purpose:
//!
//! - `episodic`: single facts, preferences, and commitments, ranked by meaning, importance, and
//!   use (`memories.jsonl`);
//! - `vault` and `index`: Lucia's own Markdown notes (an inbox, task workspaces, playbooks,
//!   entity pages, an archive), found by meaning and words and by the links between them, with
//!   the `tools` she reads and writes them through;
//! - `assets` and `secrets`: a registry of what she looks after that a model may read, and the
//!   secrets themselves, sealed, named only by handle, used through the harness and never shown.
//!
//! `jsonl` is the append-only log and its incremental fold that the stores here, and the
//! runtime's own (ledger, schedules, outbox), are built on. Nothing here knows the runtime,
//! the hub, or an interface.

pub mod assets;
pub mod episodic;
pub mod index;
pub mod jsonl;
pub mod secrets;
pub mod tools;
pub mod vault;
