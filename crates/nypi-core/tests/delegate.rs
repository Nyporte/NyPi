//! Delegation with the faux provider: the agent hands a task to a native or external specialist,
//! the delegation is a child task, and the report comes back with a review instruction.

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{Message, text_of};
use nypi_core::config::Paths;
use nypi_core::ledger::{Ledger, TaskState};
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use nypi_core::specialist::{self, Template};
use serde_json::json;
use tokio::sync::mpsc;

fn runtime(home: &std::path::Path, work: &std::path::Path, faux: &FauxProvider) -> Runtime {
    Runtime::new(RuntimeOptions {
        cwd: work.to_path_buf(),
        paths: Some(Paths { home: home.to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::Ephemeral,
        ..Default::default()
    })
    .unwrap()
}

fn tool_result(messages: &[Message]) -> (String, bool) {
    messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) => Some((text_of(&r.content), r.is_error)),
            _ => None,
        })
        .expect("a tool result")
}

#[tokio::test]
async fn a_native_specialist_runs_as_a_child_task_and_reports_back() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    let dir = specialist::create(&paths, "analyst", Template::Native).unwrap();
    std::fs::write(
        dir.join("profile.json"),
        r#"{"kind":"native","description":"Analyses data carefully.","domain":"data","model":"faux/faux-1","tools":["read"]}"#,
    )
    .unwrap();
    std::fs::write(dir.join("instructions.md"), "Show your numbers.").unwrap();
    std::fs::write(work.path().join("data.txt"), "2 3").unwrap();
    // Requests run in order: the agent delegates, the specialist reads and answers citing what it
    // read, the agent reviews and cites the specialist's source and the delegation itself.
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call(
            "d1",
            "delegate",
            json!({ "specialist": "analyst", "task": "Sum the numbers in data.txt." }),
        ),
        FauxResponse::tool_call("r1", "read", json!({ "path": "data.txt" })),
        FauxResponse::text("The file holds 2 and 3 [t1], so the sum is 5.\n\nDone: summed the numbers."),
        FauxResponse::text(
            "The sum is 5 [t1], from data.txt [analyst:t1]; I made up [t7].\n\nDone: reviewed the analyst's sum.",
        ),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    assert!(
        runtime
            .agent
            .config
            .system_prompt
            .contains("<specialists>\n- analyst (native, data): Analyses data carefully.")
    );
    let prompt = &runtime.agent.config.system_prompt;
    assert!(prompt.contains("not to your own general tools (conduct rule 1)"), "{prompt}");
    let gap = prompt.lines().find(|line| line.trim_start().contains("   "));
    assert!(gap.is_none(), "no run of spaces inside a prompt line: {gap:?}");
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("What is 2 + 3? Ask the analyst.", &tx).await.unwrap();

    let (report, is_error) = tool_result(&outcome.new_messages);
    assert!(!is_error);
    assert!(report.starts_with("[t1] Specialist analyst finished task t-"), "{report}");
    assert!(report.contains("holds 2 and 3 [analyst:t1]"), "the specialist's references are renamed: {report}");
    assert!(
        report.contains("Sources cited by analyst (cite them as written):\n- [analyst:t1] read data.txt"),
        "{report}"
    );
    let kinds: Vec<&str> = outcome
        .citations
        .cited
        .iter()
        .map(|c| match c {
            nypi_core::citations::Citation::Tool { .. } => "tool",
            nypi_core::citations::Citation::Delegated { .. } => "delegated",
            nypi_core::citations::Citation::Unknown { .. } => "unknown",
        })
        .collect();
    assert_eq!(kinds, ["tool", "delegated", "unknown"]);
    assert!(outcome.citations.footer().unwrap().contains("[t7] matches no tool result"));
    assert!(report.contains("Done: summed the numbers.") && report.contains("Review this before you report it"));

    let requests = faux.requests();
    assert_eq!(requests.len(), 4);
    let nested = &requests[1];
    assert!(
        nested.system_prompt.contains("## Specialist: analyst") && nested.system_prompt.contains("Show your numbers.")
    );
    assert!(nested.tools.iter().all(|t| t.name != "delegate"), "specialists cannot delegate");
    assert!(nested.tools.iter().any(|t| t.name == "read"));
    assert!(requests[0].system_prompt.contains("Cite the results your claims rest on"));

    let ledger = Ledger::new(paths.ledger());
    let parent = outcome.task.unwrap();
    let child = ledger.load().into_values().find(|t| t.owner == "analyst").unwrap();
    assert_eq!((child.parent.as_ref(), child.state), (Some(&parent), TaskState::Done));
    let specialist = specialist::find(&paths, work.path(), "analyst").unwrap();
    assert!(specialist.experience().contains("Done: summed the numbers."));
}

#[tokio::test]
async fn an_external_specialist_reports_its_log_and_failures_are_errors() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    let dir = specialist::create(&paths, "cli", Template::Native).unwrap();
    std::fs::write(
        dir.join("profile.json"),
        r#"{"kind":"external","description":"A CLI.","command":["sh","-c","cat >/dev/null; echo 'Failed: the build is broken.'"],"stdin":"{task}"}"#,
    )
    .unwrap();
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("d1", "delegate", json!({ "specialist": "cli", "task": "Build it." })),
        FauxResponse::text("The CLI could not build it.\n\nFailed: the build is broken; I will look at the log."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("Build the project with the CLI.", &tx).await.unwrap();
    let (report, is_error) = tool_result(&outcome.new_messages);
    assert!(is_error, "a failed delegation is an error result");
    assert!(report.contains("Failed: the build is broken.") && report.contains("Full log: "), "{report}");

    let unknown = FauxProvider::new(vec![
        FauxResponse::tool_call("d1", "delegate", json!({ "specialist": "nobody", "task": "x" })),
        FauxResponse::text("Done: noted."),
    ]);
    let mut runtime = self::runtime(home.path(), work.path(), &unknown);
    let outcome = runtime.prompt("Delegate.", &tx).await.unwrap();
    // The schema lists the specialists, so validation names the valid choices.
    let (text, is_error) = tool_result(&outcome.new_messages);
    assert!(is_error && text.contains("cli"), "{text}");
}

#[tokio::test]
async fn without_specialists_there_is_no_delegate_tool() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let runtime = runtime(home.path(), work.path(), &FauxProvider::new(vec![]));
    assert!(runtime.agent.config.tools.iter().all(|t| t.definition().name != "delegate"));
    assert!(!runtime.agent.config.system_prompt.contains("<specialists>"));
}

#[tokio::test]
async fn in_a_conversation_a_specialist_works_in_the_background_and_its_report_comes_back_as_a_pulse() {
    use nypi_core::pulse::{PulseKind, PulsePayload, PulseSource};
    use std::sync::atomic::Ordering;
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    let dir = specialist::create(&paths, "builder", Template::Native).unwrap();
    std::fs::write(
        dir.join("profile.json"),
        r#"{"kind":"external","description":"Builds things.","command":["sh","-c","cat >/dev/null; sleep 0.3; echo 'Done: the build passed.'"],"stdin":"{task}"}"#,
    )
    .unwrap();
    // The call returns before the specialist has finished, so the agent's next reply is the
    // second request, and the specialist (a command here) makes none.
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("d1", "delegate", json!({ "specialist": "builder", "task": "Build the site." })),
        FauxResponse::text("The builder is on it.\n\nContinuing: the builder is building the site."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    // Whoever holds a conversation says so, and listens for reports on its bus.
    let pending = runtime.allow_background_delegation();
    let mut reports = runtime.pulses().subscribe("test", &[PulseKind::Reported]);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("Build the site.", &tx).await.unwrap();

    let (started, is_error) = tool_result(&outcome.new_messages);
    assert!(!is_error && started.contains("Started builder in the background (d-"), "{started}");
    assert_eq!(faux.requests().len(), 2, "the promise stands without a nudge: the specialist is its way back");
    assert!(
        faux.requests()[0]
            .tools
            .iter()
            .any(|tool| tool.name == "delegate" && tool.description.contains("returns at once"))
    );
    // The run ended while the specialist was still working, and waits on it.
    let [delegation] = outcome.wake.as_slice() else { panic!("one way back: {:?}", outcome.wake) };
    assert!(started.contains(delegation.as_str()));
    // Another run that ends promising the report stands on the same specialist, without a nudge.
    faux.push(FauxResponse::text("42.\n\nContinuing: the builder is still building; its report will follow."));
    let meanwhile = runtime.prompt("What is 6 times 7?", &tx).await.unwrap();
    assert_eq!(meanwhile.wake, std::slice::from_ref(delegation), "{:?}", meanwhile.status);
    assert_eq!(faux.requests().len(), 3, "no nudge");
    let ledger = Ledger::new(paths.ledger());
    let parent = outcome.task.clone().unwrap();
    assert_eq!(ledger.get(&parent).unwrap().state, TaskState::Waiting);
    assert_eq!(pending.load(Ordering::SeqCst), 1);

    let pulse = tokio::time::timeout(std::time::Duration::from_secs(20), reports.recv()).await.unwrap().unwrap();
    let PulsePayload::Reported { delegation: reported, specialist, status, text } = &pulse.payload else { panic!() };
    assert_eq!((reported, specialist.as_str()), (delegation, "builder"));
    assert_eq!(status.to_string(), "Done: the build passed.");
    assert!(text.contains(delegation.as_str()) && text.contains("not a message from the user"), "{text}");
    assert!(
        text.contains("You gave it this task:\nBuild the site.") && text.contains("Review this before you report it")
    );
    assert_eq!(pending.load(Ordering::SeqCst), 0);
    // The pulse names the specialist's own task, a child of the task that delegated.
    let PulseSource::TaskEvent { task: child } = &pulse.source else { panic!("{:?}", pulse.source) };
    let child = ledger.load().into_values().find(|task| task.id.0 == *child).expect("the specialist's task");
    assert_eq!((child.parent.as_ref(), child.state), (Some(&parent), TaskState::Done));
}

#[tokio::test]
async fn where_nothing_takes_a_report_delegate_waits_as_before_and_wait_can_be_asked_for() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    let dir = specialist::create(&paths, "builder", Template::Native).unwrap();
    std::fs::write(
        dir.join("profile.json"),
        r#"{"kind":"external","description":"Builds things.","command":["sh","-c","cat >/dev/null; echo 'Done: the build passed.'"],"stdin":"{task}"}"#,
    )
    .unwrap();
    let delegate = |wait: Option<bool>| {
        let mut args = json!({ "specialist": "builder", "task": "Build the site." });
        if let Some(wait) = wait {
            args["wait"] = json!(wait);
        }
        vec![FauxResponse::tool_call("d1", "delegate", args), FauxResponse::text("Built.\n\nDone: the site is built.")]
    };
    // One printed answer, a job: no conversation is there afterwards, so the call waits.
    let faux = FauxProvider::new(delegate(None));
    let mut plain = runtime(home.path(), work.path(), &faux);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = plain.prompt("Build the site.", &tx).await.unwrap();
    assert!(tool_result(&outcome.new_messages).0.contains("Specialist builder finished task t-"));
    assert!(
        !faux.requests()[0].tools.iter().any(|tool| tool.name == "delegate" && tool.description.contains("at once"))
    );

    // In a conversation, the agent can still ask to wait for a result it needs now.
    let faux = FauxProvider::new(delegate(Some(true)));
    let mut conversation = runtime(home.path(), work.path(), &faux);
    let pending = conversation.allow_background_delegation();
    let outcome = conversation.prompt("Build the site.", &tx).await.unwrap();
    assert!(tool_result(&outcome.new_messages).0.contains("Specialist builder finished task t-"));
    assert!(outcome.wake.is_empty());
    assert_eq!(pending.load(std::sync::atomic::Ordering::SeqCst), 0);
}

fn external(paths: &Paths, name: &str, script: &str) {
    let dir = specialist::create(paths, name, Template::Native).unwrap();
    let profile = json!({ "kind": "external", "description": format!("Tries the {name} way."), "command": ["sh", "-c", script], "stdin": "{task}" });
    std::fs::write(dir.join("profile.json"), profile.to_string()).unwrap();
}

/// A race (decision D83): several ways to one answer start together; the first through is
/// reported and stops the rest, a failure is not reported while another may still come
/// through, and the run waits on the first of them.
#[tokio::test]
async fn the_first_probe_through_is_reported_and_the_others_are_stopped() {
    use nypi_core::ledger::{WaitPredicate, WaitToken};
    use nypi_core::pulse::{PulseKind, PulsePayload};
    use std::sync::atomic::Ordering;
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    external(&paths, "broken", "cat >/dev/null; echo 'Failed: the mirror is down.'");
    external(&paths, "quick", "cat >/dev/null; sleep 0.3; echo 'Done: found it in the archive.'");
    external(&paths, "slow", "cat >/dev/null; sleep 30; echo 'Done: found it the long way.'");
    let race = json!({ "race": [
        { "specialist": "broken", "task": "Find the 2019 report via the mirror." },
        { "specialist": "quick", "task": "Find the 2019 report in the archive." },
        { "specialist": "slow", "task": "Find the 2019 report by asking around." },
    ] });
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("d1", "delegate", race),
        FauxResponse::text("Three ways are being tried.\n\nContinuing: waiting for the first to find the report."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let pending = runtime.allow_background_delegation();
    let mut reports = runtime.pulses().subscribe("test", &[PulseKind::Reported]);
    let (tx, _rx) = mpsc::unbounded_channel();
    let started = std::time::Instant::now();
    let outcome = runtime.prompt("Find the 2019 report.", &tx).await.unwrap();
    let (said, failed) = tool_result(&outcome.new_messages);
    assert!(!failed && said.contains("Started a race of 3"), "{said}");
    assert_eq!(outcome.wake.len(), 3);
    // The run waits on the first of them, as the ledger says.
    let task = Ledger::new(paths.ledger()).get(outcome.task.as_ref().unwrap()).unwrap();
    assert_eq!(task.state, TaskState::Waiting);
    let tokens: Vec<WaitToken> = outcome.wake.iter().map(|id| WaitToken::Delegation(id.clone())).collect();
    assert_eq!(task.waits, Some(WaitPredicate::AnyOf { tokens }));

    let pulse = tokio::time::timeout(std::time::Duration::from_secs(20), reports.recv()).await.unwrap().unwrap();
    let PulsePayload::Reported { specialist, text, .. } = &pulse.payload else { panic!() };
    assert_eq!(specialist, "quick");
    assert!(
        text.contains("found it in the archive") && text.contains("the others") && text.contains("were stopped"),
        "{text}"
    );
    // Every probe is over, long before the slow one would have been: it was stopped.
    while pending.load(Ordering::SeqCst) > 0 {
        assert!(started.elapsed() < std::time::Duration::from_secs(20), "a probe was not stopped");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(reports.try_recv().is_none(), "one report: the failure and the stopped probe are not told");
}

#[tokio::test]
async fn a_race_that_every_probe_fails_is_reported_once_with_what_each_said() {
    use nypi_core::pulse::{PulseKind, PulsePayload};
    use std::sync::atomic::Ordering;
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    external(&paths, "first", "cat >/dev/null; echo 'Failed: the mirror is down.'");
    external(&paths, "second", "cat >/dev/null; sleep 0.2; echo 'Failed: the archive has no 2019.'");
    let race = json!({ "race": [
        { "specialist": "first", "task": "Try the mirror." },
        { "specialist": "second", "task": "Try the archive." },
    ] });
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("d1", "delegate", race),
        FauxResponse::text("Two ways are being tried.\n\nContinuing: waiting for the first through."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let pending = runtime.allow_background_delegation();
    let mut reports = runtime.pulses().subscribe("test", &[PulseKind::Reported]);
    let (tx, _rx) = mpsc::unbounded_channel();
    runtime.prompt("Find the 2019 report.", &tx).await.unwrap();
    let pulse = tokio::time::timeout(std::time::Duration::from_secs(20), reports.recv()).await.unwrap().unwrap();
    let PulsePayload::Reported { text, .. } = &pulse.payload else { panic!() };
    assert!(text.starts_with("Every delegation of the race you started has failed"), "{text}");
    assert!(text.contains("the mirror is down") && text.contains("the archive has no 2019"), "{text}");
    while pending.load(Ordering::SeqCst) > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(reports.try_recv().is_none());
}
