//! Everything that happened, newest first: tasks with how they ended, alerts, approvals,
//! schedule firings, reflections, and what the owner did from the board. Filtered by kind,
//! time, and text; refreshed as runs settle.

use leptos::prelude::*;
use leptos::task::spawn_local;
use nypi_board_proto::ActivityRow;

use crate::format::{self, capitalize};
use crate::{Board, api};

const KINDS: [(&str, &str); 6] = [
    ("", "All"),
    ("task", "Tasks"),
    ("alert", "Alerts"),
    ("approval", "Approvals"),
    ("trigger", "Schedules"),
    ("board", "Your actions"),
];
const RANGES: [(u32, &str); 4] = [(24, "24 hours"), (72, "3 days"), (168, "7 days"), (720, "30 days")];

#[component]
pub fn ActivityPage() -> impl IntoView {
    let board = expect_context::<Board>();
    api::live(board, "/api/live", |_| {});
    let kind = RwSignal::new(String::new());
    let hours = RwSignal::new(24u32);
    let text = RwSignal::new(String::new());
    let rows = RwSignal::new(None::<Vec<ActivityRow>>);
    // New rows come with settled runs and alerts, which move these numbers.
    let pace = Memo::new(move |_| {
        board.now.with(|n| {
            let t = &n.today;
            (
                t.done,
                t.blocked,
                t.failed,
                t.continuing,
                n.inbox.len(),
                n.processes.iter().filter(|p| p.task.is_some()).count(),
            )
        })
    });
    Effect::new(move |_| {
        let query = format!(
            "/api/activity?hours={}&kind={}&q={}",
            hours.get(),
            api::encode(&kind.get()),
            api::encode(text.get().trim())
        );
        pace.track();
        spawn_local(async move {
            if let Ok(found) = api::get::<Vec<ActivityRow>>(board, &query).await {
                rows.set(Some(found));
            }
        });
    });
    view! {
        <div class="filters">
            <div class="chips" role="group" aria-label="Kind">
                {KINDS
                    .iter()
                    .map(|(value, label)| {
                        let value = value.to_string();
                        let here = value.clone();
                        view! {
                            <button class="chip-button" class:on=move || kind.get() == here on:click=move |_| kind.set(value.clone())>
                                {*label}
                            </button>
                        }
                    })
                    .collect_view()}
            </div>
            <div class="filter-row">
                <select on:change=move |ev| hours.set(event_target_value(&ev).parse().unwrap_or(24))>
                    {RANGES
                        .iter()
                        .map(|(value, label)| view! { <option value=value.to_string() selected=*value == 24>{*label}</option> })
                        .collect_view()}
                </select>
                <input
                    type="search"
                    placeholder="Search"
                    prop:value=move || text.get()
                    on:change=move |ev| text.set(event_target_value(&ev))
                />
            </div>
        </div>
        {move || match rows.get() {
            None => view! { <p class="loading">"Reading the record…"</p> }.into_any(),
            Some(rows) if rows.is_empty() => view! { <p class="quiet">"Nothing in this range."</p> }.into_any(),
            Some(rows) => {
                let clock = board.clock.get_untracked();
                let mut days: Vec<(String, Vec<ActivityRow>)> = Vec::new();
                for row in rows {
                    let day = format::day(&row.at, clock);
                    match days.last_mut() {
                        Some((last, group)) if *last == day => group.push(row),
                        _ => days.push((day, vec![row])),
                    }
                }
                days.into_iter()
                    .map(|(day, rows)| {
                        view! {
                            <section class="day">
                                <h2 class="heading">{day}</h2>
                                <ul class="rows">{rows.into_iter().map(|row| view! { <Row row/> }).collect_view()}</ul>
                            </section>
                        }
                    })
                    .collect_view()
                    .into_any()
            }
        }}
    }
}

fn icon(kind: &str) -> &'static str {
    match kind {
        "task" => "▸",
        "alert" => "!",
        "approval" => "?",
        "trigger" => "⏱",
        "reflection" => "✎",
        "board" => "◆",
        _ => "·",
    }
}

#[component]
fn Row(row: ActivityRow) -> impl IntoView {
    let state = row.state.clone().unwrap_or_default();
    let tone = match state.as_str() {
        "done" => "good",
        "blocked" | "stalled" => "warn",
        "failed" | "interrupted" | "cancelled" => "bad",
        "running" => "run",
        _ => "plain",
    };
    let title = match &row.task {
        Some(task) if row.kind == "task" => {
            view! { <a href=format!("/task/{}", api::encode(&task.id))>{row.title.clone()}</a> }.into_any()
        }
        _ => view! { <span>{row.title.clone()}</span> }.into_any(),
    };
    let context = match &row.task {
        Some(task) if row.kind != "task" => Some(view! {
            <a class="muted" href=format!("/task/{}", api::encode(&task.id))>{task.goal.clone()}</a>
        }),
        _ => None,
    };
    let who = [capitalize(&row.agent), row.origin.clone()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    view! {
        <li class=format!("row kind-{}", row.kind)>
            <time>{format::clock_time(&row.at)}</time>
            <span class="icon" aria-hidden="true">{icon(&row.kind)}</span>
            <div class="row-body">
                <div class="row-title">
                    {title}
                    {(!state.is_empty()).then(|| view! { <span class=format!("kind {tone}")>{capitalize(&state)}</span> })}
                </div>
                {(!row.detail.is_empty()).then(|| view! { <p class="row-detail">{row.detail.clone()}</p> })}
                {context}
                {(!who.is_empty()).then(|| view! { <span class="who-line">{who}</span> })}
            </div>
        </li>
    }
}
