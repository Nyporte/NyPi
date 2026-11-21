//! The home screen: what needs the owner (with the answer buttons on the card), chat and the
//! Principal as they are now (with the step each is at and the end of what it writes), the tasks under the goals they serve, what runs right now (with the
//! reply as it is written), the devices, the last 24 hours, and what comes next.

use leptos::prelude::*;
use leptos::task::spawn_local;
use nypi_board_proto::{
    Action, ActivationCard, ChatCard, GoalGroup, InboxItem, Now, PrincipalCard, Process, Step, TaskNode, Today,
};

use crate::format::{self, capitalize};
use crate::parts::{self, LinkPill, PlaneLiveLine, PriorityPill};
use crate::{Board, api};

#[component]
pub fn NowPage() -> impl IntoView {
    let board = expect_context::<Board>();
    spawn_local(async move {
        if let Ok(now) = api::get::<Now>(board, "/api/now").await {
            api::show_now(board, now);
        }
    });
    // Every live card streams its reply while this screen is open.
    api::live(board, "/api/live?watch=*", |_| {});
    let inbox = Memo::new(move |_| board.now.with(|n| n.inbox.clone()));
    let running = Memo::new(move |_| {
        board
            .now
            .with(|n| n.processes.iter().filter(|p| p.run_started.is_some()).map(|p| p.id.clone()).collect::<Vec<_>>())
    });
    let idle = Memo::new(move |_| {
        board.now.with(|n| n.processes.iter().filter(|p| p.run_started.is_none()).cloned().collect::<Vec<_>>())
    });
    view! {
        <Show when=move || board.loaded.get() fallback=|| view! { <p class="loading">"Connecting to the board…"</p> }>
            <section class="section">
                {move || {
                    let count = inbox.with(Vec::len);
                    if count == 0 {
                        view! {
                            <div class="calm">
                                <span class="calm-mark">"✓"</span>
                                <div>
                                    <strong>"Nothing needs you."</strong>
                                    <p>"Approvals, blocked tasks, and alerts show up here."</p>
                                </div>
                            </div>
                        }
                            .into_any()
                    } else {
                        view! {
                            <h2 class="heading">"Needs you" <span class="count urgent">{count}</span></h2>
                        }
                            .into_any()
                    }
                }}
                <div class="cards">
                    <For each=move || inbox.get() key=InboxItem::key let:item>
                        <InboxCard item/>
                    </For>
                </div>
            </section>
            <section class="section">
                <h2 class="heading">"Chat and the Principal"</h2>
                <Planes/>
            </section>
            <section class="section">
                <h2 class="heading">
                    "Tasks and campaigns" <span class="count">{move || board.now.with(|n| n.campaigns.iter().map(|g| g.tasks.len()).sum::<usize>().max(n.graph.len()))}</span>
                </h2>
                <Campaigns/>
            </section>
            <section class="section">
                <h2 class="heading">
                    "Working now" <span class="count">{move || running.with(Vec::len)}</span>
                </h2>
                <Show when=move || running.with(Vec::is_empty)>
                    <p class="quiet">"Nothing is running."</p>
                </Show>
                <div class="cards">
                    <For each=move || running.get() key=|id| id.clone() let:id>
                        <ProcessCard id/>
                    </For>
                </div>
                <Show when=move || !idle.with(Vec::is_empty)>
                    <div class="idle-list">
                        <span class="label">"Idle"</span>
                        {move || {
                            idle.get()
                                .into_iter()
                                .map(|p| {
                                    view! {
                                        <span class="idle" title=p.cwd.clone()>
                                            <span class="chip">{p.front_end.clone()}</span>
                                            {capitalize(&p.agent)}
                                            <span class="muted">{format::short_path(&p.cwd)}</span>
                                        </span>
                                    }
                                })
                                .collect_view()
                        }}
                    </div>
                </Show>
            </section>
            <section class="section">
                <h2 class="heading">"Devices" <span class="count">{move || board.now.with(|n| n.devices.len())}</span></h2>
                {move || {
                    let devices = board.now.with(|n| n.devices.clone());
                    if devices.is_empty() {
                        return view! { <p class="quiet">"This computer only."</p> }.into_any();
                    }
                    let clock = board.clock.get();
                    view! {
                        <ul class="upcoming">
                            {devices
                                .into_iter()
                                .map(|device| {
                                    let presence = if device.connected {
                                        format!("connected since {}", format::when(&device.since, clock))
                                    } else {
                                        format!("away, last seen {}", format::when(&device.since, clock))
                                    };
                                    let version = if device.version.is_empty() { String::new() } else { format!("NyPi {}", device.version) };
                                    let what = [device.kind, device.system, version].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
                                    let paired = if device.paired.is_empty() { String::new() } else { format!(" · paired as {}", device.paired.join(" and ")) };
                                    let calls = if device.running > 0 { format!(" · {} call(s) under way", device.running) } else { String::new() };
                                    view! {
                                        <li title=device.tools.join(", ")>
                                            <span class="chip">{device.name}</span>
                                            <span>{what}</span>
                                            <span class="muted">" " {presence} {paired} {calls}</span>
                                        </li>
                                    }
                                })
                                .collect_view()}
                        </ul>
                    }
                        .into_any()
                }}
            </section>
            <section class="section">
                <h2 class="heading">"Last 24 hours" <a class="heading-link" href="/activity">"Activity →"</a></h2>
                {move || view! { <TodayStrip today=board.now.with(|n| n.today.clone())/> }}
            </section>
            <section class="section">
                <h2 class="heading">"Up next"</h2>
                {move || {
                    let upcoming = board.now.with(|n| n.upcoming.clone());
                    if upcoming.is_empty() {
                        return view! { <p class="quiet">"Nothing scheduled."</p> }.into_any();
                    }
                    let clock = board.clock.get();
                    view! {
                        <ul class="upcoming">
                            {upcoming
                                .into_iter()
                                .map(|u| {
                                    view! {
                                        <li>
                                            <span class="when">{format::when(&u.at, clock)}</span>
                                            <span>{u.label}</span>
                                        </li>
                                    }
                                })
                                .collect_view()}
                        </ul>
                    }
                        .into_any()
                }}
            </section>
            <section class="section">
                <h2 class="heading">"Vault" <a class="heading-link" href="/vault">"All notes →"</a></h2>
                {move || {
                    let notes = board.now.with(|n| n.vault.clone());
                    if notes.is_empty() {
                        return view! { <p class="quiet">"Nothing in the inbox or the workspaces yet."</p> }.into_any();
                    }
                    let clock = board.clock.get();
                    view! {
                        <ul class="upcoming">
                            {notes
                                .into_iter()
                                .map(|note| {
                                    view! {
                                        <li>
                                            <span class="when">{format::ago(&note.modified, clock)}</span>
                                            <a href=parts::note_href(&note.path)>{note.path.clone()}</a>
                                        </li>
                                    }
                                })
                                .collect_view()}
                        </ul>
                    }
                        .into_any()
                }}
            </section>
        </Show>
    }
}

#[component]
fn InboxCard(item: InboxItem) -> impl IntoView {
    let board = expect_context::<Board>();
    let busy = RwSignal::new(false);
    let key = item.key();
    let acknowledge = move |key: String| {
        busy.set(true);
        api::act(board, Action::Acknowledge { key }, move |_| busy.set(false));
    };
    match item {
        InboxItem::Approval { id, agent, tool, summary, reason, task, since, .. } => {
            let denying = RwSignal::new(false);
            let why = RwSignal::new(String::new());
            let approve = {
                let id = id.clone();
                move |_| {
                    busy.set(true);
                    api::act(board, Action::Approve { id: id.clone() }, move |ok| busy.set(ok));
                }
            };
            let deny = move |_| {
                let reason = Some(why.get_untracked()).filter(|r| !r.trim().is_empty());
                busy.set(true);
                api::act(board, Action::Deny { id: id.clone(), reason }, move |ok| busy.set(ok));
            };
            view! {
                <article class="card approval" class:busy=move || busy.get()>
                    <header>
                        <span class="kind warn">"Approval"</span>
                        <span class="title">{capitalize(&agent)} " wants to run " <b>{tool}</b></span>
                        <span class="age">{move || format::since(&since, board.clock.get())}</span>
                    </header>
                    <pre class="command">{summary}</pre>
                    <p class="why">{reason}</p>
                    {task.map(|t| view! { <a class="task-link" href=format!("/task/{}", api::encode(&t.id))>"Task: " {t.goal}</a> })}
                    <Show
                        when=move || denying.get()
                        fallback=move || {
                            let approve = approve.clone();
                            view! {
                                <div class="actions">
                                    <button class="primary" on:click=approve disabled=move || busy.get()>"Approve"</button>
                                    <button on:click=move |_| denying.set(true) disabled=move || busy.get()>"Deny…"</button>
                                </div>
                            }
                        }
                    >
                        <div class="actions deny">
                            <input
                                type="text"
                                placeholder="Why not? (Lucia reads this)"
                                prop:value=move || why.get()
                                on:input=move |ev| why.set(event_target_value(&ev))
                            />
                            <button class="danger" on:click=deny.clone() disabled=move || busy.get()>"Deny"</button>
                            <button class="plain" on:click=move |_| denying.set(false)>"Back"</button>
                        </div>
                    </Show>
                </article>
            }
                .into_any()
        }
        InboxItem::Blocked { task, question, since } => view! {
            <article class="card blocked">
                <header>
                    <span class="kind warn">"Blocked"</span>
                    <span class="title">{task.goal.clone()}</span>
                    <span class="age">{move || format::ago(&since, board.clock.get())}</span>
                </header>
                <p class="question">{if question.is_empty() { "Lucia is waiting for you.".to_string() } else { question }}</p>
                <div class="actions">
                    <a class="button primary" href=format!("/task/{}", api::encode(&task.id))>"Open task"</a>
                    <button class="plain" on:click=move |_| acknowledge(key.clone()) disabled=move || busy.get()>"Clear"</button>
                </div>
                <p class="hint">"Reply where you started it; the conversation continues there."</p>
            </article>
        }
            .into_any(),
        InboxItem::Stuck { task, state, note, since } => view! {
            <article class="card stuck">
                <header>
                    <span class="kind bad">{capitalize(&state)}</span>
                    <span class="title">{task.goal.clone()}</span>
                    <span class="age">{move || format::ago(&since, board.clock.get())}</span>
                </header>
                <p>{note}</p>
                <div class="actions">
                    <a class="button" href=format!("/task/{}", api::encode(&task.id))>"Open task"</a>
                    <button class="plain" on:click=move |_| acknowledge(key.clone()) disabled=move || busy.get()>"Clear"</button>
                </div>
                <p class="hint">"The watchdog takes up tasks whose process died; a run that stops making progress is stopped and fails. " <code>"nypi tasks"</code> " resumes or cancels by hand."</p>
            </article>
        }
            .into_any(),
        InboxItem::Join { asker, name, system, from, code, expires, .. } => {
            let answer = move |action: Action| {
                busy.set(true);
                api::act(board, action, move |ok| busy.set(ok));
            };
            let (admit, exec_only, refuse) = (code.clone(), code.clone(), code.clone());
            let who = if asker == "browser" { format!("A browser ({system})") } else { format!("{name} ({system})") };
            let spaced = if code.len() == 6 { format!("{} {}", &code[..3], &code[3..]) } else { code.clone() };
            view! {
                <article class="card join">
                    <header>
                        <span class="kind warn">"Join request"</span>
                        <span class="title">{format!("{who} asks to join from {from}")}</span>
                        <span class="age">{move || format::ago(&expires, board.clock.get())}</span>
                    </header>
                    <p class="code">"Code " <strong>{spaced}</strong></p>
                    <p class="hint">"Admit it only if this is the code on the screen of the machine you are adding."</p>
                    <div class="actions">
                        <button class="primary" on:click=move |_| answer(Action::Admit { code: admit.clone(), exec_only: false }) disabled=move || busy.get()>"Admit as my computer"</button>
                        <button on:click=move |_| answer(Action::Admit { code: exec_only.clone(), exec_only: true }) disabled=move || busy.get()>"Admit as exec-only"</button>
                        <button class="danger" on:click=move |_| answer(Action::Refuse { code: refuse.clone() }) disabled=move || busy.get()>"Refuse"</button>
                    </div>
                </article>
            }
                .into_any()
        }
        InboxItem::Alert { level, message, since, .. } => {
            let tone = if level == "error" { "bad" } else { "warn" };
            view! {
                <article class="card alert">
                    <header>
                        <span class=format!("kind {tone}")>{capitalize(&level)}</span>
                        <span class="title">{message}</span>
                        <span class="age">{move || format::ago(&since, board.clock.get())}</span>
                    </header>
                    <div class="actions">
                        <button class="plain" on:click=move |_| acknowledge(key.clone()) disabled=move || busy.get()>"Clear"</button>
                    </div>
                </article>
            }
                .into_any()
        }
    }
}

/// What a step looks like on a card: its words, its tone, and when it began.
pub fn describe(step: &Step) -> (String, &'static str, Option<String>) {
    match step {
        Step::Idle => ("Idle".into(), "idle", None),
        Step::Waiting { since } => ("Waiting for the model".into(), "wait", Some(since.clone())),
        Step::Thinking { since } => ("Thinking".into(), "think", Some(since.clone())),
        Step::Writing { since, .. } => ("Writing".into(), "write", Some(since.clone())),
        Step::Tool { since, name, .. } => (format!("Running {name}"), "tool", Some(since.clone())),
        Step::Retrying { since, attempt, max_attempts, delay_ms, .. } => (
            format!("Retrying ({attempt}/{max_attempts}) after {}", format::seconds(*delay_ms)),
            "warn",
            Some(since.clone()),
        ),
        Step::Compacting { since, .. } => ("Compacting the conversation".into(), "wait", Some(since.clone())),
        Step::Approval { since, tool, .. } => {
            (format!("Waiting for your approval of {tool}"), "warn", Some(since.clone()))
        }
    }
}

#[component]
pub fn StepLine(process: Process) -> impl IntoView {
    let board = expect_context::<Board>();
    let (label, tone, since) = describe(&process.step);
    let detail = match &process.step {
        Step::Tool { summary, .. } => Some(summary.clone()),
        Step::Retrying { error, .. } => Some(error.clone()),
        Step::Compacting { reason, .. } => Some(reason.clone()),
        _ => None,
    }
    .filter(|d| !d.is_empty());
    view! {
        <div class=format!("step-line {tone}")>
            <span class="pulse" aria-hidden="true"></span>
            <span class="what">{label}</span>
            {since.map(|s| view! { <span class="age">{move || format::since(&s, board.clock.get())}</span> })}
        </div>
        {detail.map(|d| view! { <pre class="step-detail">{d}</pre> })}
    }
}

#[component]
fn ProcessCard(id: String) -> impl IntoView {
    let board = expect_context::<Board>();
    let process = Memo::new({
        let id = id.clone();
        move |_| board.now.with(|n| n.processes.iter().find(|p| p.id == id).cloned())
    });
    // Each part follows only what it shows, so typing a message is never interrupted.
    let field = move |f: fn(&Process) -> String| move || process.with(|p| p.as_ref().map(f).unwrap_or_default());
    let task = Memo::new(move |_| process.with(|p| p.as_ref().and_then(|p| p.task.clone())));
    let tail = Memo::new(move |_| {
        process.with(|p| match p.as_ref().map(|p| &p.step) {
            Some(Step::Writing { tail, .. }) if !tail.is_empty() => Some(tail.clone()),
            _ => None,
        })
    });
    let used = move || {
        process.with(|p| match p {
            Some(p) if p.context_window > 0 => p.context_tokens as f64 / p.context_window as f64,
            _ => 0.0,
        })
    };
    let messaging = RwSignal::new(false);
    let message = RwSignal::new(String::new());
    let confirm_stop = RwSignal::new(false);
    let stop = {
        let id = id.clone();
        move |_| {
            if !confirm_stop.get_untracked() {
                confirm_stop.set(true);
                return;
            }
            confirm_stop.set(false);
            api::act(board, Action::Stop { process: id.clone() }, |_| {});
        }
    };
    let send = move || {
        let text = message.get_untracked();
        if text.trim().is_empty() {
            return;
        }
        api::act(board, Action::Message { process: id.clone(), text }, move |ok| {
            if ok {
                message.set(String::new());
                messaging.set(false);
            }
        });
    };
    let send_key = send.clone();
    view! {
        <article class="card process">
            <header>
                <span class="chip">{field(|p| p.front_end.clone())}</span>
                <span class="title">{field(|p| capitalize(&p.agent))}</span>
                <span class="muted model">{field(|p| p.model.clone())}</span>
                <span class="age" title="Running for">
                    {move || format::since(&process.with(|p| p.as_ref().and_then(|p| p.run_started.clone()).unwrap_or_default()), board.clock.get())}
                </span>
            </header>
            {move || task.get().map(|t| view! { <a class="goal" href=format!("/task/{}", api::encode(&t.id))>{t.goal}</a> })}
            {move || process.get().map(|process| view! { <StepLine process/> })}
            {move || tail.get().map(|t| view! { <div class="tail"><p>{t}</p></div> })}
            <div class="meters">
                <div class="gauge" title=field(|p| format!("{} of {} tokens in context", p.context_tokens, p.context_window))>
                    <span class="gauge-label">"Context"</span>
                    <span class="bar">
                        <span
                            class=move || {
                                let used = used();
                                format!("fill {}", if used > 0.9 { "bad" } else if used > 0.7 { "warn" } else { "ok" })
                            }
                            style=move || format!("width:{:.0}%", (used() * 100.0).min(100.0))
                        ></span>
                    </span>
                    <span class="gauge-value">{move || format!("{:.0}%", used() * 100.0)}</span>
                </div>
                <span class="cost" title="This run so far">{field(|p| format::money(p.run_cost))}</span>
            </div>
            {move || {
                let dropped = process.with(|p| p.as_ref().map_or(0, |p| p.dropped));
                (dropped > 0).then(|| view! { <p class="hint">{format!("The board missed {dropped} events from this process.")}</p> })
            }}
            <div class="actions" class:hidden=move || messaging.get()>
                <button on:click=move |_| messaging.set(true)>"Message"</button>
                <button class="danger-quiet" on:click=stop>
                    {move || if confirm_stop.get() { "Stop it?" } else { "Stop" }}
                </button>
            </div>
            <div class="actions message" class:hidden=move || !messaging.get()>
                <input
                    type="text"
                    placeholder="Tell Lucia something before her next step"
                    prop:value=move || message.get()
                    on:input=move |ev| message.set(event_target_value(&ev))
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        if ev.key() == "Enter" {
                            send_key();
                        }
                    }
                />
                <button class="primary" on:click=move |_| send()>"Send"</button>
                <button class="plain" on:click=move |_| messaging.set(false)>"Cancel"</button>
            </div>
        </article>
    }
}

#[component]
fn TodayStrip(today: Today) -> impl IntoView {
    let runs = today.done + today.blocked + today.failed + today.continuing;
    let share = |n: u32| if runs == 0 { 0.0 } else { n as f64 * 100.0 / runs as f64 };
    let versus = if today.cost_week_average > 0.0 {
        let ratio = today.cost / today.cost_week_average;
        format!("{:.0}% of a usual day", ratio * 100.0)
    } else {
        "no week to compare yet".into()
    };
    view! {
        <div class="tiles">
            <div class="tile wide">
                <span class="tile-label">"Runs"</span>
                <span class="tile-value">{runs}</span>
                <span class="runbar" aria-hidden="true">
                    <span class="seg done" style=format!("width:{}%", share(today.done))></span>
                    <span class="seg blocked" style=format!("width:{}%", share(today.blocked))></span>
                    <span class="seg failed" style=format!("width:{}%", share(today.failed))></span>
                    <span class="seg continuing" style=format!("width:{}%", share(today.continuing))></span>
                </span>
                <span class="legend">
                    <span class="l done">{today.done} " done"</span>
                    <span class="l blocked">{today.blocked} " blocked"</span>
                    <span class="l failed">{today.failed} " failed"</span>
                    <span class="l continuing">{today.continuing} " continuing"</span>
                </span>
            </div>
            <div class="tile">
                <span class="tile-label">"Tool calls"</span>
                <span class="tile-value">{today.tool_calls}</span>
                <span class="tile-note" class:bad={today.tool_failures > 0}>{today.tool_failures} " failed"</span>
            </div>
            <div class="tile">
                <span class="tile-label">"Retries"</span>
                <span class="tile-value">{today.retries}</span>
                <span class="tile-note">{today.retries_recovered} " came through"</span>
            </div>
            <div class="tile">
                <span class="tile-label">"Tokens"</span>
                <span class="tile-value">{format::tokens(today.tokens_in + today.tokens_out)}</span>
                <span class="tile-note">{format::tokens(today.tokens_in)} " in · " {format::tokens(today.tokens_out)} " out"</span>
            </div>
            <div class="tile">
                <span class="tile-label">"Cost"</span>
                <span class="tile-value">{format::money(today.cost)}</span>
                <span class="tile-note">{versus}</span>
            </div>
        </div>
    }
}

/// How much of what a plane is writing its card shows.
const LIVE_TAIL: usize = 200;

/// Chat and the Principal side by side, or a word that the planes are not in use.
#[component]
fn Planes() -> impl IntoView {
    let board = expect_context::<Board>();
    // The cards draw what is live from the board's own signals, so a step does not redraw them.
    let chat = Memo::new(move |_| board.now.with(|n| ChatCard { live: None, ..n.planes.chat.clone() }));
    let principal = Memo::new(move |_| board.now.with(|n| PrincipalCard { live: None, ..n.planes.principal.clone() }));
    view! {
        {move || {
            if !chat.with(|c| c.on) && !principal.with(|p| p.on) {
                return view! {
                    <div class="calm off">
                        <span class="calm-mark">"–"</span>
                        <div>
                            <strong>"Chat mode is off."</strong>
                            <p>"No chat model is set, so the worker talks with you directly and there is no Principal."</p>
                        </div>
                    </div>
                }
                    .into_any();
            }
            view! {
                <div class="planes">
                    <ChatPlane card=chat.get()/>
                    <PrincipalPlane card=principal.get()/>
                </div>
            }
                .into_any()
        }}
    }
}

#[component]
fn ChatPlane(card: ChatCard) -> impl IntoView {
    let board = expect_context::<Board>();
    let ChatCard {
        on,
        model,
        last_at,
        last_said,
        last_reply,
        turns_today,
        reads_today,
        timeouts_today,
        signals_today,
        ..
    } = card;
    view! {
        <article class="card plane lane-chat">
            <header>
                <span class="lane-name">"Chat"</span>
                <span class="muted model">{model}</span>
                {last_at.map(|at| view! { <span class="age">{move || format::ago(&at, board.clock.get())}</span> })}
            </header>
            <PlaneLiveLine live=board.chat_live max=LIVE_TAIL/>
            {if !on {
                view! { <p class="quiet">"Chat mode is off: no chat model is set."</p> }.into_any()
            } else if last_said.is_empty() && last_reply.is_empty() {
                view! { <p class="quiet">"No turn yet."</p> }.into_any()
            } else {
                view! {
                    <div class="exchange">
                        {(!last_said.is_empty()).then(|| view! { <p class="said"><span class="who-tag">"In"</span> {last_said.clone()}</p> })}
                        {(!last_reply.is_empty()).then(|| view! { <p class="reply"><span class="who-tag">"Lucia"</span> {last_reply.clone()}</p> })}
                    </div>
                }
                    .into_any()
            }}
            <div class="plane-counts">
                <span><b>{turns_today}</b> " turns"</span>
                <span title="Read-only lookups chat made today">
                    <b>{reads_today}</b> " reads"
                    {(timeouts_today > 0).then(|| view! { <span class="kind warn">{timeouts_today} " timed out"</span> })}
                </span>
                <span title="Handed up to the Principal today"><b>{signals_today}</b> " signals"</span>
                <span class="muted">"today"</span>
            </div>
            <a class="more" href="/chat">"Open chat →"</a>
        </article>
    }
}

#[component]
fn PrincipalPlane(card: PrincipalCard) -> impl IntoView {
    let board = expect_context::<Board>();
    let PrincipalCard { on, model, active, suspended, queued, decisions_today, cost_today, .. } = card;
    let mut by_priority: Vec<(String, usize)> = Vec::new();
    for item in &queued {
        let label = item.priority.get(..2).unwrap_or(&item.priority).to_string();
        match by_priority.iter_mut().find(|(p, _)| *p == label) {
            Some((_, count)) => *count += 1,
            None => by_priority.push((label, 1)),
        }
    }
    by_priority.sort();
    view! {
        <article class="card plane lane-principal">
            <header>
                <span class="lane-name">"Principal"</span>
                <span class="muted model">{model}</span>
                <span class="age">{decisions_today} " decisions · " {format::money(cost_today)}</span>
            </header>
            {if !on {
                view! { <p class="quiet">"The Principal is off."</p> }.into_any()
            } else {
                match active {
                    Some(active) => {
                        let created = active.created.clone();
                        view! {
                            <a class="active-line" href=parts::activation_href(&active.id)>
                                <span class="pulse-dot" aria-hidden="true"></span>
                                <PriorityPill priority=active.priority.clone()/>
                                <span class="topic">{active.topic.clone()}</span>
                            </a>
                            <p class="sub">
                                "Active · " {active.steps} " steps · " {active.kind.clone()} " · "
                                {move || format::since(&created, board.clock.get())}
                            </p>
                        }
                            .into_any()
                    }
                    None => view! { <p class="idle-line"><span class="kind">"Idle"</span></p> }.into_any(),
                }
            }}
            <PlaneLiveLine live=board.principal_live max=LIVE_TAIL/>
            {(!suspended.is_empty()).then(|| view! {
                <div class="stack-mini">
                    <span class="label">"Suspended, next to resume first"</span>
                    <ul>{suspended.into_iter().rev().map(|a| view! { <SuspendedLine activation=a/> }).collect_view()}</ul>
                </div>
            })}
            <div class="plane-counts">
                <span><b>{queued.len()}</b> " queued"</span>
                {by_priority
                    .into_iter()
                    .map(|(p, n)| view! { <span><PriorityPill priority=p/> " ×" {n}</span> })
                    .collect_view()}
            </div>
            <a class="more" href="/principal">"Open the Principal →"</a>
        </article>
    }
}

#[component]
fn SuspendedLine(activation: ActivationCard) -> impl IntoView {
    view! {
        <li>
            <PriorityPill priority=activation.priority/>
            <a href=parts::activation_href(&activation.id)>{activation.topic}</a>
            <span class="muted">" " {activation.steps} " steps kept"</span>
        </li>
    }
}

/// The open and newest tasks under the goals they serve; those serving no goal last.
#[component]
fn Campaigns() -> impl IntoView {
    let board = expect_context::<Board>();
    let groups = Memo::new(move |_| {
        board.now.with(|n| {
            let mut groups = n.campaigns.clone();
            // A board service from before the campaigns sends only the tree.
            if groups.is_empty() && !n.graph.is_empty() {
                groups.push(GoalGroup { goal: None, tasks: n.graph.clone() });
            }
            groups.sort_by_key(|g| g.goal.is_none());
            groups
        })
    });
    view! {
        {move || {
            let groups = groups.get();
            if groups.is_empty() {
                return view! { <p class="quiet">"No task is open."</p> }.into_any();
            }
            view! {
                <div class="card plane lane-tasks campaigns">
                    {groups.into_iter().map(|group| view! { <Campaign group/> }).collect_view()}
                </div>
            }
                .into_any()
        }}
    }
}

#[component]
fn Campaign(group: GoalGroup) -> impl IntoView {
    let board = expect_context::<Board>();
    let head = match &group.goal {
        Some(goal) => {
            let plan = parts::plan_path(goal);
            let due = goal.due.clone();
            view! {
                <div class="campaign-head">
                    <a class="pill vault" href=parts::goal_href(&goal.id)>{goal.id.clone()}</a>
                    <span class="campaign-title">{goal.text.clone()}</span>
                    {due.map(|due| view! { <span class="kind warn" title=due.clone()>"Due " {format::day(&due, board.clock.get_untracked())}</span> })}
                    {plan.map(|plan| view! { <LinkPill href=parts::note_href(&plan) lane="vault" label="plan.md".to_string()/> })}
                </div>
            }
            .into_any()
        }
        None => view! {
            <div class="campaign-head">
                <span class="campaign-title muted">"No goal"</span>
            </div>
        }
        .into_any(),
    };
    view! {
        <section class="campaign">
            {head}
            {if group.tasks.is_empty() {
                view! { <p class="quiet campaign-empty">"No open task."</p> }.into_any()
            } else {
                view! { <ul class="campaign-tasks">{group.tasks.into_iter().map(|task| view! { <TaskLine task/> }).collect_view()}</ul> }.into_any()
            }}
        </section>
    }
}

#[component]
fn TaskLine(task: TaskNode) -> impl IntoView {
    let board = expect_context::<Board>();
    // A task under the task it works for is set in by its depth.
    let inset = format!("margin-left:{}rem", task.depth.min(6) as f32 * 1.25);
    let process = task.process.clone();
    let step = move || {
        let id = process.as_ref()?;
        let (label, tone, _) =
            board.now.with(|n| n.processes.iter().find(|p| &p.id == id).map(|p| describe(&p.step)))?;
        (tone != "idle").then(|| view! { <span class=format!("live-step {tone}")>{label}</span> })
    };
    view! {
        <li style=inset>
            <span class=format!("kind {}", parts::state_class(&task.state))>{capitalize(&task.state)}</span>
            <a class="task-title" href=parts::task_href(&task.id)>{task.goal.clone()}</a>
            {(!task.place.is_empty()).then(|| view! { <span class="chip place" title="Where it works">{task.place.clone()}</span> })}
            {step}
            {task.waits.clone().map(|waits| view! { <span class="muted">"waits on " {waits}</span> })}
            {task.activation.clone().map(|a| view! { <LinkPill href=parts::activation_href(&a) lane="principal" label=a.clone()/> })}
        </li>
    }
}
