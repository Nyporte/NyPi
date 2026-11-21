//! The NyPi board's page (decision D56, design in docs/board.md), compiled to WebAssembly and
//! served by `nypi board`. The screens in the bar (decision D86): Now (what needs the owner, the
//! three planes, the tasks under their goals, the devices), Chat (the conversation turn by turn),
//! Principal (its stack, its queue, and every activation; an activation's story on its own page),
//! Vault (the goals and Lucia's notes), System (the whole program as a map, decision D65), and
//! Activity (everything that happened). A task's story has a page of its own. What is live comes
//! over one server-sent event stream per screen; actions go back as JSON posts carrying the
//! board's CSRF value.

mod activity;
mod api;
mod chat;
mod format;
mod now;
mod parts;
mod principal;
pub mod system;
mod task;
mod vault;

use leptos::prelude::*;
use leptos_router::components::{Route, Router, Routes};
use leptos_router::hooks::use_location;
use leptos_router::path;
use nypi_board_proto::{Now, PlaneLive};

/// What every screen shares.
#[derive(Clone, Copy)]
pub struct Board {
    pub now: RwSignal<Now>,
    /// The first home screen arrived.
    pub loaded: RwSignal<bool>,
    /// The live stream is open.
    pub connected: RwSignal<bool>,
    pub locked: RwSignal<bool>,
    pub csrf: RwSignal<String>,
    /// Milliseconds since the epoch, ticking each second, so "3 min ago" stays true.
    pub clock: RwSignal<f64>,
    /// The last action's result: (ok, message, when).
    pub toast: RwSignal<Option<(bool, String, f64)>>,
    /// What chat and the Principal are doing this moment (decision D88), kept apart from `now`
    /// because it changes several times a second and would redraw every card that reads `now`.
    pub chat_live: RwSignal<Option<PlaneLive>>,
    pub principal_live: RwSignal<Option<PlaneLive>>,
}

#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    let board = Board {
        now: RwSignal::new(Now::default()),
        loaded: RwSignal::new(false),
        connected: RwSignal::new(false),
        locked: RwSignal::new(false),
        csrf: RwSignal::new(String::new()),
        clock: RwSignal::new(format::now_ms()),
        toast: RwSignal::new(None),
        chat_live: RwSignal::new(None),
        principal_live: RwSignal::new(None),
    };
    provide_context(board);
    api::hello(board);
    let _ = set_interval_with_handle(move || board.clock.set(format::now_ms()), std::time::Duration::from_secs(1));
    // The tab says how many things wait, so a glance at the tab bar is enough.
    Effect::new(move |_| {
        let waiting = board.now.with(|n| n.inbox.len());
        let title = if waiting > 0 { format!("({waiting}) NyPi board") } else { "NyPi board".to_string() };
        if let Some(document) = web_sys::window().and_then(|w| w.document()) {
            document.set_title(&title);
        }
    });
    view! {
        <Router>
            <TopBar/>
            <main>
                <Show when=move || board.locked.get()>
                    <section class="locked">
                        <h1>"Locked"</h1>
                        <p>"Open the link " <code>"nypi board"</code> " printed; it carries the token."</p>
                    </section>
                </Show>
                <Routes fallback=|| view! { <p class="empty">"There is no such screen."</p> }>
                    <Route path=path!("/") view=now::NowPage/>
                    <Route path=path!("/chat") view=chat::ChatPage/>
                    <Route path=path!("/principal") view=principal::PrincipalPage/>
                    <Route path=path!("/activation/:id") view=principal::ActivationPage/>
                    <Route path=path!("/task/:id") view=task::TaskPage/>
                    <Route path=path!("/vault") view=vault::VaultPage/>
                    <Route path=path!("/activity") view=activity::ActivityPage/>
                    <Route path=path!("/system") view=system::SystemPage/>
                </Routes>
            </main>
            <Toast/>
        </Router>
    }
}

#[component]
fn TopBar() -> impl IntoView {
    let board = expect_context::<Board>();
    let location = use_location();
    let here = move |prefix: &'static str| {
        let path = location.pathname.get();
        match prefix {
            "/" => path == "/" || path.starts_with("/task/"),
            "/principal" => path.starts_with("/principal") || path.starts_with("/activation/"),
            _ => path.starts_with(prefix),
        }
    };
    let waiting = move || board.now.with(|n| n.inbox.len());
    let running = move || board.now.with(|n| n.processes.iter().filter(|p| p.run_started.is_some()).count());
    let principal_busy = move || board.now.with(|n| n.planes.principal.active.is_some());
    view! {
        <header class="topbar">
            <a class="brand" href="/">
                <span class="mark" aria-hidden="true">"◆"</span>
                "NyPi"
            </a>
            <nav>
                <a href="/" class:here=move || here("/")>
                    "Now"
                    <Show when=move || { waiting() > 0 }>
                        <span class="badge" title="Needs you">{waiting}</span>
                    </Show>
                </a>
                <a href="/chat" class:here=move || here("/chat")>"Chat"</a>
                <a href="/principal" class:here=move || here("/principal")>
                    "Principal"
                    <Show when=move || principal_busy()>
                        <span class="dot-live" title="An activation is running"></span>
                    </Show>
                </a>
                <a href="/vault" class:here=move || here("/vault")>"Vault"</a>
                <a href="/system" class:here=move || here("/system")>"System"</a>
                <a href="/activity" class:here=move || here("/activity")>"Activity"</a>
            </nav>
            <span
                class="link-state"
                class:up=move || board.connected.get()
                title=move || if board.connected.get() { "Live" } else { "Reconnecting…" }
            >
                <span class="dot"></span>
                {move || match (board.connected.get(), running()) {
                    (false, _) => "Reconnecting".to_string(),
                    (true, 0) => "Live".to_string(),
                    (true, n) => format!("{n} running"),
                }}
            </span>
        </header>
    }
}

#[component]
fn Toast() -> impl IntoView {
    let board = expect_context::<Board>();
    let shown = move || board.toast.get().filter(|(_, _, at)| board.clock.get() - at < 5000.0);
    view! {
        <div class="toast-area" aria-live="polite">
            {move || {
                shown()
                    .map(|(ok, message, _)| {
                        view! {
                            <div class="toast" class:bad=!ok>
                                {message}
                            </div>
                        }
                    })
            }}
        </div>
    }
}
