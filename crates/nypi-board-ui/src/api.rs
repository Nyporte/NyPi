//! Talking to `nypi board`: JSON reads, actions with the CSRF value, and the live stream.

use std::rc::Rc;

use gloo_net::http::Request;
use leptos::prelude::*;
use leptos::task::spawn_local;
use nypi_board_proto::{Action, ActionResult, LiveEvent, Now, PlaneLive};
use send_wrapper::SendWrapper;
use serde::de::DeserializeOwned;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{EventSource, MessageEvent};

use crate::Board;
use crate::format::now_ms;

pub async fn get<T: DeserializeOwned>(board: Board, path: &str) -> Result<T, String> {
    let response = Request::get(path).send().await.map_err(|e| e.to_string())?;
    if response.status() == 401 {
        board.locked.set(true);
        return Err("locked".into());
    }
    if !response.ok() {
        return Err(format!("{} {}", response.status(), response.status_text()));
    }
    response.json::<T>().await.map_err(|e| e.to_string())
}

/// Learn the CSRF value actions must carry.
pub fn hello(board: Board) {
    spawn_local(async move {
        if let Ok(hello) = get::<serde_json::Value>(board, "/api/hello").await {
            board.csrf.set(hello["csrf"].as_str().unwrap_or_default().to_string());
        }
    });
}

/// Send an action; the result shows as a toast, and `done` hears whether it worked.
pub fn act(board: Board, action: Action, done: impl FnOnce(bool) + 'static) {
    spawn_local(async move {
        let csrf = board.csrf.get_untracked();
        let result = async {
            let request =
                Request::post("/api/action").header("x-board-csrf", &csrf).json(&action).map_err(|e| e.to_string())?;
            let response = request.send().await.map_err(|e| e.to_string())?;
            response.json::<ActionResult>().await.map_err(|e| e.to_string())
        }
        .await
        .unwrap_or_else(|error| ActionResult { ok: false, message: format!("The board did not answer: {error}") });
        // A stale CSRF value (the board restarted) is fixed by asking again.
        if !result.ok && result.message.starts_with("Reload") {
            hello(board);
        }
        board.toast.set(Some((result.ok, result.message.clone(), now_ms())));
        done(result.ok);
    });
}

/// A home screen read or sent: into the shared state, with what the planes are doing.
pub fn show_now(board: Board, now: Now) {
    show_plane(board, now.planes.chat.live.clone(), "chat");
    show_plane(board, now.planes.principal.live.clone(), "principal");
    board.now.set(now);
    board.loaded.set(true);
}

/// One plane's step, `None` (or `idle`) when it does nothing.
pub fn show_plane(board: Board, live: Option<PlaneLive>, plane: &str) {
    let live = live.filter(|live| !live.step.is_empty() && live.step != "idle");
    let slot = match plane {
        "chat" => board.chat_live,
        "principal" => board.principal_live,
        _ => return,
    };
    if slot.with_untracked(|current| *current != live) {
        slot.set(live);
    }
}

/// Follow the live stream at `url` until the calling scope ends. Home-screen updates and the
/// planes' steps go to the shared state; every event also goes to `on_event`.
pub fn live(board: Board, url: &str, on_event: impl Fn(LiveEvent) + 'static) {
    let Ok(source) = EventSource::new(url) else { return };
    let on_event = Rc::new(on_event);
    let mut listeners = Vec::new();
    for name in ["now", "task", "delta", "chat", "activation", "plane"] {
        let on_event = on_event.clone();
        let listener = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let Some(data) = event.data().as_string() else { return };
            let Ok(live) = serde_json::from_str::<LiveEvent>(&data) else { return };
            match &live {
                LiveEvent::Now { now } => show_now(board, now.clone()),
                LiveEvent::Plane { live } => show_plane(board, Some(live.clone()), &live.plane),
                LiveEvent::Task { .. }
                | LiveEvent::Delta { .. }
                | LiveEvent::Chat { .. }
                | LiveEvent::Activation { .. } => {}
            }
            on_event(live);
        });
        let _ = source.add_event_listener_with_callback(name, listener.as_ref().unchecked_ref());
        listeners.push(listener);
    }
    let opened = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| board.connected.set(true));
    // The browser reconnects by itself; until then the bar says so.
    let failed = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| board.connected.set(false));
    source.set_onopen(Some(opened.as_ref().unchecked_ref()));
    source.set_onerror(Some(failed.as_ref().unchecked_ref()));
    let held = SendWrapper::new((source, listeners, opened, failed));
    on_cleanup(move || {
        held.0.close();
        drop(held);
    });
}

pub fn encode(text: &str) -> String {
    js_sys::encode_uri_component(text).into()
}
