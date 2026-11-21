//! The briefing pack (decision D88): what the Principal is handed before an activation's first
//! model call, looked up by what woke it, by words, without waiting. The owner's request gets
//! the matching campaign's plan whole, the matching chunks of entity notes with the notes one
//! link away, what is remembered, and the assets it names by handle; a review gets the plan of
//! the goal its task served, the task's findings, and the last chat turns; idle time gets the
//! inbox and the budget left.

use std::sync::Arc;

use nypi_core::briefing::{Sources, pack};
use nypi_core::config::Paths;
use nypi_core::principal::{Settled, Trigger};
use nypi_core::task_pool::TaskBrief;
use nypi_vault::vault::WriteMode;

fn home() -> (tempfile::TempDir, Paths, Arc<nypi_vault::tools::VaultStore>) {
    let home = tempfile::tempdir().unwrap();
    let paths = Paths { home: home.path().to_path_buf() };
    let vault = nypi_vault::tools::VaultStore::open(&paths.vault(), None).unwrap();
    std::fs::write(
        paths.goals(),
        "## Short term\n- [ ] Launch the Atlas shop · due: 2026-12-01 · vault: 10-projects/atlas-shop <!-- g-a71a50 -->\n",
    )
    .unwrap();
    vault
        .write(
            "10-projects/atlas-shop/plan.md",
            "# Atlas shop\n\n## Workstreams & Milestones\n- [~] the checkout page\n- [!] the domain (the owner pays)\n",
            WriteMode::Replace,
        )
        .unwrap();
    vault
        .write(
            "30-entities/atlas.md",
            "# Atlas\n\n## Renewal\nThe Atlas contract renews on 2027-03-01; see [[stripe]].\n\n## History\nSigned in 2024.\n",
            WriteMode::Replace,
        )
        .unwrap();
    vault.write("30-entities/stripe.md", "# Stripe\n\nPayments for [[atlas]].\n", WriteMode::Replace).unwrap();
    (home, paths, vault)
}

#[test]
fn the_owners_request_is_briefed_with_its_plan_notes_memories_and_assets() {
    let (_home, paths, vault) = home();
    let store = Arc::new(nypi_vault::episodic::MemoryStore::new(paths.memory()));
    store
        .add(nypi_vault::episodic::NewMemory {
            text: "The owner wants the Atlas checkout in euros".into(),
            ..Default::default()
        })
        .unwrap();
    store
        .add(nypi_vault::episodic::NewMemory {
            text: "Atlas: a painful dispute".into(),
            sensitive: true,
            ..Default::default()
        })
        .unwrap();
    let valve =
        nypi_core::valve::MemoryValve::new(store, None, Default::default(), nypi_core::valve::ValveSettings::default());
    nypi_vault::assets::AssetRegistry::new(paths.assets())
        .upsert(nypi_vault::assets::AssetChange {
            kind: Some(nypi_vault::assets::AssetKind::Account),
            name: Some("Atlas billing".into()),
            identifier: Some("billing@atlas.example".into()),
            secret_refs: Some(vec!["cred://atlas/billing".into()]),
            ..Default::default()
        })
        .unwrap();
    let dmn = nypi_core::dmn::DmnSettings::default();
    let sources = Sources { paths: &paths, vault: Some(&vault), valve: Some(&valve), dmn: &dmn };
    let trigger = Trigger::UserSignal {
        topic: "When does the Atlas contract renew, and is the shop's checkout ready?".into(),
        urgency: "normal".into(),
        conversation: "Owner: when does atlas renew?".into(),
        gathered: String::new(),
        focus: None,
    };
    let briefed = pack(&sources, &trigger, &[], chrono::Utc::now());
    let kinds: Vec<&str> = briefed.items.iter().map(|item| item.kind.as_str()).collect();
    assert_eq!(kinds, ["plan", "entity", "entity", "memory", "asset"], "{briefed:#?}");
    let plan = &briefed.items[0];
    assert_eq!(plan.path.as_deref(), Some("10-projects/atlas-shop/plan.md"));
    assert!(plan.text.contains("- [!] the domain (the owner pays)"), "the plan whole");
    let atlas = briefed.items.iter().find(|item| item.path.as_deref() == Some("30-entities/atlas.md")).unwrap();
    assert!(atlas.text.contains("renews on 2027-03-01") && !atlas.text.contains("Signed in 2024"), "the chunk only");
    assert_eq!(atlas.links, ["30-entities/stripe.md"], "the note one link away");
    let memory = &briefed.items[3].text;
    assert!(memory.contains("checkout in euros") && !memory.contains("dispute"), "nothing sensitive: {memory}");
    assert!(briefed.items[4].text.contains("cred://atlas/billing"));
    // Made from what is on disk and in memory, by words: no model, no network, no wait.
    assert!(briefed.took_ms < 200, "{} ms", briefed.took_ms);
    let shown = briefed.render().unwrap();
    assert!(
        shown.starts_with("<briefing_pack>\nLooked up for this activation in") && shown.ends_with("</briefing_pack>")
    );
    assert!(shown.contains("Linked: [[30-entities/stripe.md]]"), "{shown}");
}

#[test]
fn a_review_and_idle_time_are_briefed_for_what_they_are() {
    let (_home, paths, vault) = home();
    std::fs::create_dir_all(paths.chat_turns().parent().unwrap()).unwrap();
    std::fs::write(
        paths.chat_turns(),
        format!(
            "{}\n",
            serde_json::json!({ "turnId": "c-1", "at": "2026-11-19T09:00:00Z", "origin": "userSay",
                                "userText": "is the checkout done?", "replyText": "A task is on it." })
        ),
    )
    .unwrap();
    let dmn = nypi_core::dmn::DmnSettings::default();
    let sources = Sources { paths: &paths, vault: Some(&vault), valve: None, dmn: &dmn };
    let settled = Settled {
        task: "t-1".into(),
        ledger_task: "t-1".into(),
        round: 0,
        workspace: Some("10-workspaces/261119-checkout".into()),
        brief: TaskBrief { goal: Some("g-a71a50".into()), topic: "Build the checkout".into(), ..Default::default() },
        status: "Done: built".into(),
        answer: "Built it.".into(),
        evidence: String::new(),
    };
    let dossier = nypi_core::evidence::Dossier {
        findings: Some("- Verified: the cart totals match [t2]".into()),
        ..Default::default()
    };
    let briefed = pack(&sources, &Trigger::TaskSettled(Box::new(settled)), &[dossier], chrono::Utc::now());
    let kinds: Vec<&str> = briefed.items.iter().map(|item| item.kind.as_str()).collect();
    assert_eq!(kinds, ["plan", "findings", "conversation"], "{briefed:#?}");
    assert!(briefed.items[2].text.contains("User: is the checkout done?\nYou: A task is on it."));

    vault.write("00-inbox/2611-invoice.md", "An invoice.", WriteMode::Replace).unwrap();
    let idle = Trigger::Idle { why: "idle pass".into(), digest: "Nothing is due.".into() };
    let briefed = pack(&sources, &idle, &[], chrono::Utc::now());
    let kinds: Vec<&str> = briefed.items.iter().map(|item| item.kind.as_str()).collect();
    assert_eq!(kinds, ["digest", "inbox", "budget"], "{briefed:#?}");
    assert!(briefed.items[0].text.contains("g-a71a50"), "the goals by their clocks");
    assert!(briefed.items[1].text.contains("00-inbox/2611-invoice.md"));
    assert!(briefed.items[2].text.contains(&format!("of {} idle passes", dmn.max_passes_per_day)));
}
