//! `nypi scorecard`: the week against its bars (decision D63), and the weeks before it for the
//! trend. Reads the ledger, the outbox, and the audit log; writes nothing.

use chrono::{DateTime, Local, Utc};
use nypi_core::config::{Paths, Settings};
use nypi_core::ledger::{Ledger, TaskRecord};
use nypi_core::outbox::Outbox;
use nypi_core::scorecard::{Judgement, Measure, Ratio, ScorecardSettings, Week, Window, steers, week};

/// The weeks, newest first, each with its judged lines.
pub fn weeks(paths: &Paths, bars: &ScorecardSettings, count: u32, now: DateTime<Local>) -> Vec<(Week, Vec<Measure>)> {
    let tasks: Vec<TaskRecord> = Ledger::new(paths.ledger()).load().into_values().collect();
    let notices = Outbox::new(paths.outbox()).load();
    // One audit file per day with activity; a few more than the weeks need does no harm.
    let audit = nypi_core::audit_log::load(&paths.audit(), (count as usize + 1) * 7);
    let steers = steers(&audit);
    (0..count.max(1))
        .map(|back| {
            let week = week(&tasks, &notices, &steers, Window::week(now, back), now.with_timezone(&Utc));
            let measures = week.measures(bars);
            (week, measures)
        })
        .collect()
}

fn day(at: &str) -> String {
    DateTime::parse_from_rfc3339(at)
        .map(|t| t.with_timezone(&Local).format("%-d %b").to_string())
        .unwrap_or_else(|_| "?".into())
}

fn verdict(measure: &Measure) -> String {
    let word = match measure.judgement {
        Judgement::Met => "met",
        Judgement::Missed => "MISSED",
        Judgement::TooFew => "not judged",
    };
    if measure.detail.is_empty() { word.to_string() } else { format!("{word} · {}", measure.detail) }
}

fn plural(count: u32, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The scorecard as text: the newest week in full, earlier ones as one row each.
pub fn render(weeks: &[(Week, Vec<Measure>)], now: DateTime<Utc>) -> String {
    let Some((this, measures)) = weeks.first() else { return String::new() };
    let running = DateTime::parse_from_rfc3339(&this.to).is_ok_and(|to| now < to);
    let mut text =
        format!("NyPi scorecard · week of {}{}\n\n", day(&this.from), if running { " (so far)" } else { "" });
    for measure in measures {
        let bar = format!("bar {}", measure.bar);
        text.push_str(&format!("  {:<26}{:<10}{:<18}{}\n", measure.name, measure.value, bar, verdict(measure)));
    }
    let count = |judgement| measures.iter().filter(|m| m.judgement == judgement).count();
    text.push_str(&format!(
        "\n{} of {} bars met, {} missed, {} not judged.\n",
        count(Judgement::Met),
        measures.len(),
        count(Judgement::Missed),
        count(Judgement::TooFew)
    ));
    text.push_str(&format!(
        "\nYou gave it {} on {}; it started {} itself and handed {} to specialists.\n",
        plural(this.asked, "task", "tasks"),
        plural(this.active_days, "day", "days"),
        this.self_started,
        this.delegated
    ));
    let ended = this.ended;
    text.push_str(&format!(
        "Ended: {} done, {} blocked, {} failed, {} cancelled, {} open. Recovered after an interruption: {}. \
         You stepped in on {}.\n",
        ended.done, ended.blocked, ended.failed, ended.cancelled, ended.open, this.recovered, this.steered
    ));
    text.push_str(&format!("Notices: {} raised, {} reached you.\n", this.notices.of, this.notices.part));
    text.push_str(&format!("Verdicts: {} of {} finished tasks.", this.rated.part, this.rated.of));
    match this.unrated() {
        0 => text.push('\n'),
        left => text.push_str(&format!(" Rate the other {left} with: nypi rate\n")),
    }
    if weeks.len() > 1 {
        text.push_str("\nEarlier weeks\n");
        let header = ["week of", "asked", "self", "silent", "right", "wrong Done", "useful", "rated"];
        let mut rows = vec![header.map(String::from)];
        for (week, _) in &weeks[1..] {
            let ratio = |r: Ratio| format!("{} of {}", r.part, r.of);
            rows.push([
                day(&week.from),
                week.asked.to_string(),
                week.self_started.to_string(),
                week.silent.count().to_string(),
                ratio(week.right_without_help),
                ratio(week.false_done),
                ratio(week.useful_self_started),
                ratio(week.rated),
            ]);
        }
        for row in rows {
            let cells: Vec<String> = row.iter().map(|cell| format!("{cell:<10}")).collect();
            text.push_str(&format!("  {}\n", cells.join("  ").trim_end()));
        }
    }
    text
}

pub fn run(paths: &Paths, settings: &Settings, count: u32, json: bool) -> anyhow::Result<i32> {
    let now = Local::now();
    let weeks = weeks(paths, &settings.scorecard, count, now);
    if json {
        let weeks: Vec<serde_json::Value> = weeks
            .iter()
            .map(|(week, measures)| {
                let mut value = serde_json::to_value(week)?;
                value["measures"] = serde_json::to_value(measures)?;
                Ok(value)
            })
            .collect::<serde_json::Result<_>>()?;
        nypi_hub::outln!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "bars": settings.scorecard, "weeks": weeks }))?
        );
    } else {
        nypi_hub::out!("{}", render(&weeks, now.with_timezone(&Utc)));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use nypi_core::inspector::AlertLevel;
    use nypi_core::ledger::{LedgerEvent, TaskId, TaskState, Verdict};

    use super::*;

    /// A pid that cannot exist stands for a process that is gone.
    const DEAD: u32 = i32::MAX as u32 - 13;

    fn add(ledger: &Ledger, id: &str, origin: &str, at: DateTime<Local>, state: TaskState) -> TaskId {
        let task = TaskId(id.into());
        ledger
            .append(&LedgerEvent::Created {
                task: task.clone(),
                at: at.to_rfc3339(),
                goal: id.into(),
                prompt: id.into(),
                owner: "lucia".into(),
                origin: origin.into(),
                parent: None,
                session_id: format!("s-{id}"),
                session_path: None,
                cwd: "/".into(),
                remote: None,
                pid: DEAD,
                pulse: None,
            })
            .unwrap();
        let ended = (at + chrono::Duration::minutes(10)).to_rfc3339();
        ledger.append(&LedgerEvent::State { task: task.clone(), at: ended, state, note: String::new() }).unwrap();
        task
    }

    #[test]
    fn the_scorecard_is_read_from_the_files_nypi_keeps() {
        use chrono::TimeZone;
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let ledger = Ledger::new(paths.ledger());
        // Wednesday; the week began on Monday the 9th.
        let now = Local.with_ymd_and_hms(2026, 11, 11, 15, 0, 0).unwrap();
        let tuesday = Local.with_ymd_and_hms(2026, 11, 10, 10, 0, 0).unwrap();
        for index in 0..5 {
            let task = add(&ledger, &format!("t-{index}"), "cli", tuesday, TaskState::Done);
            ledger.rate(&task, if index == 0 { Verdict::Wrong } else { Verdict::Good }, "").unwrap();
        }
        let steered = TaskId("t-1".into());
        add(&ledger, "t-idle", "dmn", tuesday, TaskState::Done);
        add(&ledger, "t-cut", "telegram", tuesday, TaskState::Running);
        let last_week = add(&ledger, "t-before", "cli", tuesday - chrono::Duration::days(7), TaskState::Done);
        ledger.rate(&last_week, Verdict::Good, "").unwrap();

        let outbox = Outbox::new(paths.outbox());
        let sent = outbox.queue(AlertLevel::Notice, "run", "Done: x", None).unwrap();
        outbox.record_attempt(&sent, vec!["desktop notification".into()], vec![]).unwrap();
        std::fs::create_dir_all(paths.audit()).unwrap();
        let steer = serde_json::json!({
            "source": "pulse", "session": format!("s-{steered}"), "ts": (tuesday + chrono::Duration::minutes(5)).to_rfc3339(),
            "payload": { "payload": { "kind": "userMessage", "delivery": "steer", "text": "the other folder" } }
        });
        std::fs::write(paths.audit().join("2026-11-10.jsonl"), format!("{steer}\nnot json\n")).unwrap();

        let weeks = weeks(&paths, &ScorecardSettings::default(), 2, now);
        let (this, measures) = &weeks[0];
        assert_eq!((this.asked, this.self_started, this.steered), (6, 1, 1));
        assert_eq!(this.silent.stopped, [TaskId("t-cut".into())], "cut off a day ago and nothing picked it up");
        let judged: Vec<Judgement> = measures.iter().map(|m| m.judgement).collect();
        assert_eq!(judged, [Judgement::Missed, Judgement::Missed, Judgement::Missed, Judgement::TooFew]);
        assert_eq!(weeks[1].0.asked, 1);

        let text = render(&weeks, now.with_timezone(&Utc));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "NyPi scorecard · week of 9 Nov (so far)");
        assert_eq!(
            lines[2],
            "  Silent stops              1         bar 0             MISSED · stopped with nobody told: t-cut"
        );
        assert_eq!(lines[3], "  Right without help        3 of 5    bar at least 80%  MISSED · 60%");
        assert_eq!(lines[4], "  Done that was wrong       1 of 5    bar at most 5%    MISSED · 20%");
        assert!(lines[5].ends_with("not judged · 0 rated, 5 needed to judge"), "{}", lines[5]);
        assert!(text.contains("0 of 4 bars met, 3 missed, 1 not judged."), "{text}");
        assert!(text.contains("You gave it 6 tasks on 1 day; it started 1 itself and handed 0 to specialists."));
        assert!(text.contains("Ended: 6 done, 0 blocked, 0 failed, 0 cancelled, 1 open."), "{text}");
        assert!(text.contains("Verdicts: 5 of 6 finished tasks. Rate the other 1 with: nypi rate"), "{text}");
        let earlier = lines.iter().position(|line| *line == "Earlier weeks").unwrap();
        let row: Vec<&str> = lines[earlier + 2].split_whitespace().collect();
        assert_eq!(row, ["2", "Nov", "1", "0", "0", "1", "of", "1", "0", "of", "1", "0", "of", "0", "1", "of", "1"]);

        let closed = render(&weeks[1..], now.with_timezone(&Utc));
        assert!(closed.starts_with("NyPi scorecard · week of 2 Nov\n"), "a finished week is not 'so far'");
        assert!(!closed.contains("Earlier weeks"));
    }
}
