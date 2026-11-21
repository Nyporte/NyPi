//! The three-plane benchmark judged on itself (decision D90), with a scripted model for each
//! plane and no network: a lookup chat answers alone wakes nothing; a task that goes through
//! every plane (chat hands it up, the Principal starts it, sends round one back, accepts round
//! two, and has chat tell the user) is counted plane by plane; and each of the four new
//! scenarios can be passed, and is not passed by doing nothing.

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{Context, Message, text_of};
use nypi_eval::{EvalOptions, Metrics, PlaneCounts, ScenarioResult, Suite, catalog, run_scenario};
use serde_json::json;

fn said(message: &Message) -> Option<String> {
    match message {
        Message::User(user) => Some(text_of(&user.content)),
        _ => None,
    }
}

fn last_said(ctx: &Context) -> String {
    ctx.messages.iter().rev().find_map(said).unwrap_or_default()
}

fn opened(ctx: &Context) -> String {
    let opening = ctx.messages.iter().find_map(said).unwrap_or_default();
    nypi_core::principal::activation_text(&opening).to_string()
}

fn after_a_tool(ctx: &Context) -> bool {
    matches!(ctx.messages.last(), Some(Message::ToolResult(_)))
}

fn seen(ctx: &Context) -> String {
    format!("{}\n{:?}", ctx.system_prompt, ctx.messages)
}

fn workspace_in(text: &str) -> String {
    let Some(at) = text.find("10-workspaces/") else { return String::new() };
    let path: String = text[at..].chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '/')).collect();
    path.trim_end_matches('/').to_string()
}

fn base_in(ctx: &Context) -> String {
    let all = seen(ctx);
    let Some(at) = all.find("http://127.0.0.1:") else { return String::new() };
    let port: String = all[at + 17..].chars().take_while(char::is_ascii_digit).collect();
    format!("http://127.0.0.1:{port}")
}

/// A model for every plane, scripted; none of them reaches a network.
fn planes(chat: &FauxProvider, principal: &FauxProvider, worker: &FauxProvider) -> EvalOptions {
    EvalOptions {
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(worker.stream_fn()),
        chat_stream_fn: Some(chat.stream_fn()),
        principal_stream_fn: Some(principal.stream_fn()),
        three_planes: true,
        suite: Suite::Planes,
        ..Default::default()
    }
}

async fn judged(id: &str, options: &EvalOptions) -> ScenarioResult {
    let all = catalog(Suite::All);
    let scenario = all.iter().find(|scenario| scenario.id == id).expect("a scenario of that name");
    run_scenario(scenario, options).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quick_lookup_is_answered_in_chat_and_wakes_nothing() {
    let chat = FauxProvider::answering(|ctx| {
        let results = ctx.messages.iter().filter(|m| matches!(m, Message::ToolResult(_))).count();
        Some(match results {
            0 => FauxResponse::tool_call("r1", "vault_read", json!({ "path": "30-entities/staging-cluster.md" })),
            1 => FauxResponse::tool_call("r2", "asset_manage", json!({ "action": "list" })),
            _ => FauxResponse::text(
                "staging-cluster takes SSH on port 2222 in ap-northeast-1; staging-wildcard-tls expires on 2027-03-15.",
            ),
        })
    });
    // Neither of these may be asked anything: an empty script fails the run if it is.
    let (principal, worker) = (FauxProvider::new(vec![]), FauxProvider::new(vec![]));
    let result = judged("quick_lookup_stays_in_chat", &planes(&chat, &principal, &worker)).await;
    assert!(result.passed, "{result:?}");
    assert_eq!(
        result.planes,
        Some(PlaneCounts { chat_turns: 1, signals_fired: 0, activations: 0, tasks_started: 0, review_rounds: 0 })
    );
    assert_eq!(result.tool_calls, 2, "chat's two lookups");
    assert!(principal.requests().is_empty() && worker.requests().is_empty());

    // The same answer with the Principal woken is not a pass.
    let signalling = FauxProvider::answering(|ctx| {
        Some(if after_a_tool(ctx) {
            FauxResponse::text("Port 2222, ap-northeast-1, and it expires 2027-03-15.")
        } else {
            FauxResponse::tool_call("s1", "signal_principal", json!({ "topic": "Look up staging-cluster" }))
        })
    });
    let principal = FauxProvider::answering(|ctx| {
        Some(if after_a_tool(ctx) {
            FauxResponse::text("Concluded.")
        } else {
            FauxResponse::tool_call(
                "p1",
                "conclude_activation",
                json!({ "disposition": "already_handled", "reason": "chat answered it" }),
            )
        })
    });
    let result = judged("quick_lookup_stays_in_chat", &planes(&signalling, &principal, &worker)).await;
    assert!(!result.passed && result.note.contains("woke the Principal"), "{result:?}");
    assert_eq!(result.planes.as_ref().map(|p| (p.signals_fired, p.activations)), Some((1, 1)));
}

/// The fix that passes the suite, written without running it.
const FIX: &str = "cat > normalize.py <<'EOF'
import sys
text = sys.stdin.buffer.read().decode('utf-8')
for line in text.split('\\n'):
    name = line.strip()
    if name:
        sys.stdout.buffer.write((name + '\\n').encode('utf-8'))
EOF";

#[tokio::test(flavor = "multi_thread")]
async fn a_task_goes_through_every_plane_and_is_counted() {
    let chat = FauxProvider::answering(|ctx| {
        let last = last_said(ctx);
        Some(if last.starts_with("[The Principal asks you to tell the user") {
            let fact = last.split("What they must hear:\n- ").nth(1).and_then(|rest| rest.lines().next());
            FauxResponse::text(fact.unwrap_or("Something happened."))
        } else if after_a_tool(ctx) {
            FauxResponse::text("I'll have it fixed and checked.")
        } else {
            FauxResponse::tool_call(
                "c1",
                "signal_principal",
                json!({ "topic": "Fix normalize.py so ./check.sh passes" }),
            )
        })
    });
    let principal = FauxProvider::answering(|ctx| {
        let opened = opened(ctx);
        Some(if opened.starts_with("[A task settled") {
            let task = opened.split("Task: ").nth(1).and_then(|r| r.split_whitespace().next()).unwrap_or_default();
            match (opened.contains("Sent back so far: 0 time(s)"), after_a_tool(ctx)) {
                (_, true) => FauxResponse::text("Reviewed."),
                (true, false) => FauxResponse::tool_call(
                    "p2",
                    "continue_task",
                    json!({ "task_id": task, "critique": "normalize.py changed but ./check.sh was never run. Run it." }),
                ),
                (false, false) => FauxResponse::tool_call(
                    "p3",
                    "stimulate_chat",
                    json!({
                        "kind": "result",
                        "headline": "normalize.py is fixed",
                        "must_convey": ["normalize.py is fixed and ./check.sh prints ALL_OK."],
                    }),
                ),
            }
        } else if after_a_tool(ctx) {
            FauxResponse::text("Started.")
        } else {
            FauxResponse::tool_call(
                "p1",
                "start_task",
                json!({ "topic": "Fix normalize.py so ./check.sh passes", "done_when": "./check.sh prints ALL_OK" }),
            )
        })
    });
    let worker = FauxProvider::answering(|ctx| {
        let last = last_said(ctx);
        let workspace = workspace_in(&seen(ctx));
        Some(match (after_a_tool(ctx), last.as_str()) {
            (false, l) if l.starts_with("[NyPi harness] Before this task settles") => FauxResponse::tool_call(
                "w2",
                "vault_write",
                json!({
                    "path": format!("{workspace}/report.md"),
                    "content": "# Report\n\n## Contract Check\n- normalize.py rewritten [t1]\n\n## Key Findings & \
                                Deliverables\n- normalize.py\n\n## Falsified Paths & Residual Risks\n- none\n",
                }),
            ),
            (true, l) if l.starts_with("[NyPi harness] Before this task settles") => {
                FauxResponse::text("Report written.\n\nDone: normalize.py is fixed [t1].")
            }
            (false, l) if l.contains("[Review of your report on this task") => {
                FauxResponse::tool_call("w3", "bash", json!({ "command": "./check.sh" }))
            }
            (true, l) if l.contains("[Review of your report on this task") => {
                FauxResponse::text("The suite prints ALL_OK [t3].\n\nDone: ./check.sh passes.")
            }
            (false, _) => FauxResponse::tool_call("w1", "bash", json!({ "command": FIX })),
            (true, _) => FauxResponse::text("Rewrote normalize.py [t1].\n\nDone: normalize.py is fixed."),
        })
    });
    let result = judged("unverified_fix_caught_in_review", &planes(&chat, &principal, &worker)).await;
    assert!(result.passed, "{result:?}");
    assert_eq!(
        result.planes,
        Some(PlaneCounts { chat_turns: 2, signals_fired: 1, activations: 3, tasks_started: 1, review_rounds: 1 }),
        "{result:?}"
    );
    assert_eq!(result.status, "Done");
    let checked = worker.requests().iter().any(|ctx| seen(ctx).contains("[Review of your report on this task"));
    assert!(checked, "round two ran the suite the review asked for");
    let metrics = Metrics::of(std::slice::from_ref(&result));
    let planes = metrics.planes.unwrap();
    assert_eq!((planes.activations, planes.review_rounds_total, planes.answered_in_chat), (3.0, 1, 0));
}

/// A way through each of the four scenarios on a worker alone, as shell commands and a last word.
fn reference(id: &str) -> (Vec<&'static str>, &'static str) {
    match id {
        "ambiguous_target_probed" => (
            vec![
                "cat configs/*.toml",
                "curl -s {base}/alpha/health; curl -s {base}/beta/health",
                "printf 'beta:database_locked\\n' > diagnosis.txt",
            ],
            "beta's database is locked [t2].\n\nDone: diagnosis.txt written.",
        ),
        "unverified_fix_caught_in_review" => {
            (vec![FIX, "./check.sh"], "The suite prints ALL_OK [t2].\n\nDone: normalize.py is fixed.")
        }
        "parallel_checks_one_summary" => (
            vec![
                "curl -s {base}/services/auth; curl -s {base}/services/billing",
                "grep -c ERROR logs/billing.log",
                "printf '{\"degraded\": \"billing\", \"errors\": 5}' > report.json",
            ],
            "billing is degraded with 5 errors [t1] [t2].\n\nDone: report.json written.",
        ),
        "quick_lookup_stays_in_chat" => (
            // On a worker alone the answer is all that is judged.
            vec!["true"],
            "Port 2222, region ap-northeast-1; staging-wildcard-tls expires 2027-03-15.\n\nDone: looked up.",
        ),
        other => panic!("no reference for {other}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn each_planes_scenario_can_be_passed_and_none_passes_by_doing_nothing() {
    let all = catalog(Suite::Planes);
    assert_eq!(
        all.iter().map(|s| s.id).collect::<Vec<_>>(),
        [
            "ambiguous_target_probed",
            "unverified_fix_caught_in_review",
            "parallel_checks_one_summary",
            "quick_lookup_stays_in_chat"
        ]
    );
    assert_eq!(catalog(Suite::All).len(), 16);
    for scenario in &all {
        let (steps, last) = reference(scenario.id);
        let steps: Vec<String> = steps.iter().map(|step| step.to_string()).collect();
        let last = last.to_string();
        let worker = FauxProvider::answering(move |ctx| {
            let done = ctx.messages.iter().filter(|m| matches!(m, Message::ToolResult(_))).count();
            Some(match steps.get(done) {
                Some(step) => FauxResponse::tool_call(
                    &format!("s{done}"),
                    "bash",
                    json!({ "command": step.replace("{base}", &base_in(ctx)) }),
                ),
                None => FauxResponse::text(&last),
            })
        });
        let alone = EvalOptions {
            model: Some("faux/faux-1".into()),
            extra_model: Some(faux_model()),
            stream_fn: Some(worker.stream_fn()),
            ..Default::default()
        };
        let result = run_scenario(scenario, &alone).await.unwrap();
        assert!(result.passed, "the reference way through {} fails: {result:?}", scenario.id);
        assert!(result.planes.is_none());

        let idle = FauxProvider::answering(|_| Some(FauxResponse::text("Done: nothing was needed.")));
        let nothing = EvalOptions {
            model: Some("faux/faux-1".into()),
            extra_model: Some(faux_model()),
            stream_fn: Some(idle.stream_fn()),
            ..Default::default()
        };
        let result = run_scenario(scenario, &nothing).await.unwrap();
        assert!(!result.passed && result.false_done, "{} passes by doing nothing: {result:?}", scenario.id);
    }
}
