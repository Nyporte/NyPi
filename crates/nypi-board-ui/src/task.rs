//! A task's story: the goal it serves and the activation that started it, the plan it works under,
//! its workspace notes (brief, findings, report), the evidence the harness drew for its report
//! (which cited calls resolve, and whether a change went unchecked), what was asked, what the model said and reasoned, each tool call with
//! its arguments, output, and device, where the observer steered it, what the harness did,
//! delegated work, the Principal's reviews, and how it ended. While the task runs, the story grows
//! as it happens and the reply streams in as it is written.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_params_map;
use nypi_board_proto::{Action, LiveEvent, StoryStep, TaskDetail, Tone};

use crate::format::{self, capitalize, markdown};
use crate::now::StepLine;
use crate::parts::{self, LinkPill, TextDrawer, state_class};
use crate::{Board, api};

#[component]
pub fn TaskPage() -> impl IntoView {
    let board = expect_context::<Board>();
    let params = use_params_map();
    let id = Memo::new(move |_| params.with(|p| p.get("id").unwrap_or_default()));
    let detail = RwSignal::new(None::<TaskDetail>);
    let missing = RwSignal::new(false);
    let writing = RwSignal::new(String::new());
    Effect::new(move |_| {
        let id = id.get();
        detail.set(None);
        missing.set(false);
        spawn_local(async move {
            match api::get::<TaskDetail>(board, &format!("/api/task/{}", api::encode(&id))).await {
                Ok(found) => detail.set(Some(found)),
                Err(_) => missing.set(true),
            }
        });
    });
    let live_process = Memo::new(move |_| detail.with(|d| d.as_ref().and_then(|d| d.live.clone())));
    // One stream per task and process: the story as it grows, and the reply being written.
    Effect::new(move |_| {
        let id = id.get();
        let url = match live_process.get() {
            Some(process) => format!("/api/live?task={}&watch={}", api::encode(&id), api::encode(&process)),
            None => format!("/api/live?task={}", api::encode(&id)),
        };
        api::live(board, &url, move |event| match event {
            LiveEvent::Task { task } => {
                writing.set(String::new());
                detail.set(Some(*task));
            }
            LiveEvent::Delta { text, .. } => writing.update(|w| w.push_str(&text)),
            LiveEvent::Now { .. } | LiveEvent::Chat { .. } | LiveEvent::Activation { .. } | LiveEvent::Plane { .. } => {
            }
        });
    });
    view! {
        <a class="back" href="/">"← Now"</a>
        <Show when=move || missing.get()>
            <p class="empty">"There is no task " <code>{move || id.get()}</code> "."</p>
        </Show>
        <Show when=move || detail.with(Option::is_some)>
            <Story detail writing/>
        </Show>
    }
}

fn tone_class(tone: Tone) -> &'static str {
    match tone {
        Tone::Plain => "plain",
        Tone::Good => "good",
        Tone::Warning => "warn",
        Tone::Bad => "bad",
    }
}

#[component]
fn Story(detail: RwSignal<Option<TaskDetail>>, writing: RwSignal<String>) -> impl IntoView {
    let board = expect_context::<Board>();
    let steps = Memo::new(move |_| detail.with(|d| d.as_ref().map(|d| d.steps.clone()).unwrap_or_default()));
    let live = Memo::new(move |_| detail.with(|d| d.as_ref().and_then(|d| d.live.clone())));
    let who = Memo::new(move |_| detail.with(|d| d.as_ref().map(|d| capitalize(&d.owner)).unwrap_or_default()));
    let header = move || {
        let detail = detail.get()?;
        let created = detail.created.clone();
        let duration = match (format::parse(&created), format::parse(&detail.updated)) {
            (Some(start), Some(end)) if detail.live.is_none() => format::span(end - start),
            _ => String::new(),
        };
        Some(view! {
            <header class="task-head">
                <div class="task-state">
                    <span class=format!("kind {}", state_class(&detail.state))>{capitalize(&detail.state)}</span>
                    {(!detail.note.is_empty()).then(|| view! { <span class="note">{detail.note.clone()}</span> })}
                </div>
                <h1>{detail.goal.clone()}</h1>
                <dl class="facts">
                    <div><dt>"Who"</dt><dd>{capitalize(&detail.owner)}</dd></div>
                    <div><dt>"From"</dt><dd>{detail.origin.clone()}</dd></div>
                    <div><dt>"Started"</dt><dd title=created.clone()>{format::day(&created, board.clock.get_untracked())} " " {format::clock_time(&created)}</dd></div>
                    {(!duration.is_empty()).then(|| view! { <div><dt>"Took"</dt><dd>{duration.clone()}</dd></div> })}
                    {(detail.attempts > 1).then(|| view! { <div><dt>"Attempts"</dt><dd>{detail.attempts}</dd></div> })}
                    {(!detail.place.is_empty()).then(|| view! { <div><dt>"Where"</dt><dd>{detail.place.clone()}</dd></div> })}
                    {(detail.rounds > 0).then(|| view! { <div><dt>"Sent back"</dt><dd>{detail.rounds} {if detail.rounds == 1 { " time" } else { " times" }}</dd></div> })}
                    <div><dt>"Tokens"</dt><dd>{format::tokens(detail.tokens_in)} " in · " {format::tokens(detail.tokens_out)} " out"</dd></div>
                    <div><dt>"Cost"</dt><dd>{format::money(detail.cost)}</dd></div>
                </dl>
                {detail.parent.clone().map(|p| view! {
                    <p class="relation">"Part of " <a href=format!("/task/{}", api::encode(&p.id))>{p.goal}</a></p>
                })}
                {(detail.goal_id.is_some() || detail.activation.is_some()).then(|| view! {
                    <div class="row-links up-links">
                        {detail.goal_id.clone().map(|g| view! { <span class="muted">"Serves"</span> <LinkPill href=parts::goal_href(&g) lane="vault" label=g.clone()/> })}
                        {detail.activation.clone().map(|a| view! { <span class="muted">"Started by"</span> <LinkPill href=parts::activation_href(&a) lane="principal" label=a.clone()/> })}
                    </div>
                })}
            </header>
            <Pinned detail=detail.clone()/>
        })
    };
    view! {
        {header}
        {move || live.get().map(|id| view! { <LiveBar id/> })}
        <ol class="story">
            <For each=move || steps.get() key=parts::content_key let:step>
                <StoryItem step who=who.get_untracked()/>
            </For>
            <Show when=move || !writing.with(String::is_empty)>
                <li class="step said live">
                    <div class="who">"Writing now"</div>
                    <div class="md streaming">{move || writing.get()}</div>
                </li>
            </Show>
        </ol>
    }
}

/// The running task's step, with Stop and Message, kept at the top of the story.
#[component]
fn LiveBar(id: String) -> impl IntoView {
    let board = expect_context::<Board>();
    let process = Memo::new({
        let id = id.clone();
        move |_| board.now.with(|n| n.processes.iter().find(|p| p.id == id).cloned())
    });
    let message = RwSignal::new(String::new());
    let stop_id = id.clone();
    let send = move || {
        let text = message.get_untracked();
        if !text.trim().is_empty() {
            api::act(board, Action::Message { process: id.clone(), text }, move |ok| {
                if ok {
                    message.set(String::new());
                }
            });
        }
    };
    let send_key = send.clone();
    let confirm = RwSignal::new(false);
    view! {
        <div class="live-bar">
            <div class="live-step">{move || process.get().map(|process| view! { <StepLine process/> })}</div>
            <div class="live-actions">
                <input
                    type="text"
                    placeholder="Message Lucia"
                    prop:value=move || message.get()
                    on:input=move |ev| message.set(event_target_value(&ev))
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        if ev.key() == "Enter" {
                            send_key();
                        }
                    }
                />
                <button on:click=move |_| send()>"Send"</button>
                <button class="danger-quiet" on:click=move |_| {
                    if confirm.get_untracked() {
                        confirm.set(false);
                        api::act(board, Action::Stop { process: stop_id.clone() }, |_| {});
                    } else {
                        confirm.set(true);
                    }
                }>{move || if confirm.get() { "Stop it?" } else { "Stop" }}</button>
            </div>
        </div>
    }
}

#[component]
fn StoryItem(step: StoryStep, who: String) -> impl IntoView {
    match step {
        StoryStep::Prompt { at, text, from } => view! {
            <li class="step prompt">
                <div class="who">"Asked from " {from} <time>{format::clock_time(&at)}</time></div>
                <div class="md" inner_html=markdown(&text)></div>
            </li>
        }
        .into_any(),
        StoryStep::Said { at, model, text, thinking, tokens_in, tokens_out, cost, error } => view! {
            <li class="step said">
                <div class="who">{who} <time>{format::clock_time(&at)}</time></div>
                {(!thinking.trim().is_empty()).then(|| view! {
                    <details class="thinking">
                        <summary>"Reasoning"</summary>
                        <div class="md" inner_html=markdown(&thinking)></div>
                    </details>
                })}
                {(!text.trim().is_empty()).then(|| view! { <div class="md" inner_html=markdown(&text)></div> })}
                {error.map(|e| view! { <p class="error">{e}</p> })}
                <div class="meta">{model} " · " {format::tokens(tokens_in)} " in · " {format::tokens(tokens_out)} " out · " {format::money(cost)}</div>
            </li>
        }
        .into_any(),
        StoryStep::Tool { at, name, summary, arguments, output, truncated, is_error, duration_ms, device, .. } => {
            let mark = match (&output, is_error) {
                (None, _) => ("…", "run"),
                (Some(_), true) => ("✗", "bad"),
                (Some(_), false) => ("✓", "good"),
            };
            view! {
                <li class="step tool">
                    <details>
                        <summary>
                            <span class=format!("mark {}", mark.1)>{mark.0}</span>
                            <b>{name}</b>
                            {device.map(|d| view! { <span class="chip place" title="Carried out on">{d}</span> })}
                            <span class="summary">{summary}</span>
                            <span class="took">{duration_ms.map(format::seconds)}</span>
                            <time>{format::clock_time(&at)}</time>
                        </summary>
                        <h4>"Arguments"</h4>
                        <pre>{arguments}</pre>
                        <h4>"Output"</h4>
                        {match output {
                            Some(output) if output.is_empty() => view! { <p class="quiet">"(nothing)"</p> }.into_any(),
                            Some(output) => view! { <pre class:bad=is_error>{output}</pre> }.into_any(),
                            None => view! { <p class="quiet">"Still running."</p> }.into_any(),
                        }}
                        {truncated.then(|| view! { <p class="hint">"Shortened here; the session file has all of it."</p> })}
                    </details>
                </li>
            }
            .into_any()
        }
        StoryStep::Steer { at, text } => view! {
            <li class="step callout steer">
                <div class="who"><span class="label">"Observer steered"</span> <time>{format::clock_time(&at)}</time></div>
                <div class="md" inner_html=markdown(&text)></div>
            </li>
        }
        .into_any(),
        StoryStep::Review { at, activation, accepted, summary } => view! {
            <li class=format!("step verdict {}", if accepted { "good" } else { "bad" })>
                <div class="who">
                    <span class=format!("kind {}", if accepted { "good" } else { "bad" })>{if accepted { "Accepted" } else { "Sent back" }}</span>
                    <span>"The Principal's review"</span>
                    <LinkPill href=parts::activation_href(&activation) lane="principal" label=activation.clone()/>
                    <time>{format::clock_time(&at)}</time>
                </div>
                {(!summary.trim().is_empty()).then(|| view! { <div class="md" inner_html=markdown(&summary)></div> })}
            </li>
        }
        .into_any(),
        StoryStep::Note { at, tone, title, detail } => view! {
            <li class=format!("step note {}", tone_class(tone))>
                <span class="note-title">{title}</span>
                {(!detail.is_empty()).then(|| view! { <span class="note-detail">{detail}</span> })}
                <time>{format::clock_time(&at)}</time>
            </li>
        }
        .into_any(),
        StoryStep::Child { at, task, state } => view! {
            <li class="step child">
                <span class="label">"Handed to a specialist"</span>
                <a href=format!("/task/{}", api::encode(&task.id))>{task.goal}</a>
                <span class=format!("kind {}", state_class(&state))>{capitalize(&state)}</span>
                <time>{format::clock_time(&at)}</time>
            </li>
        }
        .into_any(),
        StoryStep::Ended { at, status, text, answer } => view! {
            <li class=format!("step ended {}", state_class(&status))>
                <div class="who">
                    <span class=format!("kind {}", state_class(&status))>{capitalize(&status)}</span>
                    <span>{text}</span>
                    <time>{format::clock_time(&at)}</time>
                </div>
                {(!answer.trim().is_empty()).then(|| view! {
                    <details class="answer">
                        <summary>"The answer as delivered"</summary>
                        <div class="md" inner_html=markdown(&answer)></div>
                    </details>
                })}
            </li>
        }
        .into_any(),
    }
}

/// What the worker works under and what it reported, as they read now: the goal's plan, the
/// workspace's brief, findings, and report, and the evidence manifest the Principal reviews with
/// the report (decision D87).
#[component]
fn Pinned(detail: TaskDetail) -> impl IntoView {
    let TaskDetail {
        plan,
        workspace,
        brief,
        findings,
        report,
        manifest,
        verified_refs,
        unknown_refs,
        unchecked_change,
        ..
    } = detail;
    let evidence = unchecked_change || !verified_refs.is_empty() || !unknown_refs.is_empty();
    if plan.is_none() && brief.is_none() && findings.is_none() && report.is_none() && manifest.is_none() && !evidence {
        return None;
    }
    let in_workspace = move |name: &str| workspace.as_ref().map(|w| format!("{}/{name}", w.trim_end_matches('/')));
    Some(view! {
        {unchecked_change.then(|| view! {
            <p class="evidence-warning" role="alert">
                <span class="banner-mark" aria-hidden="true">"!"</span>
                "Changed a file and reported Done without a check after it."
            </p>
        })}
        {(!verified_refs.is_empty() || !unknown_refs.is_empty()).then(|| view! {
            <div class="row-links evidence-refs">
                <span class="muted">"Cited calls"</span>
                {verified_refs.into_iter().map(|r| view! {
                    <span class="kind good" title="Matches a call of this task">{r}</span>
                }).collect_view()}
                {unknown_refs.into_iter().map(|r| view! {
                    <span class="kind warn" title="Matches no call of this task: unverified">{r} " ?"</span>
                }).collect_view()}
            </div>
        })}
        <div class="pinned">
            {plan.map(|plan| view! { <TextDrawer title="Project plan".to_string() text=plan.text path=Some(plan.path)/> })}
            {brief.map(|text| view! { <TextDrawer title="Brief".to_string() text path=in_workspace("brief.md")/> })}
            {findings.map(|text| view! { <TextDrawer title="Findings".to_string() text path=in_workspace("findings.md")/> })}
            {report.map(|text| view! { <TextDrawer title="Report".to_string() text path=in_workspace("report.md") open=true/> })}
            {manifest.map(|text| view! {
                <details class="drawer manifest">
                    <summary><span class="drawer-title">"Evidence manifest"</span> <span class="drawer-path">"drawn by the harness"</span></summary>
                    <pre>{text}</pre>
                </details>
            })}
        </div>
    })
}
