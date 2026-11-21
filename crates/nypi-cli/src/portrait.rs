//! `nypi portrait`: what Lucia has learned about the user (nypi-core/src/portrait.rs), each trait
//! with the user's own words behind it, and forgetting a trait.

use nypi_core::config::Paths;
use nypi_core::memory::now;
use nypi_core::portrait::{Portrait, PortraitEvent};

pub fn list(paths: &Paths) -> anyhow::Result<i32> {
    let portrait = Portrait::new(paths.portrait());
    let traits = portrait.load();
    if traits.is_empty() {
        nypi_hub::outln!(
            "No portrait yet ({}). Lucia learns it from what you say, as you talk.",
            portrait.path().display()
        );
        return Ok(0);
    }
    nypi_hub::outln!("{} traits, most shown first (nypi portrait show <id> for the words behind one):", traits.len());
    for t in traits {
        nypi_hub::outln!("{}  {:<10}  {}  (shown {}×)", t.id, t.category.label(), t.text, t.shown);
    }
    Ok(0)
}

pub fn show(paths: &Paths, id: &str) -> anyhow::Result<i32> {
    let Some(t) = Portrait::new(paths.portrait()).get(id) else {
        nypi_hub::errln!("No trait {id}.");
        return Ok(1);
    };
    nypi_hub::outln!("{} [{}] {}", t.id, t.category.label(), t.text);
    nypi_hub::outln!("Shown {}× since {}; your words:", t.shown, t.created.get(..10).unwrap_or_default());
    for e in &t.evidence {
        nypi_hub::outln!(
            "  \"{}\"  ({} · session {} · trace {})",
            e.quote,
            e.at.get(..10).unwrap_or_default(),
            e.session,
            e.pulse
        );
    }
    Ok(0)
}

pub fn forget(paths: &Paths, id: &str) -> anyhow::Result<i32> {
    let portrait = Portrait::new(paths.portrait());
    if portrait.get(id).is_none() {
        nypi_hub::errln!("No trait {id}.");
        return Ok(1);
    }
    portrait.append(&PortraitEvent::Removed {
        id: id.to_string(),
        at: now(),
        reason: "forgotten by the user".into(),
        by: "user".into(),
        evidence: Vec::new(),
    })?;
    nypi_hub::outln!("Forgot {id}.");
    Ok(0)
}
