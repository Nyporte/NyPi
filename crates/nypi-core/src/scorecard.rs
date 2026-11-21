//! The scorecard (decision D63): how well NyPi served the user in one week, worked out from its
//! own records and nothing else: the task ledger (what ran, how it ended, and the user's verdict
//! on it), the notice outbox (what reached them), and the audit log (how often they stepped in
//! while it worked). The numbers that are judged against a bar rest on the user's verdicts,
//! because "it reported Done" is the agent's word and the question is whether that word holds.
//! A ratio with too few verdicts behind it is reported as not judged rather than as passing.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Local, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ledger::{TaskId, TaskRecord, TaskState, Verdict};
use crate::outbox::Notice;

/// Channels whose tasks nobody asked for at that moment: NyPi started them itself.
const SELF_STARTED: [&str; 3] = ["dmn", "goals", "timer"];
/// A task whose process is gone and that nobody has touched for this long was not picked up by
/// any watchdog: it stopped and nobody was told.
const STOPPED_AFTER_MINUTES: i64 = 30;
/// A notice still neither delivered nor shown after this long did not reach the user.
const UNHEARD_AFTER_MINUTES: i64 = 60;

/// The bars the week is judged against. Silent stops have no setting: the bar is zero
/// (principle B6).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScorecardSettings {
    /// Of the tasks you asked for and rated: the least share that must be good without you
    /// stepping in.
    pub right_without_help: f64,
    /// Of the rated tasks that reported Done: the largest share that may be wrong.
    pub false_done: f64,
    /// Of the rated work NyPi started itself: the least share that must be good.
    pub useful_self_started: f64,
    /// Verdicts a share needs behind it before it is judged at all.
    pub min_rated: u32,
}

impl Default for ScorecardSettings {
    fn default() -> Self {
        ScorecardSettings { right_without_help: 0.8, false_done: 0.05, useful_self_started: 0.5, min_rated: 5 }
    }
}

/// A message the user sent into a running task.
#[derive(Clone, Debug)]
pub struct Steer {
    pub session: String,
    pub at: DateTime<Utc>,
}

fn parse(at: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(at).ok().map(|t| t.with_timezone(&Utc))
}

/// The steering messages in a stretch of audit records: the user's own, and in chat mode the
/// ones the chat module passed on to the worker.
pub fn steers(audit: &[Value]) -> Vec<Steer> {
    audit
        .iter()
        .filter(|record| record["source"] == "pulse")
        .filter_map(|record| {
            let pulse = &record["payload"]["payload"];
            let kind = pulse["kind"].as_str()?;
            let steered = matches!(kind, "userMessage" | "delegated") && pulse["delivery"] == "steer";
            let at = parse(record["ts"].as_str()?)?;
            steered.then(|| Steer { session: record["session"].as_str().unwrap_or_default().to_string(), at })
        })
        .collect()
}

/// One week, from Monday 00:00 local time to the next.
#[derive(Clone, Copy, Debug)]
pub struct Window {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

impl Window {
    /// The week `weeks_back` weeks before the one `now` falls in.
    pub fn week(now: DateTime<Local>, weeks_back: u32) -> Self {
        use chrono::Datelike;
        let today = now.date_naive();
        let monday = today - Duration::days(today.weekday().num_days_from_monday() as i64 + 7 * weeks_back as i64);
        // Midnight exists on every day but a rare clock change; then the first instant after it.
        let start = |date: chrono::NaiveDate| {
            let midnight = date.and_time(NaiveTime::MIN);
            Local
                .from_local_datetime(&midnight)
                .earliest()
                .unwrap_or_else(|| Local.from_utc_datetime(&midnight))
                .with_timezone(&Utc)
        };
        Window { from: start(monday), to: start(monday + Duration::days(7)) }
    }

    fn holds(&self, at: &str) -> bool {
        parse(at).is_some_and(|at| self.from <= at && at < self.to)
    }
}

/// So many of so many.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Ratio {
    pub part: u32,
    pub of: u32,
}

impl Ratio {
    fn count(&mut self, counts: bool) {
        self.of += 1;
        self.part += counts as u32;
    }

    pub fn share(self) -> Option<f64> {
        (self.of > 0).then(|| self.part as f64 / self.of as f64)
    }
}

/// How the week's tasks ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Ended {
    pub done: u32,
    pub blocked: u32,
    pub failed: u32,
    pub cancelled: u32,
    /// Still running, waiting on a schedule, or stopped.
    pub open: u32,
}

/// Every way the user was left not knowing (principle B6).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Silent {
    /// Tasks the user rated silent.
    pub rated: Vec<TaskId>,
    /// Tasks whose process is gone, that nothing resumed or failed, and nobody reported.
    pub stopped: Vec<TaskId>,
    /// Notices no channel took and no front end showed.
    pub unheard: Vec<String>,
}

impl Silent {
    pub fn count(&self) -> usize {
        let tasks: BTreeSet<&TaskId> = self.rated.iter().chain(&self.stopped).collect();
        tasks.len() + self.unheard.len()
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Week {
    pub from: String,
    pub to: String,
    /// Tasks the user gave it, and the days they did so on.
    pub asked: u32,
    pub active_days: u32,
    /// Tasks NyPi started itself: idle passes, goal reviews, schedules.
    pub self_started: u32,
    /// Sub-tasks handed to specialists.
    pub delegated: u32,
    pub ended: Ended,
    /// Interrupted, picked up again by the harness, and finished.
    pub recovered: u32,
    /// Tasks the user sent a message into while they ran.
    pub steered: u32,
    pub silent: Silent,
    pub right_without_help: Ratio,
    pub false_done: Ratio,
    pub useful_self_started: Ratio,
    /// Settled tasks that have a verdict.
    pub rated: Ratio,
    /// Notices that reached the user, of those raised.
    pub notices: Ratio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Judgement {
    Met,
    Missed,
    /// Not enough behind the number to say either way.
    TooFew,
}

/// One judged line of the scorecard.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Measure {
    pub key: &'static str,
    pub name: &'static str,
    pub value: String,
    pub bar: String,
    pub judgement: Judgement,
    /// What is behind the value, or what is missing to judge it.
    pub detail: String,
}

fn percent(share: f64) -> String {
    format!("{:.0}%", share * 100.0)
}

fn ids(tasks: &[TaskId]) -> String {
    tasks.iter().map(|t| t.0.as_str()).collect::<Vec<_>>().join(", ")
}

/// The week `window`, from every task and notice on file and the steering messages of at least
/// that week.
pub fn week(tasks: &[TaskRecord], notices: &[Notice], steers: &[Steer], window: Window, now: DateTime<Utc>) -> Week {
    let mut week = Week {
        from: window.from.to_rfc3339(),
        to: window.to.to_rfc3339(),
        asked: 0,
        active_days: 0,
        self_started: 0,
        delegated: 0,
        ended: Ended::default(),
        recovered: 0,
        steered: 0,
        silent: Silent::default(),
        right_without_help: Ratio::default(),
        false_done: Ratio::default(),
        useful_self_started: Ratio::default(),
        rated: Ratio::default(),
        notices: Ratio::default(),
    };
    let mut days = BTreeSet::new();
    for task in tasks.iter().filter(|task| window.holds(&task.created)) {
        if task.parent.is_some() {
            week.delegated += 1;
            continue;
        }
        let own = SELF_STARTED.contains(&task.origin.as_str());
        if own {
            week.self_started += 1;
        } else {
            week.asked += 1;
            days.extend(parse(&task.created).map(|at| at.with_timezone(&Local).date_naive()));
        }
        match task.state {
            TaskState::Done => week.ended.done += 1,
            TaskState::BlockedOnUser => week.ended.blocked += 1,
            TaskState::Failed => week.ended.failed += 1,
            TaskState::Cancelled => week.ended.cancelled += 1,
            TaskState::Planned | TaskState::Running | TaskState::Stalled | TaskState::Waiting => week.ended.open += 1,
        }
        week.recovered += (task.attempts > 1 && task.state == TaskState::Done) as u32;

        // The run lasted from the task's creation to its last sign of life.
        let (start, end) = (parse(&task.created), parse(&task.heartbeat_at));
        let steered = steers.iter().any(|steer| {
            steer.session == task.session_id
                && start.is_some_and(|s| s <= steer.at)
                && end.is_some_and(|e| steer.at <= e)
        });
        week.steered += steered as u32;

        let quiet_for = end.map_or(0, |end| (now - end).num_minutes());
        if task.is_interrupted() && quiet_for > STOPPED_AFTER_MINUTES {
            week.silent.stopped.push(task.id.clone());
        }
        let verdict = task.rating.as_ref().map(|rating| rating.verdict);
        if verdict == Some(Verdict::Silent) {
            week.silent.rated.push(task.id.clone());
        }
        if task.is_settled() {
            week.rated.count(verdict.is_some());
        }
        let Some(verdict) = verdict else { continue };
        if own {
            week.useful_self_started.count(verdict == Verdict::Good);
        } else {
            week.right_without_help.count(verdict == Verdict::Good && !steered);
        }
        if task.state == TaskState::Done {
            week.false_done.count(verdict == Verdict::Wrong);
        }
    }
    week.active_days = days.len() as u32;
    for notice in notices.iter().filter(|notice| window.holds(&notice.at)) {
        week.notices.count(!notice.is_open());
        let waiting = parse(&notice.at).map_or(0, |at| (now - at).num_minutes());
        if notice.is_open() && waiting > UNHEARD_AFTER_MINUTES {
            week.silent.unheard.push(notice.id.clone());
        }
    }
    week
}

impl Week {
    /// The week against its bars, the one that cannot be bought back first.
    pub fn measures(&self, bars: &ScorecardSettings) -> Vec<Measure> {
        let silent = self.silent.count();
        let ran = self.asked + self.self_started;
        let mut found = Vec::new();
        if !self.silent.rated.is_empty() {
            found.push(format!("you rated silent: {}", ids(&self.silent.rated)));
        }
        if !self.silent.stopped.is_empty() {
            found.push(format!("stopped with nobody told: {}", ids(&self.silent.stopped)));
        }
        if !self.silent.unheard.is_empty() {
            found.push(format!("notices that never reached you: {}", self.silent.unheard.join(", ")));
        }
        let mut measures = vec![Measure {
            key: "silentStops",
            name: "Silent stops",
            value: silent.to_string(),
            bar: "0".into(),
            judgement: match (silent, ran) {
                (0, 0) => Judgement::TooFew,
                (0, _) => Judgement::Met,
                _ => Judgement::Missed,
            },
            detail: if ran == 0 && silent == 0 { "nothing ran".into() } else { found.join(" · ") },
        }];
        let shares = [
            ("rightWithoutHelp", "Right without help", self.right_without_help, bars.right_without_help, true),
            ("falseDone", "Done that was wrong", self.false_done, bars.false_done, false),
            ("usefulSelfStarted", "Useful self-started work", self.useful_self_started, bars.useful_self_started, true),
        ];
        for (key, name, ratio, bar, at_least) in shares {
            let share = ratio.share().unwrap_or(0.0);
            let judged = ratio.of >= bars.min_rated.max(1);
            // Compared as counts, so 4 of 5 meets 80% whatever the rounding.
            let (part, limit) = (ratio.part as f64, bar * ratio.of as f64);
            let met = if at_least { part + 1e-9 >= limit } else { part <= limit + 1e-9 };
            measures.push(Measure {
                key,
                name,
                value: format!("{} of {}", ratio.part, ratio.of),
                bar: format!("{} {}", if at_least { "at least" } else { "at most" }, percent(bar)),
                judgement: match (judged, met) {
                    (false, _) => Judgement::TooFew,
                    (true, true) => Judgement::Met,
                    (true, false) => Judgement::Missed,
                },
                detail: if judged {
                    percent(share)
                } else {
                    format!("{} rated, {} needed to judge", ratio.of, bars.min_rated.max(1))
                },
            });
        }
        measures
    }

    /// Settled tasks still without a verdict.
    pub fn unrated(&self) -> u32 {
        self.rated.of - self.rated.part
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::inspector::AlertLevel;
    use crate::ledger::Rating;

    /// A pid that cannot exist stands for a process that is gone.
    const DEAD: u32 = i32::MAX as u32 - 7;

    fn window() -> Window {
        Window { from: parse("2026-11-09T00:00:00Z").unwrap(), to: parse("2026-11-16T00:00:00Z").unwrap() }
    }

    fn at(day: u32, time: &str) -> String {
        format!("2026-11-{day:02}T{time}:00+00:00")
    }

    fn task(id: &str, origin: &str, state: TaskState, verdict: Option<Verdict>) -> TaskRecord {
        TaskRecord {
            id: TaskId(id.into()),
            goal: "g".into(),
            prompt: "g".into(),
            owner: "lucia".into(),
            origin: origin.into(),
            parent: None,
            session_id: format!("s-{id}"),
            session_path: None,
            cwd: "/".into(),
            remote: None,
            pulse: None,
            created: at(10, "12:00"),
            heartbeat_at: at(10, "12:10"),
            state,
            note: String::new(),
            pid: DEAD,
            attempts: 1,
            open_tools: Vec::new(),
            dispatched_at: None,
            dispatches: 0,
            wake: Vec::new(),
            rating: verdict.map(|verdict| Rating { verdict, note: String::new(), at: at(10, "13:00") }),
            workspace: None,
            waits: None,
            goal_id: None,
            spawned_by_activation: None,
        }
    }

    fn notice(id: &str, at: String, delivered: bool) -> Notice {
        Notice {
            id: id.into(),
            at,
            level: AlertLevel::Warning,
            source: "watchdog".into(),
            text: "x".into(),
            task: None,
            delivered: if delivered { vec!["desktop notification".into()] } else { Vec::new() },
            failed: Vec::new(),
            attempts: 1,
            last_attempt: None,
            seen_by: None,
        }
    }

    fn judgements(week: &Week) -> Vec<(&'static str, Judgement)> {
        week.measures(&ScorecardSettings::default()).into_iter().map(|m| (m.key, m.judgement)).collect()
    }

    #[test]
    fn a_week_is_counted_from_the_ledger_and_judged_on_the_users_verdicts() {
        let now = parse(&at(12, "09:00")).unwrap();
        let mut tasks = vec![
            task("t-1", "cli", TaskState::Done, Some(Verdict::Good)),
            task("t-2", "telegram", TaskState::Done, Some(Verdict::Good)),
            task("t-3", "cli", TaskState::Done, Some(Verdict::Good)),
            task("t-4", "print", TaskState::Done, Some(Verdict::Fixed)),
            task("t-5", "cli", TaskState::Done, Some(Verdict::Wrong)),
            task("t-6", "cli", TaskState::BlockedOnUser, None),
            task("t-7", "dmn", TaskState::Done, Some(Verdict::Good)),
            task("t-8", "timer", TaskState::Failed, None),
            task("t-9", "cli", TaskState::Done, Some(Verdict::Good)),
        ];
        // t-3 was rated good, but the user had to speak into it while it ran.
        let steers = steers(&[
            json!({ "source": "pulse", "session": "s-t-3", "ts": at(10, "12:05"),
                    "payload": { "payload": { "kind": "userMessage", "delivery": "steer", "text": "not that file" } } }),
            json!({ "source": "pulse", "session": "s-t-1", "ts": at(10, "12:05"),
                    "payload": { "payload": { "kind": "userMessage", "delivery": "prompt", "text": "start" } } }),
            json!({ "source": "pulse", "session": "s-t-2", "ts": at(10, "15:00"),
                    "payload": { "payload": { "kind": "userMessage", "delivery": "steer", "text": "after it ended" } } }),
            json!({ "source": "cli", "session": "s-t-1", "ts": at(10, "12:06"), "payload": { "input": "x" } }),
        ]);
        assert_eq!(steers.len(), 2, "prompts and other records are not steering");
        // A sub-task, a task of another week, and one on a second day.
        let mut child = task("t-c", "delegate", TaskState::Done, None);
        child.parent = Some(TaskId("t-1".into()));
        let mut earlier = task("t-old", "cli", TaskState::Done, Some(Verdict::Wrong));
        earlier.created = at(3, "12:00");
        tasks[8].created = at(11, "12:00");
        tasks[8].heartbeat_at = at(11, "12:10");
        tasks[8].attempts = 2;
        tasks.extend([child, earlier]);
        let notices = [notice("n-1", at(10, "12:00"), true), notice("n-0", at(2, "12:00"), false)];

        let week = week(&tasks, &notices, &steers, window(), now);
        assert_eq!((week.asked, week.active_days, week.self_started, week.delegated), (7, 2, 2, 1));
        assert_eq!(week.ended, Ended { done: 7, blocked: 1, failed: 1, cancelled: 0, open: 0 });
        assert_eq!((week.recovered, week.steered), (1, 1));
        assert_eq!(week.right_without_help, Ratio { part: 3, of: 6 }, "good and never steered, of the asked and rated");
        assert_eq!(week.false_done, Ratio { part: 1, of: 7 });
        assert_eq!(week.useful_self_started, Ratio { part: 1, of: 1 });
        assert_eq!((week.rated, week.unrated()), (Ratio { part: 7, of: 9 }, 2));
        assert_eq!(week.notices, Ratio { part: 1, of: 1 });
        assert_eq!(week.silent.count(), 0);
        assert_eq!(
            judgements(&week),
            [
                ("silentStops", Judgement::Met),
                ("rightWithoutHelp", Judgement::Missed),
                ("falseDone", Judgement::Missed),
                ("usefulSelfStarted", Judgement::TooFew),
            ]
        );
        let measures = week.measures(&ScorecardSettings::default());
        assert_eq!((measures[1].value.as_str(), measures[1].detail.as_str()), ("3 of 6", "50%"));
        assert_eq!(measures[3].detail, "1 rated, 5 needed to judge");
    }

    #[test]
    fn every_way_of_not_being_told_counts_as_a_silent_stop() {
        let now = parse(&at(12, "09:00")).unwrap();
        // Rated silent by the user; cut off days ago with nothing done about it (and rated
        // silent as well: one stop, not two); cut off a minute ago, which a watchdog still has
        // time to catch; and waiting on a schedule, which has no process by design.
        let rated = task("t-1", "cli", TaskState::Done, Some(Verdict::Silent));
        let stopped = task("t-2", "cli", TaskState::Running, Some(Verdict::Silent));
        let mut fresh = task("t-3", "cli", TaskState::Running, None);
        fresh.heartbeat_at = at(12, "08:59");
        let waiting = task("t-4", "cli", TaskState::Waiting, None);
        let notices = [
            notice("n-lost", at(11, "08:00"), false),
            notice("n-new", at(12, "08:50"), false),
            notice("n-sent", at(11, "08:00"), true),
        ];
        let week = week(&[rated, stopped, fresh, waiting], &notices, &[], window(), now);
        assert_eq!(week.silent.rated, [TaskId("t-1".into()), TaskId("t-2".into())]);
        assert_eq!(week.silent.stopped, [TaskId("t-2".into())]);
        assert_eq!(week.silent.unheard, ["n-lost"]);
        assert_eq!(week.silent.count(), 3);
        assert_eq!(week.notices, Ratio { part: 1, of: 3 });
        assert_eq!(week.ended.open, 3);
        let silent = &week.measures(&ScorecardSettings::default())[0];
        assert_eq!((silent.value.as_str(), silent.judgement), ("3", Judgement::Missed));
        assert!(silent.detail.contains("stopped with nobody told: t-2") && silent.detail.contains("n-lost"));
    }

    #[test]
    fn nothing_is_judged_on_too_little_and_a_bar_is_met_at_its_edge() {
        let now = parse(&at(12, "09:00")).unwrap();
        let empty = week(&[], &[], &[], window(), now);
        assert!(judgements(&empty).iter().all(|(_, judgement)| *judgement == Judgement::TooFew), "an empty week");
        assert_eq!(empty.measures(&ScorecardSettings::default())[0].detail, "nothing ran");

        // Four good of five is exactly 80%; none wrong of five is within 5%.
        let mut tasks: Vec<TaskRecord> =
            (0..4).map(|i| task(&format!("t-{i}"), "cli", TaskState::Done, Some(Verdict::Good))).collect();
        tasks.push(task("t-4", "cli", TaskState::Done, Some(Verdict::Fixed)));
        let edge = week(&tasks, &[], &[], window(), now);
        assert_eq!(judgements(&edge)[1..3], [("rightWithoutHelp", Judgement::Met), ("falseDone", Judgement::Met)]);
        let strict = ScorecardSettings { right_without_help: 0.9, min_rated: 0, ..Default::default() };
        assert_eq!(edge.measures(&strict)[1].judgement, Judgement::Missed, "the user's own bar applies");
    }

    #[test]
    fn a_week_runs_from_monday_to_monday_in_local_time() {
        use chrono::{Datelike, Timelike, Weekday};
        let now = Local.with_ymd_and_hms(2026, 11, 11, 15, 30, 0).unwrap();
        let this = Window::week(now, 0);
        let last = Window::week(now, 1);
        let from = this.from.with_timezone(&Local);
        assert_eq!((from.weekday(), from.day(), from.hour()), (Weekday::Mon, 9, 0));
        assert_eq!(this.to.with_timezone(&Local).day(), 16);
        assert_eq!(last.to, this.from, "weeks meet without a gap");
        assert!(this.holds(&now.to_rfc3339()) && !last.holds(&now.to_rfc3339()));
        assert!(!this.holds("not a time"));
    }
}
