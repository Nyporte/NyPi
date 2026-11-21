//! The Principal's screens (decision D86). `/principal`: the activation running now (with what it
//! writes as it writes it), the ones set aside for a higher priority (in stack order, with the
//! steps each keeps), the queue, and every activation, newest first, filtered by priority, with
//! how each was disposed of. `/activation/:id`: one activation's story: what woke it, the briefing
//! pack looked up for it, each model turn and tool call, what it had chat tell the owner, where it
//! was suspended, resumed, or nudged, and its decision. Live while it is not finished.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_params_map;
use nypi_board_proto::{
    ActivationCard, ActivationDetail, ActivationRow, ActivationStep, BriefingItemView, BriefingView, LiveEvent,
    PrincipalCard, PrincipalView, StimulusView,
};
use serde_json::Value;

use crate::format::{self, capitalize, markdown};
use crate::parts::{self, LinkPill, PlaneLiveLine, PriorityPill, StimulusCard};
use crate::{Board, api};

/// How often the list is read again while the screen is open, besides when the stack changes.
const REFRESH_MS: f64 = 5000.0;

const FILTERS: [(&str, &str); 4] = [("", "All"), ("p0", "P0 User"), ("p1", "P1 Event"), ("p2", "P2 Idle")];

#[component]
pub fn PrincipalPage() -> impl IntoView {
    let board = expect_context::<Board>();
    api::live(board, "/api/live", |_| {});
    let view_now = RwSignal::new(None::<PrincipalView>);
    let filter = RwSignal::new(String::new());
    // What is live is drawn from the board's own signal; here it would only cause reads and redraws.
    let stack = Memo::new(move |_| board.now.with(|n| PrincipalCard { live: None, ..n.planes.principal.clone() }));
    let tick = Memo::new(move |_| (board.clock.get() / REFRESH_MS).floor() as i64);
    Effect::new(move |_| {
        stack.track();
        tick.track();
        spawn_local(async move {
            if let Ok(found) = api::get::<PrincipalView>(board, "/api/principal").await
                && view_now.with_untracked(|current| current.as_ref() != Some(&found))
            {
                view_now.set(Some(found));
            }
        });
    });
    let card =
        Memo::new(move |_| view_now.with(|v| v.as_ref().map(|v| PrincipalCard { live: None, ..v.card.clone() })));
    let rows = Memo::new(move |_| {
        let filter = filter.get();
        view_now.with(|v| {
            v.as_ref()
                .map(|v| {
                    v.activations
                        .iter()
                        .filter(|row| filter.is_empty() || parts::priority_class(&row.priority) == filter)
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    view! {
        <div class="screen-head">
            <div>
                <h1>"Principal"</h1>
                <p class="muted">"What it decides, one activation at a time: the user first, then events, then idle time."</p>
            </div>
        </div>
        {move || match card.get() {
            None => view! { <p class="loading">"Reading the Principal…"</p> }.into_any(),
            Some(card) => view! { <Stack card/> }.into_any(),
        }}
        <section class="section">
            <h2 class="heading">"Activations"</h2>
            <div class="chips" role="group" aria-label="Priority">
                {FILTERS
                    .iter()
                    .map(|(value, label)| {
                        let value = value.to_string();
                        let here = value.clone();
                        view! {
                            <button class="chip-button" class:on=move || filter.get() == here on:click=move |_| filter.set(value.clone())>
                                {*label}
                            </button>
                        }
                    })
                    .collect_view()}
            </div>
            {move || {
                let rows = rows.get();
                if view_now.with(Option::is_none) {
                    return view! { <span></span> }.into_any();
                }
                if rows.is_empty() {
                    return view! { <p class="quiet">"No activation yet."</p> }.into_any();
                }
                let clock = board.clock.get_untracked();
                let mut days: Vec<(String, Vec<ActivationRow>)> = Vec::new();
                for row in rows {
                    let day = format::day(&row.created, clock);
                    match days.last_mut() {
                        Some((last, group)) if *last == day => group.push(row),
                        _ => days.push((day, vec![row])),
                    }
                }
                days.into_iter()
                    .map(|(day, rows)| view! {
                        <section class="day">
                            <h3 class="subheading">{day}</h3>
                            <ul class="rows">{rows.into_iter().map(|row| view! { <ActivationLine row/> }).collect_view()}</ul>
                        </section>
                    })
                    .collect_view()
                    .into_any()
            }}
        </section>
    }
}

/// The running activation, the suspended stack, and the queue.
#[component]
fn Stack(card: PrincipalCard) -> impl IntoView {
    let board = expect_context::<Board>();
    let PrincipalCard { on, model, active, suspended, queued, decisions_today, cost_today, .. } = card;
    view! {
        <section class="section">
            <div class="facts-line muted">
                {(!on).then(|| view! { <span class="kind">"Off"</span> })}
                {(!model.is_empty()).then(|| view! { <span class="chip">{model.clone()}</span> })}
                <span>{decisions_today} " decisions today"</span>
                <span>{format::money(cost_today)} " today"</span>
            </div>
            <div class="stack">
                <div class="card plane lane-principal stack-active">
                    <span class="label">"Active"</span>
                    {match active {
                        Some(a) => {
                            let created = a.created.clone();
                            view! {
                                <a class="active-line big" href=parts::activation_href(&a.id)>
                                    <span class="pulse-dot" aria-hidden="true"></span>
                                    <PriorityPill priority=a.priority.clone()/>
                                    <span class="topic">{a.topic.clone()}</span>
                                </a>
                                <p class="sub">
                                    {a.steps} " steps · " {a.kind.clone()} " · running "
                                    {move || format::since(&created, board.clock.get())}
                                </p>
                            }
                                .into_any()
                        }
                        None => view! { <p class="idle-line"><span class="kind">"Idle"</span> <span class="muted">"Nothing is being decided."</span></p> }.into_any(),
                    }}
                    <PlaneLiveLine live=board.principal_live/>
                </div>
                <div class="stack-side">
                    <h3 class="subheading">"Suspended" <span class="count">{suspended.len()}</span></h3>
                    {if suspended.is_empty() {
                        view! { <p class="quiet">"None set aside."</p> }.into_any()
                    } else {
                        let total = suspended.len();
                        view! {
                            <ol class="stack-cards">
                                {suspended
                                    .into_iter()
                                    .enumerate()
                                    .rev()
                                    .map(|(index, a)| view! { <StackCard activation=a note=if index + 1 == total { "resumes first" } else { "" }/> })
                                    .collect_view()}
                            </ol>
                        }
                            .into_any()
                    }}
                    <h3 class="subheading">"Queued" <span class="count">{queued.len()}</span></h3>
                    {if queued.is_empty() {
                        view! { <p class="quiet">"Nothing waits."</p> }.into_any()
                    } else {
                        view! {
                            <ol class="queue">
                                {queued.into_iter().map(|a| view! {
                                    <li>
                                        <PriorityPill priority=a.priority.clone()/>
                                        <a href=parts::activation_href(&a.id)>{a.topic.clone()}</a>
                                        <span class="muted">{a.kind.clone()}</span>
                                    </li>
                                }).collect_view()}
                            </ol>
                        }
                            .into_any()
                    }}
                </div>
            </div>
        </section>
    }
}

#[component]
fn StackCard(activation: ActivationCard, note: &'static str) -> impl IntoView {
    view! {
        <li class="stack-card">
            <PriorityPill priority=activation.priority/>
            <a href=parts::activation_href(&activation.id)>{activation.topic}</a>
            <span class="muted">{activation.steps} " steps kept"</span>
            {(!note.is_empty()).then(|| view! { <span class="kind warn">{note}</span> })}
        </li>
    }
}

#[component]
fn ActivationLine(row: ActivationRow) -> impl IntoView {
    let ActivationRow {
        id,
        priority,
        kind,
        topic,
        state,
        created,
        summary,
        tokens,
        cost,
        tasks,
        goals,
        stimulated_chat,
        disposition,
        ..
    } = row;
    let finished = state == "finished";
    view! {
        <li class="row activation-row">
            <time title=created.clone()>{format::clock_time(&created)}</time>
            <div class="row-body">
                <div class="row-title">
                    <PriorityPill priority/>
                    <a href=parts::activation_href(&id)>{if topic.is_empty() { id.clone() } else { topic }}</a>
                    <span class=format!("kind {}", parts::state_class(&state))>{capitalize(&state)}</span>
                </div>
                {(!summary.is_empty()).then(|| view! { <p class="row-detail">{summary}</p> })}
                <div class="row-links">
                    <span class="chip">{kind}</span>
                    {(finished || !disposition.is_empty()).then(|| view! { <DispositionBadge disposition=disposition.clone() finished/> })}
                    {stimulated_chat.then(|| view! { <span class="pill chat">"told you"</span> })}
                    {tasks.into_iter().map(|t| view! { <LinkPill href=parts::task_href(&t) lane="tasks" label=t.clone()/> }).collect_view()}
                    {goals.into_iter().map(|g| view! { <LinkPill href=parts::goal_href(&g) lane="vault" label=g.clone()/> }).collect_view()}
                    {(tokens > 0 || cost > 0.0).then(|| view! { <span class="who-line">{format::tokens(tokens)} " tokens · " {format::money(cost)}</span> })}
                </div>
            </div>
        </li>
    }
}

#[component]
pub fn ActivationPage() -> impl IntoView {
    let board = expect_context::<Board>();
    let params = use_params_map();
    let id = Memo::new(move |_| params.with(|p| p.get("id").unwrap_or_default()));
    let detail = RwSignal::new(None::<ActivationDetail>);
    let missing = RwSignal::new(false);
    Effect::new(move |_| {
        let id = id.get();
        detail.set(None);
        missing.set(false);
        spawn_local(async move {
            match api::get::<ActivationDetail>(board, &format!("/api/activation/{}", api::encode(&id))).await {
                Ok(found) => detail.set(Some(found)),
                Err(_) => missing.set(true),
            }
        });
    });
    let finished = Memo::new(move |_| detail.with(|d| d.as_ref().is_some_and(|d| d.row.state == "finished")));
    Effect::new(move |_| {
        let id = id.get();
        let url =
            if finished.get() { "/api/live".to_string() } else { format!("/api/live?activation={}", api::encode(&id)) };
        api::live(board, &url, move |event| {
            if let LiveEvent::Activation { activation, .. } = event
                && activation.row.id == id
            {
                detail.set(Some(*activation));
            }
        });
    });
    let steps = Memo::new(move |_| detail.with(|d| d.as_ref().map(|d| d.steps.clone()).unwrap_or_default()));
    let live_here = Signal::derive(move || {
        let id = id.get();
        board.principal_live.get().filter(|live| live.id == id)
    });
    view! {
        <a class="back" href="/principal">"← Principal"</a>
        <Show when=move || missing.get()>
            <p class="empty">"There is no activation " <code>{move || id.get()}</code> "."</p>
        </Show>
        {move || detail.get().map(|d| view! { <ActivationHead detail=d/> })}
        <Show when=move || detail.with(Option::is_some)>
            <ol class="story">
                <For each=move || steps.get() key=parts::content_key let:step>
                    <ActivationStepItem step/>
                </For>
                <Show when=move || !finished.get() || live_here.with(Option::is_some)>
                    <li class="step live">
                        <div class="who">"Still deciding"</div>
                        <PlaneLiveLine live=live_here/>
                    </li>
                </Show>
            </ol>
        </Show>
    }
}

#[component]
fn ActivationHead(detail: ActivationDetail) -> impl IntoView {
    let board = expect_context::<Board>();
    let ActivationDetail { row, trigger, started, briefing, .. } = detail;
    let finished = row.state == "finished";
    let clock = board.clock.get_untracked();
    let at = move |time: &str| format!("{} {}", format::day(time, clock), format::clock_time(time));
    let took = match (
        started.as_deref().or(Some(row.created.as_str())).and_then(format::parse),
        row.finished.as_deref().and_then(format::parse),
    ) {
        (Some(start), Some(end)) => Some(format::span(end - start)),
        _ => None,
    };
    view! {
        <header class="task-head">
            <div class="task-state">
                <PriorityPill priority=row.priority.clone()/>
                <span class=format!("kind {}", parts::state_class(&row.state))>{capitalize(&row.state)}</span>
                <span class="chip">{row.kind.clone()}</span>
                <DispositionBadge disposition=row.disposition.clone() finished/>
                <code class="muted">{row.id.clone()}</code>
            </div>
            <h1>{if row.topic.is_empty() { row.id.clone() } else { row.topic.clone() }}</h1>
            <dl class="facts">
                <div><dt>"Created"</dt><dd title=row.created.clone()>{at(&row.created)}</dd></div>
                {started.map(|s| view! { <div><dt>"Started"</dt><dd title=s.clone()>{at(&s)}</dd></div> })}
                {row.finished.clone().map(|f| view! { <div><dt>"Finished"</dt><dd title=f.clone()>{at(&f)}</dd></div> })}
                {took.map(|t| view! { <div><dt>"Took"</dt><dd>{t}</dd></div> })}
                <div><dt>"Tokens"</dt><dd>{format::tokens(row.tokens)}</dd></div>
                <div><dt>"Cost"</dt><dd>{format::money(row.cost)}</dd></div>
            </dl>
            {(!row.summary.is_empty()).then(|| view! {
                <div class="decision">
                    <span class="label">"Decision"</span>
                    <div class="md" inner_html=markdown(&row.summary)></div>
                </div>
            })}
            {(!row.tasks.is_empty() || !row.goals.is_empty() || row.stimulated_chat).then(|| view! {
                <div class="row-links">
                    {row.stimulated_chat.then(|| view! { <a class="pill chat" href="/chat">"told you in chat"</a> })}
                    {row.tasks.iter().map(|t| view! { <LinkPill href=parts::task_href(t) lane="tasks" label=t.clone()/> }).collect_view()}
                    {row.goals.iter().map(|g| view! { <LinkPill href=parts::goal_href(g) lane="vault" label=g.clone()/> }).collect_view()}
                </div>
            })}
            {(!trigger.trim().is_empty()).then(|| view! {
                <details class="drawer">
                    <summary><span class="drawer-title">"What woke it"</span></summary>
                    <pre>{parts::pretty(&trigger)}</pre>
                </details>
            })}
            {briefing.map(|briefing| view! { <Briefing briefing/> })}
        </header>
    }
}

/// How an activation was disposed of (decision D87): what it did, or why it only took note.
#[component]
fn DispositionBadge(disposition: String, finished: bool) -> impl IntoView {
    let (class, label) = if disposition.is_empty() {
        let label = if finished { "no disposition" } else { "no disposition yet" };
        (if finished { "kind warn" } else { "kind" }, label.to_string())
    } else if disposition.starts_with("noted_only") {
        ("kind", disposition.replacen("noted_only", "noted only", 1))
    } else {
        ("kind good", disposition.replace('_', " "))
    };
    view! { <span class=format!("{class} disposition") title="How it was disposed of">{label}</span> }
}

/// What was looked up for the activation before its first model call, closed until asked for.
#[component]
fn Briefing(briefing: BriefingView) -> impl IntoView {
    let BriefingView { took_ms, items } = briefing;
    let count = items.len();
    view! {
        <details class="drawer briefing lane-principal">
            <summary>
                <span class="drawer-title">"Briefing pack"</span>
                <span class="count">{count}</span>
                <span class="drawer-path">"looked up in " {format::seconds(took_ms)}</span>
            </summary>
            {if items.is_empty() {
                view! { <p class="quiet">"Nothing was looked up."</p> }.into_any()
            } else {
                view! {
                    <ul class="briefing-items">
                        {items.into_iter().map(|item| view! { <BriefingItem item/> }).collect_view()}
                    </ul>
                }
                    .into_any()
            }}
        </details>
    }
}

#[component]
fn BriefingItem(item: BriefingItemView) -> impl IntoView {
    let BriefingItemView { kind, title, path, text, links } = item;
    view! {
        <li class="briefing-item">
            <div class="briefing-head">
                <span class="kind lane-principal">{kind}</span>
                <strong>{title}</strong>
                {path.map(|path| view! { <code class="briefing-path">{path}</code> })}
            </div>
            {(!text.trim().is_empty()).then(|| view! { <pre class="briefing-text">{text}</pre> })}
            {(!links.is_empty()).then(|| view! {
                <ul class="briefing-links">
                    {links.into_iter().map(|link| view! { <li><code>{format!("[[{link}]]")}</code></li> }).collect_view()}
                </ul>
            })}
        </li>
    }
}

/// The stimulus `stimulate_chat` was called with (its arguments are snake case, as the model wrote them).
fn stimulus_from(arguments: &Value) -> Option<StimulusView> {
    let text = |key: &str| arguments[key].as_str().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
    Some(StimulusView {
        activation: String::new(),
        kind: text("kind").unwrap_or_else(|| "update".into()),
        headline: text("headline")?,
        must_convey: arguments["must_convey"]
            .as_array()
            .map(|items| items.iter().filter_map(|i| i.as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
        ask_user: text("ask_user"),
        vault_ref: text("vault_ref"),
    })
}

/// What a tool call said, in a line, for the closed row.
fn one_line(name: &str, arguments: &Value, raw: &str) -> String {
    let field = |key: &str| arguments[key].as_str().map(str::to_string);
    let picked = match name {
        "start_task" => field("topic").or_else(|| field("instruction")),
        "steer_task" => field("message"),
        "continue_task" => field("critique"),
        "stimulate_chat" => field("headline"),
        _ => None,
    };
    picked.unwrap_or_else(|| raw.split_whitespace().collect::<Vec<_>>().join(" "))
}

#[component]
fn ActivationStepItem(step: ActivationStep) -> impl IntoView {
    match step {
        ActivationStep::Said { at, thinking, text, calls, stopped } => view! {
            <li class="step said">
                <div class="who">
                    "Principal" <time>{format::clock_time(&at)}</time>
                    {stopped.then(|| view! { <span class="kind warn">"Stopped"</span> })}
                </div>
                {(!thinking.trim().is_empty()).then(|| view! {
                    <details class="thinking">
                        <summary>"Reasoning"</summary>
                        <div class="md" inner_html=markdown(&thinking)></div>
                    </details>
                })}
                {(!text.trim().is_empty()).then(|| view! { <div class="md" inner_html=markdown(&text)></div> })}
                {(!calls.is_empty()).then(|| view! {
                    <div class="meta">"Calls " {calls.into_iter().map(|c| view! { <code class="call-name">{c}</code> }).collect_view()}</div>
                })}
            </li>
        }
        .into_any(),
        ActivationStep::Tool { at, name, arguments, output, duration_ms, is_error, task } => {
            let parsed = serde_json::from_str::<Value>(&arguments).unwrap_or(Value::Null);
            let line = one_line(&name, &parsed, &arguments);
            let (mark, tone) = if is_error { ("✗", "bad") } else { ("✓", "good") };
            let shown = match name.as_str() {
                "stimulate_chat" => stimulus_from(&parsed).map(|stimulus| {
                    view! {
                        <div class="tool-shown">
                            <span class="label">"Stimulus sent to chat"</span>
                            <StimulusCard stimulus/>
                        </div>
                    }
                    .into_any()
                }),
                "continue_task" => parsed["critique"].as_str().map(|critique| {
                    view! {
                        <div class="callout critique">
                            <span class="label">"Sent back to the worker with this critique"</span>
                            <div class="md" inner_html=markdown(critique)></div>
                        </div>
                    }
                    .into_any()
                }),
                "steer_task" => parsed["message"].as_str().map(|message| {
                    view! {
                        <div class="callout steer">
                            <span class="label">"Told the worker"</span>
                            <div class="md" inner_html=markdown(message)></div>
                        </div>
                    }
                    .into_any()
                }),
                _ => None,
            };
            let handover = shown.is_some();
            view! {
                <li class="step tool" class:handover=handover>
                    {shown}
                    <details>
                        <summary>
                            <span class=format!("mark {tone}")>{mark}</span>
                            <b>{name}</b>
                            <span class="summary">{line}</span>
                            <span class="took">{format::seconds(duration_ms)}</span>
                            <time>{format::clock_time(&at)}</time>
                        </summary>
                        <h4>"Arguments"</h4>
                        <pre>{parts::pretty(&arguments)}</pre>
                        <h4>"Output"</h4>
                        {if output.is_empty() {
                            view! { <p class="quiet">"(nothing)"</p> }.into_any()
                        } else {
                            view! { <pre class:bad=is_error>{output}</pre> }.into_any()
                        }}
                    </details>
                    {task.map(|t| view! { <div class="step-links"><LinkPill href=parts::task_href(&t) lane="tasks" label=format!("Task {t}")/></div> })}
                </li>
            }
            .into_any()
        }
        ActivationStep::Stimulus { at, stimulus } => view! {
            <li class="step stimulus-step">
                <div class="who">"Chat was asked to tell you" <time>{format::clock_time(&at)}</time> <a class="pill chat" href="/chat">"chat"</a></div>
                <StimulusCard stimulus/>
            </li>
        }
        .into_any(),
        ActivationStep::Suspended { at, by, step_count } => {
            let by_view = if !by.is_empty() && !by.contains(char::is_whitespace) {
                view! { <a href=parts::activation_href(&by)>{by.clone()}</a> }.into_any()
            } else {
                view! { <span>{by.clone()}</span> }.into_any()
            };
            view! {
                <li class="step banner suspended">
                    <span class="banner-mark">"⏸"</span>
                    <span>"Suspended by " {by_view} " after " {step_count} " steps"</span>
                    <time>{format::clock_time(&at)}</time>
                </li>
            }
            .into_any()
        }
        ActivationStep::Resumed { at, delta } => view! {
            <li class="step banner resumed">
                <div class="banner-line">
                    <span class="banner-mark">"▶"</span>
                    <span>"Resumed"</span>
                    <time>{format::clock_time(&at)}</time>
                </div>
                {(!delta.trim().is_empty()).then(|| view! {
                    <div class="banner-body">
                        <span class="label">"What was decided meanwhile"</span>
                        <div class="md" inner_html=markdown(&delta)></div>
                    </div>
                })}
            </li>
        }
        .into_any(),
        ActivationStep::Nudged { at, text } => view! {
            <li class="step banner nudged">
                <div class="banner-line">
                    <span class="banner-mark">"!"</span>
                    <span>"Nudged: it had disposed of nothing, and the harness asked it to"</span>
                    <time>{format::clock_time(&at)}</time>
                </div>
                {(!text.trim().is_empty()).then(|| view! {
                    <div class="banner-body">
                        <div class="md" inner_html=markdown(&text)></div>
                    </div>
                })}
            </li>
        }
        .into_any(),
        ActivationStep::Joined { at, activation, topic } => view! {
            <li class="step banner joined">
                <span class="banner-mark">"+"</span>
                <span>"Joined by " <a href=parts::activation_href(&activation)>{if topic.is_empty() { activation.clone() } else { topic }}</a></span>
                <time>{format::clock_time(&at)}</time>
            </li>
        }
        .into_any(),
        ActivationStep::Finished { at, summary, tokens, cost } => view! {
            <li class="step ended good">
                <div class="who">
                    <span class="kind good">"Finished"</span>
                    <span class="muted">{format::tokens(tokens)} " tokens · " {format::money(cost)}</span>
                    <time>{format::clock_time(&at)}</time>
                </div>
                {(!summary.trim().is_empty()).then(|| view! { <div class="md" inner_html=markdown(&summary)></div> })}
            </li>
        }
        .into_any(),
    }
}
