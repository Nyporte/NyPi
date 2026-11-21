//! Pieces several screens share: where a task, an activation, a goal, or a vault note is on the
//! board (the links between them), the colour of a state or a priority, the card of a stimulus
//! the Principal gave chat, a vault note read when its drawer is opened, and what a plane is
//! doing this moment.

use std::hash::{Hash, Hasher};

use leptos::prelude::*;
use leptos::task::spawn_local;
use nypi_board_proto::{GoalCard, PlaneLive, StimulusView, VaultNoteText};
use serde::Serialize;

use crate::format::{capitalize, markdown};
use crate::{Board, api};

pub fn task_href(id: &str) -> String {
    format!("/task/{}", api::encode(id))
}

pub fn activation_href(id: &str) -> String {
    format!("/activation/{}", api::encode(id))
}

pub fn goal_href(id: &str) -> String {
    format!("/vault?goal={}", api::encode(id))
}

pub fn note_href(path: &str) -> String {
    format!("/vault?path={}", api::encode(path))
}

/// Where a goal's plan is in the vault: the note the goal names, else `plan.md` in its project.
pub fn plan_path(goal: &GoalCard) -> Option<String> {
    let named = goal.plan.as_ref().filter(|plan| !plan.contains('\n') && plan.trim_end().ends_with(".md"));
    named.cloned().or_else(|| goal.project.as_ref().map(|project| format!("{}/plan.md", project.trim_end_matches('/'))))
}

/// A goal's plan as the card carries it, when it carries the text rather than where it is.
pub fn plan_text(goal: &GoalCard) -> Option<String> {
    goal.plan.clone().filter(|plan| plan.contains('\n') || !plan.trim_end().ends_with(".md"))
}

/// A key that changes when the value does, so an unchanged item keeps its place (and whatever the
/// owner opened in it) while a list is replaced as a whole.
pub fn content_key<T: Serialize>(value: &T) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(value).unwrap_or_default().hash(&mut hasher);
    hasher.finish()
}

/// The colour of a task's or an activation's state.
pub fn state_class(state: &str) -> &'static str {
    match state {
        "done" | "finished" => "good",
        "blocked" | "stalled" | "continuing" | "suspended" => "warn",
        "failed" | "interrupted" | "cancelled" => "bad",
        "running" | "active" => "run",
        _ => "plain",
    }
}

/// `P0 User` is p0, and so on; anything else is shown plain.
pub fn priority_class(priority: &str) -> &'static str {
    match priority.get(..2).map(str::to_ascii_uppercase).as_deref() {
        Some("P0") => "p0",
        Some("P1") => "p1",
        Some("P2") => "p2",
        _ => "",
    }
}

#[component]
pub fn PriorityPill(priority: String) -> impl IntoView {
    let class = format!("prio {}", priority_class(&priority));
    let label = if priority.is_empty() { "P?".to_string() } else { priority };
    view! { <span class=class>{label}</span> }
}

/// A link to another object of the board, drawn as a small pill of the object's lane.
#[component]
pub fn LinkPill(href: String, lane: &'static str, label: String) -> impl IntoView {
    view! { <a class=format!("pill {lane}") href=href>{label}</a> }
}

/// What the Principal asked chat to tell the owner.
#[component]
pub fn StimulusCard(stimulus: StimulusView, #[prop(optional)] link_activation: bool) -> impl IntoView {
    let StimulusView { activation, kind, headline, must_convey, ask_user, vault_ref } = stimulus;
    view! {
        <div class="stimulus">
            <div class="stimulus-head">
                <span class="kind lane-principal">{capitalize(&kind)}</span>
                <strong>{headline}</strong>
                {(link_activation && !activation.is_empty()).then(|| view! {
                    <LinkPill href=activation_href(&activation) lane="principal" label=activation.clone()/>
                })}
            </div>
            {(!must_convey.is_empty()).then(|| view! {
                <ul class="convey">
                    {must_convey.into_iter().map(|item| view! { <li>{item}</li> }).collect_view()}
                </ul>
            })}
            {ask_user.map(|ask| view! { <p class="ask">"Ask the user: " {ask}</p> })}
            {vault_ref.map(|path| view! {
                <p class="ref">"Details: " <LinkPill href=note_href(&path) lane="vault" label=path.clone()/></p>
            })}
        </div>
    }
}

/// A vault note in a drawer, read from the board the first time the drawer is opened.
#[component]
pub fn NoteDrawer(path: String, title: String, #[prop(optional)] open: bool) -> impl IntoView {
    let board = expect_context::<Board>();
    let text = RwSignal::new(None::<Result<String, String>>);
    let requested = RwSignal::new(false);
    let load = {
        let path = path.clone();
        move || {
            if requested.get_untracked() {
                return;
            }
            requested.set(true);
            let path = path.clone();
            spawn_local(async move {
                let found = api::get::<VaultNoteText>(board, &format!("/api/vault?path={}", api::encode(&path))).await;
                text.set(Some(found.map(|note| note.text)));
            });
        }
    };
    if open {
        load();
    }
    let href = note_href(&path);
    view! {
        <details class="drawer" open=open on:toggle=move |_| load()>
            <summary>
                <span class="drawer-title">{title}</span>
                <a class="drawer-path" href=href on:click=|ev| ev.stop_propagation()>{path}</a>
            </summary>
            {move || match text.get() {
                None => view! { <p class="quiet">"Reading…"</p> }.into_any(),
                Some(Ok(text)) if text.trim().is_empty() => view! { <p class="quiet">"(empty)"</p> }.into_any(),
                Some(Ok(text)) => view! { <div class="md" inner_html=markdown(&text)></div> }.into_any(),
                Some(Err(error)) => view! { <p class="quiet">"Could not read it: " {error}</p> }.into_any(),
            }}
        </details>
    }
}

/// Text already at hand, in a drawer.
#[component]
pub fn TextDrawer(
    title: String,
    text: String,
    #[prop(optional_no_strip)] path: Option<String>,
    #[prop(optional)] open: bool,
) -> impl IntoView {
    view! {
        <details class="drawer" open=open>
            <summary>
                <span class="drawer-title">{title}</span>
                {path.map(|path| view! {
                    <a class="drawer-path" href=note_href(&path) on:click=|ev| ev.stop_propagation()>{path.clone()}</a>
                })}
            </summary>
            {if text.trim().is_empty() {
                view! { <p class="quiet">"(empty)"</p> }.into_any()
            } else {
                view! { <div class="md" inner_html=markdown(&text)></div> }.into_any()
            }}
        </details>
    }
}

/// The tone of a plane's step, in the classes the processes' step lines use.
fn plane_tone(step: &str) -> &'static str {
    match step {
        "Thinking" => "think",
        "Writing" => "write",
        _ if step.starts_with("Reading ") || step.starts_with("Calling ") => "tool",
        _ => "wait",
    }
}

/// The last `max` characters of `text` (all of it when `max` is 0), marked when cut.
pub fn tail_of(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if max == 0 || count <= max {
        return text.to_string();
    }
    let cut: String = text.chars().skip(count - max).collect();
    format!("…{}", cut.trim_start())
}

/// What chat or the Principal is doing this moment: the step, and the end of what it writes.
/// Nothing while the plane is idle.
#[component]
pub fn PlaneLiveLine(
    #[prop(into)] live: Signal<Option<PlaneLive>>,
    /// How much of the tail to show; all of it when 0.
    #[prop(optional)]
    max: usize,
) -> impl IntoView {
    move || {
        live.get().map(|PlaneLive { step, tail, .. }| {
            let tail = tail_of(tail.trim_end(), max);
            let line = format!("step-line {}", plane_tone(&step));
            view! {
                <div class="plane-live" class:long=max == 0>
                    <div class=line>
                        <span class="pulse" aria-hidden="true"></span>
                        <span class="what">{step}</span>
                    </div>
                    {(!tail.is_empty()).then(|| view! { <div class="tail"><p>{tail}</p></div> })}
                </div>
            }
        })
    }
}

/// Pretty JSON when `text` is JSON, else the text as it is.
pub fn pretty(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .filter(|value| value.is_object() || value.is_array())
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priorities_read_from_their_first_two_letters() {
        assert_eq!(priority_class("P0 User"), "p0");
        assert_eq!(priority_class("p2 Idle"), "p2");
        assert_eq!(priority_class(""), "");
    }

    #[test]
    fn a_tail_keeps_the_end_of_the_text() {
        assert_eq!(tail_of("short", 200), "short");
        assert_eq!(tail_of("abcdef", 3), "…def");
        assert_eq!(tail_of("abcdef", 0), "abcdef");
        assert_eq!(tail_of("äöü", 2), "…öü");
    }

    #[test]
    fn pretty_leaves_plain_text_alone() {
        assert_eq!(pretty("hello"), "hello");
        assert_eq!(pretty("{\"a\":1}"), "{\n  \"a\": 1\n}");
    }
}
