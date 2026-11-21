//! The four scenarios of the three planes' way of working (decision D90). Each checks one rule
//! of the working method by its outcome on disk and at the scenario's server: a vague request
//! is settled by checking, not by a question back; an edit is checked before it counts as done;
//! two independent checks both happen and end in one answer; and a question a few lookups
//! answer is answered by chat alone. They run on a worker alone too, where the last one checks
//! the answer only.

use crate::server::Hit;
use crate::{Fixture, RunRecord, Scenario, Verdict};

pub fn all() -> Vec<Scenario> {
    vec![
        Scenario {
            id: "ambiguous_target_probed",
            hurdle: "the request names no service; two are configured and only one is failing",
            setup: ambiguous_setup,
            prompt: |_| {
                "The service health check is failing. Find out why and write `<service>:<reason>` on one line to \
                 diagnosis.txt."
                    .into()
            },
            respond,
            check: ambiguous_check,
            small_context: false,
        },
        Scenario {
            id: "unverified_fix_caught_in_review",
            hurdle: "the README shows one easy case; the check suite also tests Unicode spaces and blank lines",
            setup: normalize_setup,
            prompt: |_| "Fix normalize.py so the check suite passes.".into(),
            respond,
            check: normalize_check,
            small_context: false,
        },
        Scenario {
            id: "parallel_checks_one_summary",
            hurdle: "two services to inspect, each live and in its log, for one answer",
            setup: parallel_setup,
            prompt: |f| {
                format!(
                    "Inspect both auth and billing (their live status at {}/services/<name> and their local \
                     logs/<name>.log), and write report.json with {{\"degraded\": \"<service>\", \"errors\": <count>}}: \
                     the service whose live status is degraded, and the number of ERROR lines in its log.",
                    f.base
                )
            },
            respond,
            check: parallel_check,
            small_context: false,
        },
        Scenario {
            id: "quick_lookup_stays_in_chat",
            hurdle: "the answer is in a note and the asset registry: no task is needed",
            setup: lookup_setup,
            prompt: |_| {
                "What SSH port and region does staging-cluster use, and when does staging-wildcard-tls expire?".into()
            },
            respond,
            check: lookup_check,
            small_context: false,
        },
    ]
}

/// The services the scenarios ask about.
fn respond(hit: &Hit) -> (u16, String) {
    match hit.path.as_str() {
        "/alpha/health" => (200, "ok".into()),
        "/beta/health" => (503, "database_locked".into()),
        "/services/auth" => (200, r#"{"service":"auth","status":"ok","latencyMs":41}"#.into()),
        "/services/billing" => (200, r#"{"service":"billing","status":"degraded","latencyMs":930}"#.into()),
        _ => (404, "not found".into()),
    }
}

fn ambiguous_setup(f: &Fixture) -> anyhow::Result<()> {
    f.write("configs/alpha.toml", format!("name = \"alpha\"\nhealth = \"{}/alpha/health\"\n", f.base))?;
    f.write("configs/beta.toml", format!("name = \"beta\"\nhealth = \"{}/beta/health\"\n", f.base))?;
    f.write("README.md", "# Services\n\nEach service's settings are in configs/<name>.toml.\n")
}

fn ambiguous_check(f: &Fixture, record: &RunRecord) -> Verdict {
    let checked = record.hits_at("/beta/health") > 0;
    match f.value("diagnosis.txt").as_str() {
        "beta:database_locked" if checked => Verdict::pass("checked the services and found beta's locked database"),
        "beta:database_locked" => Verdict::fail("the right line, but beta's health endpoint was never asked"),
        "" => Verdict::fail("diagnosis.txt was not written (asked which service instead of checking?)"),
        other => Verdict::fail(format!("diagnosis.txt says {other}; beta answers 503 database_locked")),
    }
}

const NORMALIZE: &str = r#"import sys

# Trims each name, one per line.
text = sys.stdin.buffer.read().decode("utf-8")
for line in text.split("\n"):
    sys.stdout.buffer.write((line.strip(" ") + "\n").encode("utf-8"))
"#;

/// Three cases: plain spaces, Unicode spaces (no-break and ideographic), and blank lines.
const CHECK: &str = r#"#!/bin/sh
fail=0
check() {
  got=$(printf "$2" | python3 normalize.py)
  if [ "$got" != "$3" ]; then echo "FAIL case $1"; fail=1; fi
}
check 1 ' foo ' 'foo'
check 2 '\302\240bar\343\200\200' 'bar'
check 3 'a\n\n  \nb\n' "$(printf 'a\nb')"
[ $fail = 0 ] && echo ALL_OK
exit $fail
"#;

fn normalize_setup(f: &Fixture) -> anyhow::Result<()> {
    f.write("normalize.py", NORMALIZE)?;
    f.script("check.sh", CHECK)?;
    f.write("README.md", "# normalize\n\nTrims names: `printf ' foo ' | python3 normalize.py` prints `foo`.\n")
}

fn normalize_check(f: &Fixture, _: &RunRecord) -> Verdict {
    if f.read("check.sh") != CHECK {
        return Verdict::fail("check.sh was changed");
    }
    match f.sh("./check.sh") {
        (true, printed) if printed.contains("ALL_OK") => Verdict::pass("every case of the suite passes"),
        (_, printed) => Verdict::fail(format!("the suite still fails: {}", printed.trim().replace('\n', "; "))),
    }
}

fn parallel_setup(f: &Fixture) -> anyhow::Result<()> {
    f.write("logs/auth.log", "INFO start\nERROR token expired\nINFO ok\nERROR token expired\nINFO ok\n")?;
    f.write(
        "logs/billing.log",
        "INFO start\nERROR db timeout\nERROR db timeout\nWARN slow\nERROR db timeout\nERROR card declined\nINFO ok\n\
         ERROR db timeout\n",
    )
}

fn parallel_check(f: &Fixture, record: &RunRecord) -> Verdict {
    let both = record.hits_at("/services/auth") > 0 && record.hits_at("/services/billing") > 0;
    let text = f.read("report.json");
    let Ok(report) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Verdict::fail(if text.is_empty() {
            "report.json was not written".into()
        } else {
            format!("report.json is not JSON: {text}")
        });
    };
    match (report["degraded"].as_str(), report["errors"].as_u64()) {
        (Some("billing"), Some(5)) if both => Verdict::pass("both services checked; billing is degraded with 5 errors"),
        (Some("billing"), Some(5)) => Verdict::fail("the right answer, but not both live statuses were read"),
        _ => Verdict::fail(format!("report.json says {report}; billing is degraded with 5 ERROR lines")),
    }
}

fn lookup_setup(f: &Fixture) -> anyhow::Result<()> {
    let vault = f.home.join("vault/30-entities");
    std::fs::create_dir_all(&vault)?;
    std::fs::write(
        vault.join("staging-cluster.md"),
        "---\ntitle: staging-cluster\nkind: entity\n---\n# staging-cluster\n\nThe staging cluster. SSH listens on port \
         2222; it runs in region ap-northeast-1.\n",
    )?;
    nypi_vault::assets::AssetRegistry::new(f.home.join("assets.jsonl"))
        .upsert(nypi_vault::assets::AssetChange {
            kind: Some(nypi_vault::assets::AssetKind::Deliverable),
            name: Some("staging-wildcard-tls".into()),
            identifier: Some("*.staging.example.com".into()),
            renews_at: Some(Some("2027-03-15".into())),
            notes: Some("The wildcard certificate of the staging cluster.".into()),
            ..Default::default()
        })
        .map_err(anyhow::Error::msg)?;
    Ok(())
}

fn lookup_check(_: &Fixture, record: &RunRecord) -> Verdict {
    let missing: Vec<&str> =
        ["2222", "ap-northeast-1", "2027-03-15"].into_iter().filter(|fact| !record.answer.contains(fact)).collect();
    if !missing.is_empty() {
        return Verdict::fail(format!("the answer lacks {}", missing.join(", ")));
    }
    match &record.planes {
        Some(planes) if planes.activations > 0 || planes.tasks_started > 0 => Verdict::fail(format!(
            "right, but chat woke the Principal ({} activation(s), {} task(s)) for a lookup",
            planes.activations, planes.tasks_started
        )),
        Some(_) => Verdict::pass("chat answered from its own lookups"),
        None => Verdict::pass("answered the three facts"),
    }
}
