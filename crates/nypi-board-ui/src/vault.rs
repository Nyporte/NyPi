//! The Vault screen (decision D86): the goals by term, each with its due date, next step, the
//! tasks that served it, and its plan; then Lucia's notes by folder, any of which opens in the
//! reader at the top. `/vault?path=<note>` opens a note, `/vault?goal=<id>` points at a goal.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_query_map;
use nypi_board_proto::{GoalCard, VaultFolder, VaultNoteText, VaultView};

use crate::format::{self, capitalize, markdown};
use crate::parts::{self, NoteDrawer, TextDrawer};
use crate::{Board, api};

const TERMS: [(&str, &str); 3] = [("short", "Short term"), ("medium", "Medium term"), ("long", "Long term")];

#[component]
pub fn VaultPage() -> impl IntoView {
    let board = expect_context::<Board>();
    api::live(board, "/api/live", |_| {});
    let query = use_query_map();
    let path = Memo::new(move |_| query.with(|q| q.get("path")).filter(|p| !p.is_empty()));
    let goal = Memo::new(move |_| query.with(|q| q.get("goal")).filter(|g| !g.is_empty()));
    let vault = RwSignal::new(None::<Result<VaultView, String>>);
    spawn_local(async move {
        vault.set(Some(api::get::<VaultView>(board, "/api/vault").await));
    });
    // Point at the goal asked for once the goals are drawn.
    Effect::new(move |_| {
        let (Some(goal), true) = (goal.get(), vault.with(|v| matches!(v, Some(Ok(_))))) else { return };
        request_animation_frame(move || {
            if let Some(element) =
                web_sys::window().and_then(|w| w.document()).and_then(|d| d.get_element_by_id(&goal_anchor(&goal)))
            {
                element.scroll_into_view();
            }
        });
    });
    view! {
        <div class="screen-head">
            <div>
                <h1>"Vault"</h1>
                <p class="muted">"The goals Lucia works toward, and the notes she keeps."</p>
            </div>
        </div>
        {move || path.get().map(|path| view! { <Reader path/> })}
        {move || match vault.get() {
            None => view! { <p class="loading">"Reading the vault…"</p> }.into_any(),
            Some(Err(error)) => view! { <p class="empty">"Could not read the vault: " {error}</p> }.into_any(),
            Some(Ok(VaultView { goals, folders })) => view! {
                <Goals goals pointed=goal.get()/>
                <Folders folders/>
            }
            .into_any(),
        }}
    }
}

fn goal_anchor(id: &str) -> String {
    format!("goal-{id}")
}

/// One note, rendered, above everything else.
#[component]
fn Reader(path: String) -> impl IntoView {
    let board = expect_context::<Board>();
    let note = RwSignal::new(None::<Result<VaultNoteText, String>>);
    {
        let path = path.clone();
        spawn_local(async move {
            note.set(Some(api::get::<VaultNoteText>(board, &format!("/api/vault?path={}", api::encode(&path))).await));
        });
    }
    request_animation_frame(|| {
        if let Some(window) = web_sys::window() {
            window.scroll_to_with_x_and_y(0.0, 0.0);
        }
    });
    view! {
        <article class="card reader lane-vault">
            <header>
                <span class="chip">"Note"</span>
                <span class="title reader-path">{path.clone()}</span>
                {move || note.with(|n| match n {
                    Some(Ok(n)) if !n.modified.is_empty() => Some(format!("changed {}", format::ago(&n.modified, board.clock.get_untracked()))),
                    _ => None,
                }).map(|m| view! { <span class="age">{m}</span> })}
                <a class="button plain" href="/vault">"Close"</a>
            </header>
            {move || match note.get() {
                None => view! { <p class="quiet">"Reading…"</p> }.into_any(),
                Some(Err(error)) => view! { <p class="quiet">"Could not read " <code>{path.clone()}</code> ": " {error}</p> }.into_any(),
                Some(Ok(note)) if note.text.trim().is_empty() => view! { <p class="quiet">"(empty)"</p> }.into_any(),
                Some(Ok(note)) => view! { <div class="md reader-text" inner_html=markdown(&note.text)></div> }.into_any(),
            }}
        </article>
    }
}

#[component]
fn Goals(goals: Vec<GoalCard>, pointed: Option<String>) -> impl IntoView {
    if goals.is_empty() {
        return view! {
            <section class="section">
                <h2 class="heading">"Goals"</h2>
                <p class="quiet">"No goal yet."</p>
            </section>
        }
        .into_any();
    }
    let mut terms: Vec<(String, Vec<GoalCard>)> = TERMS
        .iter()
        .map(|(term, label)| (label.to_string(), goals.iter().filter(|g| g.term == *term).cloned().collect()))
        .collect();
    let other: Vec<GoalCard> = goals.into_iter().filter(|g| !TERMS.iter().any(|(term, _)| g.term == *term)).collect();
    terms.push(("Other".to_string(), other));
    view! {
        {terms
            .into_iter()
            .filter(|(_, goals)| !goals.is_empty())
            .map(|(label, goals)| {
                let count = goals.len();
                let pointed = pointed.clone();
                view! {
                    <section class="section">
                        <h2 class="heading">{label} <span class="count">{count}</span></h2>
                        <div class="cards">
                            {goals.into_iter().map(|goal| {
                                let here = pointed.as_deref() == Some(goal.id.as_str());
                                view! { <GoalItem goal here/> }
                            }).collect_view()}
                        </div>
                    </section>
                }
            })
            .collect_view()}
    }
    .into_any()
}

#[component]
fn GoalItem(goal: GoalCard, here: bool) -> impl IntoView {
    let board = expect_context::<Board>();
    let clock = board.clock.get_untracked();
    let plan = match (parts::plan_text(&goal), parts::plan_path(&goal)) {
        (Some(text), path) => Some(view! { <TextDrawer title="Plan".to_string() text path open=here/> }.into_any()),
        (None, Some(path)) => Some(view! { <NoteDrawer path title="Plan".to_string() open=here/> }.into_any()),
        (None, None) => None,
    };
    let overdue = goal.due.as_deref().and_then(format::parse).is_some_and(|due| due < clock) && !goal.done;
    view! {
        <article class="card goal-card" class:here=here id=goal_anchor(&goal.id)>
            <header>
                <span class="pill vault">{goal.id.clone()}</span>
                <span class="title">{goal.text.clone()}</span>
                {goal.done.then(|| view! { <span class="kind good">"Done"</span> })}
                {goal.due.clone().map(|due| view! {
                    <span class=format!("kind {}", if overdue { "bad" } else { "warn" }) title=due.clone()>
                        {if overdue { "Overdue " } else { "Due " }} {format::day(&due, clock)}
                    </span>
                })}
            </header>
            {goal.next.clone().map(|next| view! { <p class="next"><span class="label">"Next"</span> {next}</p> })}
            <div class="row-links">
                <span class="muted">{goal.tasks} {if goal.tasks == 1 { " task" } else { " tasks" }}</span>
                {goal.last_task.clone().map(|t| view! { <a class="pill tasks" href=parts::task_href(&t)>"latest " {t.clone()}</a> })}
                {goal.project.clone().map(|p| view! { <span class="chip">{format::short_path(&p)}</span> })}
                <span class="muted">{capitalize(&goal.term)}</span>
            </div>
            {plan}
        </article>
    }
}

#[component]
fn Folders(folders: Vec<VaultFolder>) -> impl IntoView {
    let board = expect_context::<Board>();
    view! {
        <section class="section">
            <h2 class="heading">"Notes"</h2>
            {if folders.is_empty() {
                view! { <p class="quiet">"The vault is empty."</p> }.into_any()
            } else {
                folders
                    .into_iter()
                    .map(|folder| {
                        let count = folder.notes.len();
                        let prefix = format!("{}/", folder.name.trim_end_matches('/'));
                        let open = count > 0 && count <= 12;
                        view! {
                            <details class="drawer folder" open=open>
                                <summary>
                                    <span class="drawer-title">{folder.name.clone()}</span>
                                    <span class="count">{count}</span>
                                </summary>
                                {if count == 0 {
                                    view! { <p class="quiet">"No note."</p> }.into_any()
                                } else {
                                    view! {
                                        <ul class="upcoming notes">
                                            {folder.notes.into_iter().map(|note| {
                                                let shown = note.path.strip_prefix(&prefix).unwrap_or(&note.path).to_string();
                                                let modified = note.modified.clone();
                                                view! {
                                                    <li>
                                                        <a href=parts::note_href(&note.path)>{shown}</a>
                                                        <span class="when right">{move || format::ago(&modified, board.clock.get())}</span>
                                                    </li>
                                                }
                                            }).collect_view()}
                                        </ul>
                                    }
                                        .into_any()
                                }}
                            </details>
                        }
                    })
                    .collect_view()
                    .into_any()
            }}
        </section>
    }
}
