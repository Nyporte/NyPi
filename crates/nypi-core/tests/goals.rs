//! Goals with the faux provider: open goals reach the prompt, the agent updates one through the
//! goals tool, and writing the file directly is blocked; a goal names its project folder in the
//! vault, and idle time reads the goals by two clocks (decision D85).

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{Message, text_of};
use nypi_core::config::Paths;
use nypi_core::goals::{Change, GoalsFile, Term};
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use serde_json::json;
use tokio::sync::mpsc;

#[tokio::test]
async fn the_agent_sees_and_advances_goals_through_the_tool_only() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    let goals = GoalsFile::new(paths.goals());
    let id = goals
        .add(
            Term::Short,
            "Publish the newsletter",
            &Change { next: Some("draft the intro".into()), ..Default::default() },
            "test",
        )
        .unwrap();
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call(
            "w1",
            "write",
            json!({ "path": paths.goals().display().to_string(), "content": "gone" }),
        ),
        FauxResponse::tool_call(
            "g1",
            "goals",
            json!({ "action": "update", "id": id, "next": "send it to the editor", "reason": "drafted the intro" }),
        ),
        FauxResponse::text("Drafted the intro and recorded the next step.\n\nDone: advanced the newsletter goal."),
    ]);
    let mut runtime = Runtime::new(RuntimeOptions {
        cwd: work.path().to_path_buf(),
        paths: Some(paths.clone()),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::Ephemeral,
        ..Default::default()
    })
    .unwrap();
    let prompt = runtime.agent.config.system_prompt.clone();
    assert!(prompt.contains(&format!("- {id} [short] Publish the newsletter; next: draft the intro")), "{prompt}");
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt(nypi_core::goals::REVIEW_PROMPT, &tx).await.unwrap();

    let results: Vec<(String, bool)> = outcome
        .new_messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) => Some((text_of(&r.content), r.is_error)),
            _ => None,
        })
        .collect();
    assert!(results[0].1 && results[0].0.contains("only through the goals tool"), "{results:?}");
    assert!(!results[1].1, "{results:?}");
    let goal = goals.goals().into_iter().find(|g| g.id == id).unwrap();
    assert_eq!(goal.attribute("next"), Some("send it to the editor"));
    assert!(goals.read().contains("Publish the newsletter"), "the blocked write changed nothing");
}

#[tokio::test]
async fn a_task_that_teaches_something_leaves_a_lesson() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::write(home.path().join("settings.json"), r#"{"reflection": {"mode": "always"}}"#).unwrap();
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("b1", "bash", json!({ "command": "exit 3" })),
        FauxResponse::text("The command failed.\n\nFailed: it exited with 3."),
        FauxResponse::text(
            r#"{"lessons": ["Read a command's exit code before retrying it."], "skill_proposals": [{"skill": "shell", "proposal": "Note that exit 3 means a config error."}]}"#,
        ),
    ]);
    let mut runtime = Runtime::new(RuntimeOptions {
        cwd: work.path().to_path_buf(),
        paths: Some(paths.clone()),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::Ephemeral,
        ..Default::default()
    })
    .unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("Run the setup.", &tx).await.unwrap();
    let summaries = runtime.wait_reflections(std::time::Duration::from_secs(5)).await;
    assert_eq!(summaries.len(), 1, "{summaries:?}");
    assert!(summaries[0].contains("1 lesson(s) added") && summaries[0].contains("1 skill proposal(s)"));
    let self_prompt = std::fs::read_to_string(paths.self_prompt("Lucia")).unwrap();
    assert!(self_prompt.contains("- Read a command's exit code before retrying it."), "{self_prompt}");
    let request = &faux.requests()[2];
    let prompt = text_of(match request.messages.last().unwrap() {
        Message::User(u) => &u.content,
        _ => panic!("the reflection is one user message"),
    });
    assert!(
        prompt.contains("Tool bash exit 3 -> FAILED") && prompt.contains("<outcome>Failed: it exited with 3."),
        "{prompt}"
    );
    assert!(request.tools.is_empty(), "reflection makes no tool calls");
    let journal = std::fs::read_dir(home.path().join("reflections")).unwrap().count();
    assert_eq!(journal, 1);
    assert!(outcome.task.is_some());
}

/// A goal names its campaign in the vault (decision D85): only a folder under `10-projects/`,
/// and only one inside the vault.
#[tokio::test]
async fn a_goal_names_its_project_folder_and_nothing_outside_it() {
    use nypi_agent::{Tool, ToolContext};
    let home = tempfile::tempdir().unwrap();
    let paths = Paths { home: home.path().to_path_buf() };
    let tool = nypi_core::tools::GoalsTool { path: paths.goals(), vault: Some(paths.vault()) };
    let ctx = || {
        let (sink, _) = tokio::sync::mpsc::unbounded_channel();
        ToolContext::new("c".into(), "goals".into(), tokio_util::sync::CancellationToken::new(), sink)
    };
    let added = tool
        .execute(
            json!({ "action": "add", "text": "Launch the shop", "vault": "10-projects/shop", "reason": "asked" }),
            ctx(),
        )
        .await
        .unwrap();
    assert!(!added.is_error, "{:?}", added.content);
    let goal = GoalsFile::new(paths.goals()).goals().pop().unwrap();
    assert_eq!(goal.attribute("vault"), Some("10-projects/shop"));
    assert!(GoalsFile::new(paths.goals()).prompt_section().unwrap().contains("vault: 10-projects/shop"));
    for wrong in ["../outside", "10-workspaces/x", "10-projects/a/b", "10-projects/../../etc"] {
        let refused = tool
            .execute(json!({ "action": "update", "id": goal.id, "vault": wrong, "reason": "try" }), ctx())
            .await
            .unwrap();
        assert!(refused.is_error, "{wrong} was taken");
    }
    // Empty clears it.
    tool.execute(json!({ "action": "update", "id": goal.id, "vault": "", "reason": "done with it" }), ctx())
        .await
        .unwrap();
    assert_eq!(GoalsFile::new(paths.goals()).goals()[0].attribute("vault"), None);
}

/// Idle time by two clocks (decision D85): a deadline near and what blocks it, the goal left
/// longest with the next item of its plan, and the inbox.
#[test]
fn the_goal_digest_reads_both_clocks_and_the_inbox() {
    let home = tempfile::tempdir().unwrap();
    let paths = Paths { home: home.path().to_path_buf() };
    let goals = GoalsFile::new(paths.goals());
    let today = chrono::Local::now().date_naive();
    let due = (today + chrono::Days::new(3)).format("%Y-%m-%d").to_string();
    let urgent = goals
        .add(
            Term::Short,
            "File the taxes",
            &Change { due: Some(due), vault: Some("10-projects/taxes".into()), ..Default::default() },
            "t",
        )
        .unwrap();
    let left = goals
        .add(
            Term::Medium,
            "Learn Japanese",
            &Change { vault: Some("10-projects/japanese".into()), ..Default::default() },
            "t",
        )
        .unwrap();
    // Set long ago.
    let text = std::fs::read_to_string(paths.goals()).unwrap();
    let since = today.format("%Y-%m-%d").to_string();
    std::fs::write(
        paths.goals(),
        text.replacen(&format!("since: {since} <!-- {left}"), &format!("since: 2026-01-01 <!-- {left}"), 1),
    )
    .unwrap();
    let vault = paths.vault();
    for (folder, plan) in [
        ("taxes", "## Workstreams & Milestones\n- [x] gather receipts\n- [!] sign the form (needs the owner)\n"),
        ("japanese", "## Workstreams & Milestones\n- [x] kana\n- [ ] the first 300 kanji\n"),
    ] {
        std::fs::create_dir_all(vault.join("10-projects").join(folder)).unwrap();
        std::fs::write(vault.join("10-projects").join(folder).join("plan.md"), plan).unwrap();
    }
    std::fs::create_dir_all(vault.join("00-inbox")).unwrap();
    std::fs::write(vault.join("00-inbox/link.md"), "a link").unwrap();

    let digest = nypi_core::campaign::goal_digest(&paths, chrono::Utc::now());
    assert!(
        digest.contains(&format!("- {urgent} File the taxes: due in 3 days; blocked: sign the form (needs the owner)")),
        "{digest}"
    );
    let neglect = digest.split("## Left longest").nth(1).unwrap();
    assert!(neglect.find(&left).unwrap() < neglect.find(&urgent).unwrap(), "the longest left comes first: {digest}");
    assert!(neglect.contains("next in its plan: the first 300 kanji"), "{digest}");
    assert!(digest.contains("1 note(s) in 00-inbox/"), "{digest}");
}
