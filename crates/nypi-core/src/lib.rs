//! NyPi coding layer: tools, sessions, compaction, system prompt, skills, settings, Inspector, pulse bus.

pub mod active_tasks;
pub mod audit_log;
pub mod board_link;
pub mod briefing;
pub mod campaign;
pub mod chat;
pub mod citations;
pub mod compaction;
pub mod config;
pub mod delegate;
pub mod device;
pub mod dmn;
pub mod evidence;
pub mod export_html;
pub mod goals;
pub mod html;
pub mod inspector;
pub mod introspect;
/// The append-only logs and their folds live with the stores (`nypi-vault`); the runtime's own
/// stores use them from here as before.
pub use nypi_vault::jsonl;
pub mod ledger;
pub mod mcp;
/// Episodic memory, kept in `nypi-vault` with the other stores (decision D77).
pub use nypi_vault::episodic as memory;
pub mod oneshot;
pub mod outbox;
pub mod place;
pub mod portrait;
pub mod principal;
pub mod prompt;
pub mod protected;
pub mod pulse;
pub mod reflection;
pub mod retry;
pub mod runtime;
pub mod sandbox;
pub mod schedule;
pub mod scorecard;
pub mod session;
pub mod skills;
pub mod specialist;
pub mod status;
pub mod task_pool;
pub mod thinking;
pub mod tools;
pub mod trace;
pub mod trust;
pub mod update;
pub mod valve;
pub mod watchdog;
pub mod worker;
