//! The Chat screen (decision D86): the hub's one conversation as chat's turns, each one open to
//! inspection: who it came from (the owner, with where they were, or a stimulus from the
//! Principal), the memories recall attached, the reasoning, every read-only call with its time and
//! output, what chat handed up to the Principal, and the reply. Live while open, with the turn in
//! flight as it is written; the box at the bottom says something to Lucia as the owner.

use leptos::prelude::*;
use leptos::task::spawn_local;
use nypi_board_proto::{Action, ChatToolCall, ChatTurn, ChatView, LiveEvent, SignalPill};

use crate::format::{self, markdown};
use crate::parts::{self, LinkPill, PlaneLiveLine, StimulusCard};
use crate::{Board, api};

#[component]
pub fn ChatPage() -> impl IntoView {
    let board = expect_context::<Board>();
    let chat = RwSignal::new(None::<ChatView>);
    let failed = RwSignal::new(None::<String>);
    let show_skipped = RwSignal::new(false);
    spawn_local(async move {
        match api::get::<ChatView>(board, "/api/chat").await {
            Ok(found) => {
                // A turn already in flight shows at once, not at its next step (decision D89).
                api::show_plane(board, found.live.clone(), "chat");
                chat.set(Some(found));
                to_bottom(true);
            }
            Err(error) => failed.set(Some(error)),
        }
    });
    api::live(board, "/api/live?chat=1", move |event| match event {
        LiveEvent::Chat { chat: found } => {
            // Follow the conversation only when the user is already at its end.
            let follow = near_bottom();
            api::show_plane(board, found.live.clone(), "chat");
            chat.set(Some(*found));
            to_bottom(follow);
        }
        LiveEvent::Plane { live } if live.plane == "chat" => to_bottom(near_bottom()),
        LiveEvent::Plane { .. }
        | LiveEvent::Now { .. }
        | LiveEvent::Task { .. }
        | LiveEvent::Delta { .. }
        | LiveEvent::Activation { .. } => {}
    });
    let turns = Memo::new(move |_| {
        let show = show_skipped.get();
        chat.with(|c| {
            c.as_ref()
                .map(|c| c.turns.iter().filter(|t| show || !t.skipped).cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    let skipped =
        Memo::new(move |_| chat.with(|c| c.as_ref().map_or(0, |c| c.turns.iter().filter(|t| t.skipped).count())));
    view! {
        <div class="screen-head">
            <div>
                <h1>"Chat"</h1>
                <p class="muted">
                    "The one conversation, turn by turn. "
                    {move || chat.with(|c| c.as_ref().and_then(|c| c.model.clone())).map(|m| view! { <span class="chip">{m}</span> })}
                </p>
            </div>
            <label class="switch">
                <input type="checkbox" prop:checked=move || show_skipped.get() on:change=move |ev| show_skipped.set(event_target_checked(&ev))/>
                <span>"Show skipped"</span>
                <span class="count">{move || skipped.get()}</span>
            </label>
        </div>
        {move || failed.get().map(|error| view! { <p class="empty">"Could not read the conversation: " {error}</p> })}
        {move || match chat.with(|c| c.as_ref().map(|c| c.turns.is_empty())) {
            None if failed.with(Option::is_none) => Some(view! { <p class="loading">"Reading the conversation…"</p> }.into_any()),
            Some(true) => Some(view! { <p class="quiet">"Nothing said yet. Say something below."</p> }.into_any()),
            _ => None,
        }}
        <div class="turns">
            <For each=move || turns.get() key=parts::content_key let:turn>
                <TurnCard turn/>
            </For>
            <Show when=move || board.chat_live.with(Option::is_some)>
                <article class="turn in-flight">
                    <header class="turn-head">
                        <span class="origin owner">"In flight"</span>
                    </header>
                    <PlaneLiveLine live=board.chat_live/>
                </article>
            </Show>
        </div>
        <Composer/>
    }
}

fn near_bottom() -> bool {
    let Some(window) = web_sys::window() else { return false };
    let height = window.document().and_then(|d| d.document_element()).map_or(0, |e| e.scroll_height()) as f64;
    let seen = window.scroll_y().unwrap_or(0.0) + window.inner_height().ok().and_then(|h| h.as_f64()).unwrap_or(0.0);
    height - seen < 240.0
}

/// Scroll to the newest turn once the page has drawn it.
fn to_bottom(go: bool) {
    if go {
        request_animation_frame(|| {
            if let Some(window) = web_sys::window() {
                window.scroll_to_with_x_and_y(0.0, 1e9);
            }
        });
    }
}

#[component]
fn TurnCard(turn: ChatTurn) -> impl IntoView {
    let ChatTurn {
        at,
        origin,
        focus,
        user_text,
        stimulus,
        recalled_memories,
        thinking,
        tool_calls,
        signals,
        reply_text,
        skipped,
        tokens,
        cost,
        ..
    } = turn;
    let from_principal = origin == "principalStimulus" || stimulus.is_some();
    let origin_badge = if from_principal {
        let activation = stimulus.as_ref().map(|s| s.activation.clone()).filter(|a| !a.is_empty());
        view! {
            <span class="origin principal">"From the Principal"</span>
            {activation.map(|a| view! { <LinkPill href=parts::activation_href(&a) lane="principal" label=a.clone()/> })}
        }
        .into_any()
    } else {
        view! {
            <span class="origin owner">"You"</span>
            {focus.map(|f| view! { <span class="chip place" title="Where you were">{f}</span> })}
        }
        .into_any()
    };
    let stimulus_drawer = stimulus.map(|stimulus| {
        view! {
            <details class="drawer lane-principal">
                <summary>
                    <span class="drawer-title">"Stimulus: " {stimulus.headline.clone()}</span>
                </summary>
                <StimulusCard stimulus/>
            </details>
        }
    });
    let memories = (!recalled_memories.is_empty()).then(|| {
        let count = recalled_memories.len();
        view! {
            <details class="drawer">
                <summary><span class="drawer-title">"Recalled memories"</span> <span class="count">{count}</span></summary>
                <ul class="memories">{recalled_memories.into_iter().map(|m| view! { <li>{m}</li> }).collect_view()}</ul>
            </details>
        }
    });
    let reasoning = (!thinking.trim().is_empty()).then(|| {
        view! {
            <details class="thinking">
                <summary>"Reasoning"</summary>
                <div class="md" inner_html=markdown(&thinking)></div>
            </details>
        }
    });
    view! {
        <article class="turn" class:skipped=skipped>
            <header class="turn-head">
                {origin_badge}
                {skipped.then(|| view! { <span class="kind" title="Already covered: nothing was shown">"SKIP"</span> })}
                <time title=at.clone()>{format::clock_time(&at)}</time>
            </header>
            {user_text.filter(|t| !t.trim().is_empty()).map(|text| view! { <div class="md bubble owner" inner_html=markdown(&text)></div> })}
            {stimulus_drawer}
            {memories}
            {reasoning}
            {(!tool_calls.is_empty()).then(|| view! {
                <ul class="calls">{tool_calls.into_iter().map(|call| view! { <CallRow call/> }).collect_view()}</ul>
            })}
            {(!signals.is_empty()).then(|| view! {
                <div class="signals">{signals.into_iter().map(|signal| view! { <SignalLink signal/> }).collect_view()}</div>
            })}
            {(!reply_text.trim().is_empty()).then(|| view! {
                <div class="who-line">"Lucia"</div>
                <div class="md bubble reply" inner_html=markdown(&reply_text)></div>
            })}
            {(tokens > 0 || cost > 0.0).then(|| view! {
                <div class="meta">{format::tokens(tokens)} " tokens · " {format::money(cost)}</div>
            })}
        </article>
    }
}

#[component]
fn CallRow(call: ChatToolCall) -> impl IntoView {
    let ChatToolCall { name, arguments, duration_ms, output, timed_out, is_error } = call;
    let (mark, tone) = if timed_out || is_error { ("✗", "bad") } else { ("✓", "good") };
    let summary = arguments.split_whitespace().collect::<Vec<_>>().join(" ");
    view! {
        <li class="call" class:bad=is_error || timed_out>
            <details>
                <summary>
                    <span class=format!("mark {tone}")>{mark}</span>
                    <b>{name}</b>
                    <span class="summary">{summary}</span>
                    {timed_out.then(|| view! { <span class="kind warn">"timed out"</span> })}
                    <span class="took">{format!("{duration_ms} ms")}</span>
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
        </li>
    }
}

#[component]
fn SignalLink(signal: SignalPill) -> impl IntoView {
    let SignalPill { topic, urgency, activation } = signal;
    let body = view! {
        <span class="signal-mark" aria-hidden="true">"↑"</span>
        <span class="signal-label">"Signalled the Principal"</span>
        {(!urgency.is_empty()).then(|| view! { <span class="kind warn">{urgency.clone()}</span> })}
        <span class="signal-topic">{topic}</span>
    };
    if activation.is_empty() {
        view! { <span class="signal">{body}</span> }.into_any()
    } else {
        view! { <a class="signal" href=parts::activation_href(&activation) title=activation.clone()>{body}</a> }
            .into_any()
    }
}

/// The box that says something to Lucia, as the owner.
#[component]
fn Composer() -> impl IntoView {
    let board = expect_context::<Board>();
    let text = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let send = move || {
        let said = text.get_untracked();
        if said.trim().is_empty() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        api::act(board, Action::Say { text: said }, move |ok| {
            busy.set(false);
            if ok {
                text.set(String::new());
            }
        });
    };
    view! {
        <form class="composer" on:submit=move |ev| {
            ev.prevent_default();
            send();
        }>
            <textarea
                rows="2"
                placeholder="Say something to Lucia (Enter sends, Shift+Enter for a new line)"
                prop:value=move || text.get()
                on:input=move |ev| text.set(event_target_value(&ev))
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    if ev.key() == "Enter" && !ev.shift_key() && !ev.is_composing() {
                        ev.prevent_default();
                        send();
                    }
                }
            ></textarea>
            <button class="primary" type="submit" disabled=move || busy.get() || text.with(|t| t.trim().is_empty())>
                {move || if busy.get() { "Sending…" } else { "Send" }}
            </button>
        </form>
    }
}
