//! The Principal's runner: activations queued by priority, held and merged reviews, the one
//! that runs, suspension and resumption, the four rings of its context (the system prompt, the
//! situational radar, the briefing pack, its transcript), the disposition nudge, and how each
//! ends in the decision journal.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nypi_agent::{Agent, AgentEvent};
use nypi_ai::{ContentBlock, Message, StopReason};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::config::Paths;
use crate::pulse::PulseId;
use crate::schedule::ScheduleStore;

use super::*;

/// Something that woke the Principal, not taken up yet.
pub(super) struct Wake {
    pub(super) id: String,
    pub(super) trigger: Trigger,
    /// The pulse that led to it (the user's message), so what it starts traces back to it.
    pub(super) cause: Option<PulseId>,
}

/// An activation, waiting, running, or suspended.
pub(super) struct Activation {
    pub(super) id: String,
    pub(super) priority: Priority,
    pub(super) trigger: Trigger,
    pub(super) topic: String,
    pub(super) created: String,
    /// Its transcript: every completed step.
    pub(super) messages: Vec<Message>,
    pub(super) started: bool,
    pub(super) cause: Option<PulseId>,
    pub(super) steps: Arc<AtomicUsize>,
    /// The journal's last entry when it was suspended.
    pub(super) at_decision_seq: u64,
    pub(super) tokens: u64,
    pub(super) cost: f64,
    pub(super) work: Arc<Mutex<Touched>>,
}

/// What an activation touched, for its journal entry, and what it did about what woke it.
#[derive(Default)]
pub(super) struct Touched {
    pub(super) tasks: Vec<String>,
    pub(super) goals: Vec<String>,
    pub(super) stimulated: bool,
    /// It started, steered, cancelled, or sent back a task.
    pub(super) tasks_acted: bool,
    /// It changed one of the stores (vault, goals, memory, assets, schedules).
    pub(super) stores_changed: bool,
    /// `conclude_activation`: the disposition and why.
    pub(super) concluded: Option<(String, String)>,
}

impl Touched {
    /// Whether it disposed of what woke it.
    pub(super) fn disposed(&self) -> bool {
        self.stimulated || self.tasks_acted || self.stores_changed || self.concluded.is_some()
    }

    pub(super) fn disposition(&self) -> String {
        if let Some((disposition, reason)) = &self.concluded {
            return format!("{disposition}: {reason}");
        }
        let did: Vec<&str> = [
            (self.stimulated, "stimulated_chat"),
            (self.tasks_acted, "tasks_touched"),
            (self.stores_changed, "stores_changed"),
        ]
        .into_iter()
        .filter_map(|(done, name)| done.then_some(name))
        .collect();
        did.join(", ")
    }
}

/// What the harness says to an activation that stopped having disposed of nothing.
pub(super) const DISPOSITION_NUDGE: &str = "[NyPi harness] You ended this activation without dispatching or updating any task, \
changing a store, sending stimulate_chat, or calling conclude_activation with a reason. Complete the required action \
so the user's request never drops silently: do what it needs, or call conclude_activation (already_handled, \
waiting_on_running_task, noted_only) with why.";

impl Activation {
    pub(super) fn of(wake: Wake) -> Activation {
        // A review is about its task from the start.
        let work: Arc<Mutex<Touched>> = Arc::default();
        for settled in wake.trigger.settled_tasks() {
            work.lock().unwrap().tasks.push(settled.task.clone());
        }
        Activation {
            priority: wake.trigger.priority(),
            topic: wake.trigger.topic(),
            id: wake.id,
            trigger: wake.trigger,
            created: now(),
            messages: Vec::new(),
            started: false,
            cause: wake.cause,
            steps: Arc::default(),
            at_decision_seq: 0,
            tokens: 0,
            cost: 0.0,
            work,
        }
    }

    pub(super) fn view(&self) -> ActivationView {
        ActivationView {
            id: self.id.clone(),
            priority: self.priority,
            kind: self.trigger.kind().into(),
            topic: self.topic.clone(),
            steps: self.steps.load(Ordering::SeqCst),
            created: self.created.clone(),
        }
    }
}

pub(super) struct Runner {
    pub(super) parts: PrincipalParts,
    /// Taken out while an activation runs, so the rest of the executive stays usable.
    pub(super) agent: Option<Agent>,
    pub(super) logs: Logs,
    pub(super) journal: DecisionJournal,
    pub(super) status: Arc<Mutex<PrincipalStatus>>,
    pub(super) current: CurrentSlot,
    /// Waiting, highest priority first, then oldest first.
    pub(super) queued: Vec<Activation>,
    /// The top of the stack last.
    pub(super) suspended: Vec<Activation>,
    /// Settlements of `batch_unless_failed` tasks waiting for their siblings, until when.
    pub(super) held: Vec<(tokio::time::Instant, Activation)>,
}

impl Runner {
    pub(super) async fn run(mut self, mut waking: mpsc::UnboundedReceiver<Wake>) {
        let mut open = true;
        loop {
            while let Ok(wake) = waking.try_recv() {
                self.enqueue(wake);
            }
            self.release_held(!open);
            let Some(activation) = self.next() else {
                if !open && self.held.is_empty() {
                    break;
                }
                self.publish(None);
                let until = self.held.iter().map(|(until, _)| *until).min();
                tokio::select! {
                    wake = waking.recv(), if open => match wake {
                        Some(wake) => self.enqueue(wake),
                        None => open = false,
                    },
                    _ = tokio::time::sleep_until(until.unwrap_or_else(tokio::time::Instant::now)), if until.is_some() => {}
                }
                continue;
            };
            self.activate(activation, &mut waking, &mut open).await;
        }
    }

    /// Take a wake into the queue, by priority, as long as there is room.
    pub(super) fn enqueue(&mut self, wake: Wake) {
        let activation = Activation::of(wake);
        self.logs.event(
            &activation.id,
            &ActivationEvent::Created {
                at: now(),
                priority: activation.priority,
                topic: activation.topic.clone(),
                trigger: Box::new(activation.trigger.clone()),
            },
        );
        self.index(&activation, "created", "");
        // A task's Done that goes with others still working waits for them (decision D87).
        if let Trigger::TaskSettled(settled) = &activation.trigger
            && settled.brief.review_policy == crate::task_pool::ReviewPolicy::BatchUnlessFailed
            && settled.done()
            && self.parts.pool.siblings_working(&settled.task, &settled.brief)
        {
            self.held.push((tokio::time::Instant::now() + self.parts.batch_hold, activation));
            self.publish(None);
            return;
        }
        self.insert(activation);
    }

    /// Held settlements whose siblings have settled, or whose hold ran out, go into the queue.
    /// `all` lets every one go (the executive is shutting down).
    pub(super) fn release_held(&mut self, all: bool) {
        let now = tokio::time::Instant::now();
        let mut at = 0;
        while at < self.held.len() {
            let (until, activation) = &self.held[at];
            let ready = all
                || *until <= now
                || activation
                    .trigger
                    .settled_tasks()
                    .iter()
                    .all(|settled| !self.parts.pool.siblings_working(&settled.task, &settled.brief));
            if ready {
                let (_, activation) = self.held.remove(at);
                self.insert(activation);
            } else {
                at += 1;
            }
        }
    }

    pub(super) fn insert(&mut self, activation: Activation) {
        if self.queued.len() >= self.parts.max_queued.max(1) {
            // Full: idle time gives way first, then the newest event.
            let drop_at = self
                .queued
                .iter()
                .rposition(|waiting| waiting.priority == Priority::P2)
                .or_else(|| (activation.priority != Priority::P0).then_some(usize::MAX));
            match drop_at {
                Some(usize::MAX) => {
                    self.finish_unrun(&activation, "dropped: the queue was full");
                    return;
                }
                Some(at) => {
                    let dropped = self.queued.remove(at);
                    self.finish_unrun(&dropped, "dropped: the queue was full");
                }
                None => {}
            }
        }
        let at = self.queued.iter().position(|waiting| waiting.priority > activation.priority);
        self.queued.insert(at.unwrap_or(self.queued.len()), activation);
        self.publish(None);
    }

    pub(super) fn finish_unrun(&self, activation: &Activation, why: &str) {
        self.logs
            .event(&activation.id, &ActivationEvent::Finished { at: now(), summary: why.into(), tokens: 0, cost: 0.0 });
        self.index(activation, "finished", why);
        (self.parts.finished)(Decision {
            activation_id: activation.id.clone(),
            at: now(),
            priority: activation.priority.label().into(),
            trigger_kind: activation.trigger.kind().into(),
            topic: activation.topic.clone(),
            summary: why.into(),
            ..Default::default()
        });
    }

    /// What runs next: a waiting one of higher priority than the top of the suspended stack,
    /// else that top, else the first waiting. A settlement taken from the queue takes the
    /// waiting settlements that go with it along, as one review (decision D87).
    pub(super) fn next(&mut self) -> Option<Activation> {
        let waiting = self.queued.first().map(|activation| activation.priority);
        let top = self.suspended.last().map(|activation| activation.priority);
        let first = match (waiting, top) {
            (Some(waiting), Some(top)) if waiting < top => self.queued.remove(0),
            (_, Some(_)) => return self.suspended.pop(),
            (Some(_), None) => self.queued.remove(0),
            (None, None) => return None,
        };
        Some(self.coalesce(first))
    }

    pub(super) fn coalesce(&mut self, mut first: Activation) -> Activation {
        let Trigger::TaskSettled(lead) = &first.trigger else { return first };
        let mut tasks = vec![(**lead).clone()];
        let mut at = 0;
        while at < self.queued.len() {
            match &self.queued[at].trigger {
                Trigger::TaskSettled(other) if !self.queued[at].started && lead.goes_with(other) => {
                    let joined = self.queued.remove(at);
                    let Trigger::TaskSettled(other) = &joined.trigger else { unreachable!() };
                    tasks.push((**other).clone());
                    first.work.lock().unwrap().tasks.push(other.task.clone());
                    self.logs.event(
                        &first.id,
                        &ActivationEvent::Joined {
                            at: now(),
                            activation: joined.id.clone(),
                            topic: joined.topic.clone(),
                        },
                    );
                    self.logs.event(
                        &joined.id,
                        &ActivationEvent::Finished {
                            at: now(),
                            summary: format!("reviewed with activation {}", first.id),
                            tokens: 0,
                            cost: 0.0,
                        },
                    );
                    self.logs.line(ActivationLine {
                        id: joined.id.clone(),
                        at: now(),
                        state: "joined".into(),
                        priority: Some(joined.priority),
                        kind: joined.trigger.kind().into(),
                        topic: joined.topic.clone(),
                        summary: first.id.clone(),
                    });
                }
                _ => at += 1,
            }
        }
        if tasks.len() > 1 {
            first.trigger = Trigger::TaskSettledBatch { tasks };
            first.topic = first.trigger.topic();
        }
        first
    }

    pub(super) fn index(&self, activation: &Activation, state: &str, summary: &str) {
        self.logs.line(ActivationLine {
            id: activation.id.clone(),
            at: now(),
            state: state.into(),
            priority: Some(activation.priority),
            kind: activation.trigger.kind().into(),
            topic: activation.topic.clone(),
            summary: summary.into(),
        });
    }

    pub(super) fn publish(&self, active: Option<&Activation>) {
        let status = PrincipalStatus {
            active: active.map(Activation::view),
            suspended: self.suspended.iter().map(Activation::view).collect(),
            queued: self
                .queued
                .iter()
                .chain(self.held.iter().map(|(_, activation)| activation))
                .map(Activation::view)
                .collect(),
        };
        self.logs.status(&status);
        *self.status.lock().unwrap() = status;
    }

    /// The system prompt: the charter, the user's portrait, and who Lucia is. The same for
    /// every activation and every turn, so the provider's cache of it holds (decision D87);
    /// what changes goes into the activation's opening message (`situation`).
    pub(super) fn system_prompt(&self) -> String {
        let mut prompt = PRINCIPAL_PROMPT
            .replace("{{agent_name}}", &self.parts.agent_name)
            .replace("{{user_ref}}", &self.parts.user_ref)
            .trim()
            .to_string();
        if let Some(portrait) = self.parts.portrait.as_ref().and_then(crate::portrait::Portrait::prompt_section) {
            prompt.push_str(&format!("\n\n<portrait>\n{}\n{portrait}\n</portrait>", crate::prompt::PORTRAIT_INTRO));
        }
        if !self.parts.self_prompt.trim().is_empty() {
            prompt.push_str(&format!("\n\n<self>\n{}\n</self>", self.parts.self_prompt.trim()));
        }
        prompt
    }

    /// The situational radar (decision D88's second ring): the date; the goals on their two
    /// clocks with the tasks working for each; the execution board (the pool's tasks, its own
    /// stack and queue, the schedules of the next 48 hours, where the user is, the devices);
    /// and its recent decisions. It opens each activation, so the system prompt stays the same.
    pub(super) fn situation(&self, activation: &Activation) -> String {
        let now = chrono::Utc::now();
        let mut parts = vec![format!("Today is {}.", chrono::Local::now().format("%Y-%m-%d %A"))];
        let views = self.parts.pool.views();
        let clocks = crate::campaign::clocks(&self.parts.paths, now);
        if !clocks.is_empty() {
            let lines: Vec<String> = clocks
                .iter()
                .map(|clock| {
                    let mut line = format!("- {} {}", clock.id, clock.text);
                    match clock.due_in_days {
                        Some(days) if days < 0 => line.push_str(&format!(" · {} days overdue", -days)),
                        Some(days) => line.push_str(&format!(" · due in {days}d")),
                        None => {}
                    }
                    if let Some(idle) = clock.idle_days {
                        line.push_str(&format!(" · idle {idle}d"));
                    }
                    let working = views
                        .iter()
                        .filter(|view| view.settled.is_none() && view.goal.as_deref() == Some(clock.id.as_str()))
                        .count();
                    if working > 0 {
                        line.push_str(&format!(" · {working} task(s) working"));
                    }
                    if let Some(project) = &clock.project {
                        line.push_str(&format!(" · {project}/plan.md"));
                    }
                    if let Some(next) = &clock.next_item {
                        line.push_str(&format!("\n  next: {next}"));
                    }
                    if !clock.blocked.is_empty() {
                        line.push_str(&format!("\n  blocked: {}", clock.blocked.join("; ")));
                    }
                    line
                })
                .collect();
            parts.push(format!("## Goals\n{}", lines.join("\n")));
        }
        let mut board = vec![format!("### Tasks\n{}", self.parts.pool.describe(None))];
        let line = |view: ActivationView| {
            format!("- {} [{}] {} ({} steps)", view.id, view.priority.label(), view.topic, view.steps)
        };
        let suspended: Vec<String> = self.suspended.iter().rev().map(|a| line(a.view())).collect();
        let queued: Vec<String> =
            self.queued.iter().chain(self.held.iter().map(|(_, a)| a)).map(|a| line(a.view())).collect();
        if !suspended.is_empty() || !queued.is_empty() {
            let mut lines = Vec::new();
            if !suspended.is_empty() {
                lines.push(format!("Suspended (yours to resume after this):\n{}", suspended.join("\n")));
            }
            if !queued.is_empty() {
                lines.push(format!("Waiting:\n{}", queued.join("\n")));
            }
            board.push(format!("### Your other activations\n{}", lines.join("\n")));
        }
        let soon = upcoming_schedules(&self.parts.paths, now, chrono::Duration::hours(48));
        if !soon.is_empty() {
            board.push(format!("### Schedules in the next 48 hours\n{}", soon.join("\n")));
        }
        if let Some(focus) = activation.trigger.focus() {
            board.push(format!(
                "## Where\nThe user is at {focus}. A task you start works there unless you give start_task a \
                 `folder` (and a `device`, for a folder on another one)."
            ));
        }
        let here = self.parts.place.here_name().unwrap_or_else(|| "this computer".into());
        if let Some(devices) = crate::place::devices_section(&here, &self.parts.place.devices()) {
            board.push(format!("<devices>\n{devices}\n</devices>"));
        }
        parts.push(format!("## Execution board\n{}", board.join("\n\n")));
        let decisions = self.journal.recent(RECENT_DECISIONS);
        if !decisions.is_empty() {
            let lines: Vec<String> = decisions
                .iter()
                .map(|decision| {
                    format!(
                        "- #{} {} [{}] {}: {}",
                        decision.seq,
                        decision.at.get(..16).unwrap_or_default(),
                        decision.priority,
                        decision.topic,
                        one_line(&decision.summary, 300)
                    )
                })
                .collect();
            parts.push(format!("<recent_decisions>\n{}\n</recent_decisions>", lines.join("\n")));
        }
        format!("<situational_radar>\n{}\n</situational_radar>", parts.join("\n\n"))
    }

    /// The briefing pack for an activation (decision D88's third ring), logged for the board.
    pub(super) fn briefing(&self, activation: &Activation, dossiers: &[crate::evidence::Dossier]) -> Option<String> {
        let sources = crate::briefing::Sources {
            paths: &self.parts.paths,
            vault: self.parts.vault.as_deref(),
            valve: self.parts.valve.as_ref(),
            dmn: &self.parts.dmn,
        };
        let pack = crate::briefing::pack(&sources, &activation.trigger, dossiers, chrono::Utc::now());
        let pack = crate::briefing::BriefingPack {
            items: pack
                .items
                .into_iter()
                .map(|item| crate::briefing::BriefingItem {
                    text: self.logs.scrubber.scrub(&item.text),
                    title: self.logs.scrubber.scrub(&item.title),
                    ..item
                })
                .collect(),
            ..pack
        };
        self.logs.event(&activation.id, &ActivationEvent::Briefed { at: now(), pack: pack.clone() });
        pack.render()
    }

    /// Run `activation` until it ends or the user's signal suspends it.
    pub(super) async fn activate(
        &mut self,
        mut activation: Activation,
        waking: &mut mpsc::UnboundedReceiver<Wake>,
        open: &mut bool,
    ) {
        *self.current.lock().unwrap() = Some(Current {
            id: activation.id.clone(),
            cause: activation.cause.clone(),
            focus: activation.trigger.focus(),
            conversation: activation.trigger.conversation(),
            work: activation.work.clone(),
        });
        let mut agent = self.agent.take().expect("the Principal's agent is back between activations");
        agent.config.system_prompt = self.system_prompt();
        agent.messages = std::mem::take(&mut activation.messages);
        let mut prompts = if activation.started {
            // Back from suspension: what was decided meanwhile.
            let delta: Vec<String> = self
                .journal
                .after(activation.at_decision_seq)
                .iter()
                .map(|decision| {
                    format!("- {} ({}): {}", decision.topic, decision.priority, one_line(&decision.summary, 400))
                })
                .collect();
            let delta = if delta.is_empty() { "- (nothing was decided)".to_string() } else { delta.join("\n") };
            self.logs.event(&activation.id, &ActivationEvent::Resumed { at: now(), delta_summary: delta.clone() });
            self.index(&activation, "resumed", "");
            // The fourth ring goes on, with what changed while it waited (decision D88).
            vec![Message::user_text(format!(
                "<resumption_delta>\n[Resumed after handling higher-priority activation(s):\n{delta}\n]\n\
                 </resumption_delta>\nGo on from where you were, taking that into account."
            ))]
        } else {
            self.logs.event(&activation.id, &ActivationEvent::Started { at: now() });
            self.index(&activation, "started", "");
            let dossiers = self.dossiers(&activation.trigger);
            let briefing = self.briefing(&activation, &dossiers).map(|pack| format!("{pack}\n\n")).unwrap_or_default();
            vec![Message::user_text(format!(
                "{}\n\n{briefing}{}",
                self.situation(&activation),
                activation.trigger.message(self.parts.max_rounds, &dossiers)
            ))]
        };
        activation.started = true;
        self.publish(Some(&activation));
        let mut suspended_by: Option<String> = None;
        let mut nudged = false;
        loop {
            let (tx, events) = mpsc::unbounded_channel::<AgentEvent>();
            let logger = tokio::spawn(log_steps(
                self.logs.clone(),
                activation.id.clone(),
                activation.steps.clone(),
                self.status.clone(),
                events,
                crate::board_link::PlaneReporter::new(self.parts.board.clone(), "principal", activation.id.clone()),
            ));
            let handle = agent.handle();
            let added = {
                let run = agent.prompt(std::mem::take(&mut prompts), &tx);
                tokio::pin!(run);
                loop {
                    tokio::select! {
                        added = &mut run => break added,
                        wake = waking.recv(), if *open => match wake {
                            Some(wake) => {
                                let from_user = wake.trigger.priority() == Priority::P0;
                                if from_user && suspended_by.is_none()
                                    && (activation.priority != Priority::P0 || wake.trigger.urgent())
                                {
                                    // The user first: stop the request in flight, keep the rest.
                                    suspended_by = Some(wake.id.clone());
                                    self.queued_front(wake);
                                    handle.abort();
                                } else if from_user && activation.priority == Priority::P0 && suspended_by.is_none() {
                                    // More from the user while theirs runs: it joins.
                                    self.join(&activation, wake, &handle);
                                } else {
                                    self.enqueue(wake);
                                    self.publish(Some(&activation));
                                }
                            }
                            None => *open = false,
                        },
                    }
                }
            };
            drop(tx);
            let _ = logger.await;
            for message in &added {
                if let Message::Assistant(reply) = message {
                    activation.tokens += reply.usage.input + reply.usage.output;
                    activation.cost += reply.usage.cost.total;
                }
            }
            // What joined too late for the run goes on in the same activation.
            let left = handle.steering.take_all();
            if suspended_by.is_none() && !left.is_empty() {
                prompts = left;
                continue;
            }
            // Ended having done nothing about what woke it: asked once to (decision D87).
            let stopped =
                matches!(added.last(), Some(Message::Assistant(reply)) if reply.stop_reason == StopReason::Stop);
            if suspended_by.is_none()
                && !nudged
                && stopped
                && self.parts.enforce_disposition
                && activation.trigger.needs_disposition()
                && !activation.work.lock().unwrap().disposed()
            {
                nudged = true;
                self.logs.event(&activation.id, &ActivationEvent::Nudged { at: now(), text: DISPOSITION_NUDGE.into() });
                prompts = vec![Message::user_text(DISPOSITION_NUDGE)];
                continue;
            }
            break;
        }
        activation.messages = std::mem::take(&mut agent.messages);
        self.agent = Some(agent);
        *self.current.lock().unwrap() = None;
        crate::board_link::PlaneReporter::new(self.parts.board.clone(), "principal", activation.id.clone()).finish();
        match suspended_by {
            Some(by) => self.suspend(activation, by),
            None => self.finish(activation),
        }
        self.publish(None);
    }

    /// The dossier of every task a review is of: brief, report, and evidence manifest, as they
    /// read now (decision D87).
    pub(super) fn dossiers(&self, trigger: &Trigger) -> Vec<crate::evidence::Dossier> {
        trigger
            .settled_tasks()
            .iter()
            .map(|settled| {
                let dossier = crate::evidence::dossier(
                    &self.parts.paths,
                    self.parts.vault.as_deref(),
                    &crate::ledger::TaskId(settled.ledger_task.clone()),
                    settled.workspace.as_deref(),
                    &settled.answer,
                    settled.done(),
                );
                // Every part goes into the review note the model reads, and into its log.
                let scrub = |text: Option<String>| text.map(|text| self.logs.scrubber.scrub(&text));
                crate::evidence::Dossier {
                    brief: scrub(dossier.brief),
                    findings: scrub(dossier.findings),
                    report: scrub(dossier.report),
                    manifest: self.logs.scrubber.scrub(&dossier.manifest),
                    unchecked_change: dossier.unchecked_change,
                }
            })
            .collect()
    }

    /// Put a wake ahead of everything waiting.
    pub(super) fn queued_front(&mut self, wake: Wake) {
        self.enqueue(wake);
        if let Some(at) = self.queued.iter().position(|waiting| waiting.priority == Priority::P0) {
            let first = self.queued.remove(at);
            self.queued.insert(0, first);
        }
    }

    pub(super) fn join(&mut self, activation: &Activation, wake: Wake, handle: &nypi_agent::AgentHandle) {
        let topic = wake.trigger.topic();
        let text = match &wake.trigger {
            Trigger::UserSignal { conversation, .. } => format!(
                "[The user said more, and chat handed it to you as well.]\nTopic: {topic}\n<conversation>\n{}\n\
                 </conversation>",
                conversation.trim()
            ),
            other => other.message(self.parts.max_rounds, &[]),
        };
        handle.steer(Message::user_text(text));
        self.logs.event(
            &activation.id,
            &ActivationEvent::Joined { at: now(), activation: wake.id.clone(), topic: topic.clone() },
        );
        // The joined one has a story of its own that says where it went.
        self.logs.event(
            &wake.id,
            &ActivationEvent::Created {
                at: now(),
                priority: Priority::P0,
                topic: topic.clone(),
                trigger: Box::new(wake.trigger),
            },
        );
        self.logs.event(
            &wake.id,
            &ActivationEvent::Finished {
                at: now(),
                summary: format!("joined activation {}", activation.id),
                tokens: 0,
                cost: 0.0,
            },
        );
        self.logs.line(ActivationLine {
            id: wake.id,
            at: now(),
            state: "joined".into(),
            priority: Some(Priority::P0),
            kind: "userSignal".into(),
            topic,
            summary: activation.id.clone(),
        });
    }

    pub(super) fn suspend(&mut self, mut activation: Activation, by: String) {
        // The request that was stopped is not a step: what came before it is kept whole.
        while matches!(activation.messages.last(), Some(Message::Assistant(reply)) if matches!(reply.stop_reason, StopReason::Aborted | StopReason::Error))
        {
            activation.messages.pop();
        }
        activation.at_decision_seq = self.journal.last_seq();
        let step_count = activation.steps.load(Ordering::SeqCst);
        self.logs.event(
            &activation.id,
            &ActivationEvent::Suspended {
                at: now(),
                by_activation: by.clone(),
                step_count,
                at_decision_seq: activation.at_decision_seq,
            },
        );
        self.index(&activation, "suspended", &by);
        self.suspended.push(activation);
    }

    pub(super) fn finish(&mut self, activation: Activation) {
        let summary = activation
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant(reply) if !reply.text().trim().is_empty() => Some(reply.text()),
                _ => None,
            })
            .map(|text| one_line(&text, SUMMARY_CHARS))
            .unwrap_or_else(|| "(no summary)".into());
        let (tasks, goals, stimulated, disposition) = {
            let work = activation.work.lock().unwrap();
            (work.tasks.clone(), work.goals.clone(), work.stimulated, work.disposition())
        };
        let decision = Decision {
            seq: 0,
            activation_id: activation.id.clone(),
            at: now(),
            priority: activation.priority.label().into(),
            trigger_kind: activation.trigger.kind().into(),
            topic: activation.topic.clone(),
            summary: self.logs.scrubber.scrub(&summary),
            tasks_touched: tasks,
            goals_touched: goals,
            stimulated_chat: stimulated,
            disposition: self.logs.scrubber.scrub(&disposition),
            tokens: activation.tokens,
            cost: activation.cost,
        };
        let seq = self.journal.append(decision.clone()).unwrap_or_default();
        (self.parts.finished)(Decision { seq, ..decision });
        self.logs.event(
            &activation.id,
            &ActivationEvent::Finished {
                at: now(),
                summary: summary.clone(),
                tokens: activation.tokens,
                cost: activation.cost,
            },
        );
        self.index(&activation, "finished", &summary);
        // A review ends with each of its tasks through or sent back.
        for settled in activation.trigger.settled_tasks() {
            let sent_back = self.parts.pool.is_working(&settled.task);
            if !sent_back {
                self.parts.pool.release(&settled.task);
            }
            let round = if sent_back { settled.round + 1 } else { settled.round };
            (self.parts.reviewed)(settled.task.clone(), !sent_back, stimulated, round);
        }
    }
}

/// The schedules that fire within `within` of `now`, soonest first, in a line each.
pub(super) fn upcoming_schedules(
    paths: &Paths,
    now: chrono::DateTime<chrono::Utc>,
    within: chrono::Duration,
) -> Vec<String> {
    use crate::schedule::Trigger as When;
    let mut soon: Vec<(chrono::DateTime<chrono::Utc>, String)> = ScheduleStore::new(paths.schedules())
        .active()
        .into_iter()
        .filter_map(|schedule| {
            let next = match &schedule.trigger {
                When::At { at } => chrono::DateTime::parse_from_rfc3339(at)
                    .ok()
                    .map(|at| at.with_timezone(&chrono::Utc))
                    .or_else(|| {
                        chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M")
                            .ok()
                            .and_then(|local| local.and_local_timezone(chrono::Local).single())
                            .map(|at| at.with_timezone(&chrono::Utc))
                    }),
                When::Every { minutes } => {
                    let last = schedule
                        .last_fired
                        .as_deref()
                        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                        .map(|at| at.with_timezone(&chrono::Utc))
                        .unwrap_or(now);
                    Some((last + chrono::Duration::minutes(*minutes as i64)).max(now))
                }
                When::Daily { time, weekdays } => crate::schedule::next_daily(now, time, weekdays),
                _ => None,
            }?;
            (next <= now + within).then(|| {
                let goal = schedule.goal.as_deref().map(|goal| format!(" (goal {goal})")).unwrap_or_default();
                (
                    next,
                    format!(
                        "- {} {}{goal}: {}",
                        next.with_timezone(&chrono::Local).format("%a %H:%M"),
                        schedule.id,
                        one_line(schedule.action.text(), 120)
                    ),
                )
            })
        })
        .collect();
    soon.sort_by_key(|(next, _)| *next);
    soon.into_iter().map(|(_, line)| line).collect()
}

/// Write an activation's steps to its log as they happen, counting them where the status shows.
pub(super) async fn log_steps(
    logs: Logs,
    id: String,
    steps: Arc<AtomicUsize>,
    status: Arc<Mutex<PrincipalStatus>>,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    mut plane: crate::board_link::PlaneReporter,
) {
    let counted = |count: usize| {
        let mut status = status.lock().unwrap();
        if let Some(active) = status.active.as_mut().filter(|active| active.id == id) {
            active.steps = count;
            logs.status(&status);
        }
    };
    let mut arguments: HashMap<String, Value> = HashMap::new();
    while let Some(event) = events.recv().await {
        plane.observe(&event);
        match event {
            AgentEvent::ToolExecutionStart { call_id, arguments: given, .. } => {
                arguments.insert(call_id, given);
            }
            AgentEvent::ToolExecutionEnd { call_id, tool_name, output, duration_ms } => {
                if tool_name == "stimulate_chat"
                    && !output.is_error
                    && let Some(stimulus) =
                        output.details.clone().and_then(|details| serde_json::from_value::<Stimulus>(details).ok())
                {
                    logs.event(&id, &ActivationEvent::StimulatedChat { at: now(), stimulus });
                }
                let text: String = nypi_ai::text_of(&output.content).chars().take(LOGGED_OUTPUT_CHARS).collect();
                logs.event(
                    &id,
                    &ActivationEvent::ToolResult {
                        at: now(),
                        tool: tool_name,
                        arguments: arguments.remove(&call_id).unwrap_or(Value::Null),
                        call_id,
                        output: text,
                        duration_ms: duration_ms.unwrap_or(0),
                        is_error: output.is_error,
                    },
                );
                counted(steps.fetch_add(1, Ordering::SeqCst) + 1);
            }
            AgentEvent::MessageEnd { message: Message::Assistant(reply) } => {
                let thinking: Vec<String> = reply
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Thinking { thinking, .. } => Some(thinking.clone()),
                        _ => None,
                    })
                    .collect();
                let calls: Vec<Value> = reply
                    .tool_calls()
                    .iter()
                    .map(|call| json!({ "id": call.id, "name": call.name, "arguments": call.arguments }))
                    .collect();
                let aborted = matches!(reply.stop_reason, StopReason::Aborted | StopReason::Error);
                logs.event(
                    &id,
                    &ActivationEvent::AssistantTurn {
                        at: now(),
                        thinking: thinking.join("\n\n"),
                        text: reply.text(),
                        tool_calls: calls,
                        stop_reason: format!("{:?}", reply.stop_reason).to_lowercase(),
                    },
                );
                if !aborted {
                    counted(steps.fetch_add(1, Ordering::SeqCst) + 1);
                }
            }
            _ => {}
        }
    }
}
