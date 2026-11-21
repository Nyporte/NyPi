//! Browsers that come through the entry (decision D94). The hub hands them to the board on
//! `run/board-public.sock`, where the local token is never taken: a request needs a browser
//! session the user admitted. A browser without one gets a page asking to be admitted, with a
//! six-digit code tied to its pending session, and the user is asked wherever join requests go
//! (with the same limits as machines'). Admitted, the page goes on by itself, and the session's
//! cookie (`HttpOnly`, `Secure`, `SameSite=Strict`) is kept; the register of peers holds only
//! its hash, as a `personal` browser. `nypi devices remove <name>` ends it at once: every
//! request, and every live stream every two seconds, looks it up again.

use bytes::Bytes;
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response, StatusCode};
use nypi_core::config::Paths;
use nypi_hub::pairing::{self, JoinAnswer, NewRequest, Peer, PeerKind, Peers, Trust};

use super::Board;
use super::web::{Body, json, respond};

const SESSION_COOKIE: &str = "nypi_session";
/// The browser's address, as the relay saw it.
const CLIENT_HEADER: &str = "x-nypi-client";

fn cookie(request: &Request<Incoming>, name: &str) -> Option<String> {
    request
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(&format!("{name}=")).map(str::to_string))
        .filter(|value| value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Whether the session whose key is `key` is an admitted browser.
pub fn admitted(paths: &Paths, key: &str) -> bool {
    Peers::open(paths).get(key).is_some_and(|peer| peer.kind == PeerKind::Browser)
}

/// The key of the request's admitted session, or None.
pub fn session(board: &Board, request: &Request<Incoming>) -> Option<String> {
    let key = pairing::session_key(&cookie(request, SESSION_COOKIE)?);
    admitted(&board.paths, &key).then_some(key)
}

fn header_text(request: &Request<Incoming>, name: impl header::AsHeaderName) -> String {
    request.headers().get(name).and_then(|value| value.to_str().ok()).unwrap_or_default().to_string()
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn set_cookie(response: &mut Response<Body>, token: Option<&str>) {
    let value = match token {
        Some(token) => format!("{SESSION_COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=31536000"),
        None => format!("{SESSION_COOKIE}=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0"),
    };
    if let Ok(value) = HeaderValue::from_str(&value) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

fn page(board: &Board, status: StatusCode, title: &str, body: &str, refresh: bool) -> Response<Body> {
    let refresh = if refresh { "<meta http-equiv=refresh content=3>" } else { "" };
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">{refresh}<title>NyPi board</title><body style=\"font:16px system-ui;margin:3em auto;max-width:32em;padding:0 1em\"><h1>{title}</h1>{body}"
    );
    respond(status, "text/html; charset=utf-8", Bytes::from(board.scrub(&html)))
}

fn waiting_page(board: &Board, code: &str) -> Response<Body> {
    let body = format!(
        "<p>This browser asks to see your NyPi board. On a machine you already trust (the hub, a personal node, \
         Telegram, or the board at home), admit the request that shows this code:</p><p style=\"font-size:2.2em;letter-spacing:.15em\"><b>{}</b></p><p>This page goes on by itself once you admit it. The request lasts {} minutes.</p>",
        nypi_node::identity::spaced(code),
        pairing::REQUEST_MINUTES
    );
    page(board, StatusCode::UNAUTHORIZED, "Admit this browser", &body, true)
}

/// What a browser without an admitted session gets: never the board's data.
pub async fn admission(board: &Board, request: &Request<Incoming>) -> Response<Body> {
    let path = request.uri().path().to_string();
    if path.starts_with("/api/") {
        return json(board, StatusCode::UNAUTHORIZED, &serde_json::json!({ "error": "this browser is not admitted" }));
    }
    let paths = &board.paths;
    if let Some(token) = cookie(request, SESSION_COOKIE) {
        let key = pairing::session_key(&token);
        match pairing::request_of(paths, &key) {
            Some(asked) if asked.answer == Some(JoinAnswer::Admit { trust: Trust::Personal }) => {
                let peers = Peers::open(paths);
                let user_agent = header_text(request, header::USER_AGENT);
                let browser = Peer {
                    name: pairing::browser_name(&peers, &user_agent),
                    trust: Trust::Personal,
                    kind: PeerKind::Browser,
                    system: user_agent.chars().take(120).collect(),
                    ..Default::default()
                };
                pairing::forget_request(paths, &asked.id);
                return match peers.add(&key, browser) {
                    Ok(_) => {
                        let mut response = respond(StatusCode::SEE_OTHER, "text/plain", "");
                        // Where the browser was going, its query too (decision D99).
                        let target = request.uri().path_and_query().map_or(path.clone(), |at| at.as_str().to_string());
                        let location = HeaderValue::from_str(&target).unwrap_or(HeaderValue::from_static("/"));
                        response.headers_mut().insert(header::LOCATION, location);
                        response
                    }
                    Err(error) => page(board, StatusCode::CONFLICT, "Not admitted", &format!("<p>{error}</p>"), false),
                };
            }
            Some(asked) if asked.answer.is_some() => {
                pairing::forget_request(paths, &asked.id);
                let mut response = page(
                    board,
                    StatusCode::FORBIDDEN,
                    "Not admitted",
                    "<p>The request was refused. Reload to ask again.</p>",
                    false,
                );
                set_cookie(&mut response, None);
                return response;
            }
            Some(asked) if pairing::pending(paths).iter().any(|waiting| waiting.id == asked.id) => {
                return waiting_page(board, &asked.code);
            }
            // Expired, or never made: a new request, under a new session.
            _ => {}
        }
    }
    let token = new_token();
    let user_agent = header_text(request, header::USER_AGENT);
    let from = header_text(request, CLIENT_HEADER);
    let new = NewRequest {
        kind: PeerKind::Browser,
        name: "browser".into(),
        system: user_agent.chars().take(120).collect(),
        from: if from.is_empty() { "unknown".into() } else { from },
        code: pairing::browser_code(&token),
        key: pairing::session_key(&token),
        instance_id: String::new(),
    };
    match pairing::open_request(paths, new) {
        Ok(asked) => {
            let mut response = waiting_page(board, &asked.code);
            set_cookie(&mut response, Some(&token));
            response
        }
        Err(refusal) => {
            if refusal.tell_user {
                let notice = nypi_hub::notify::Notice {
                    source: "devices",
                    level: nypi_core::inspector::AlertLevel::Warning,
                    text: "Requests to see the board are being refused until tomorrow: more came today than NyPi \
                           takes. If you did not ask for them, someone else is trying the entry.",
                    task: None,
                    shown_in: None,
                };
                nypi_hub::notify::post(paths, None, notice).await;
            }
            page(board, StatusCode::TOO_MANY_REQUESTS, "Not now", &format!("<p>{}.</p>", refusal.reason), false)
        }
    }
}
