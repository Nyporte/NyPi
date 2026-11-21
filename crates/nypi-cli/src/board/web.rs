//! The board's HTTP side: the page (built from `crates/nypi-board-ui` and embedded at compile
//! time), a JSON API, a live stream of server-sent events, and the user's actions. On the local
//! network every request needs the token `nypi board` prints (kept as an HttpOnly cookie after
//! the first visit); through the entry, a browser session the user admitted (public.rs). Actions
//! also need the per-run CSRF value only the board's own page can read.

use std::convert::Infallible;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use nypi_board_proto::{Action, ActionResult, ChatView, LiveEvent};
use nypi_core::audit_log;
use nypi_core::config::Paths;
use nypi_core::ledger::{self, Ledger};
use serde::Serialize;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};

use super::Board;
use super::hub::{Change, Reach};
use super::planes;
use super::public;
use super::records::{self, ActivityFilter};

pub(super) type Body = BoxBody<Bytes, Infallible>;

const COOKIE: &str = "nypi_board";
const CSRF_HEADER: &str = "x-board-csrf";
/// A comment line now and then, so proxies and phones keep the stream open.
const KEEPALIVE: Duration = Duration::from_secs(15);
/// A task's story is rebuilt at most this often while it runs.
const STORY_EVERY: Duration = Duration::from_millis(900);
const MAX_ACTION: usize = 64 * 1024;
/// How often a live stream through the entry checks that its browser is still admitted.
const SESSION_LOOK: Duration = Duration::from_secs(2);

mod assets {
    include!(concat!(env!("OUT_DIR"), "/board_assets.rs"));
}

pub fn ui_built() -> bool {
    assets::ASSETS.iter().any(|(name, _)| *name == "index.html")
}

/// Serve the browsers that came through the entry, which the hub hands over on `listener`
/// (`run/board-public.sock`, decision D94). The local token is never taken there: a request
/// needs a browser session the user admitted, and without one gets only the page that asks.
pub async fn serve_public(listener: tokio::net::UnixListener, board: Arc<Board>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let board = board.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let board = board.clone();
                async move {
                    let response = match public::session(&board, &request) {
                        Some(key) => routes(board, request, Some(key)).await,
                        None => public::admission(&board, &request).await,
                    };
                    Ok::<_, Infallible>(response)
                }
            });
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
        });
    }
}

pub async fn serve(listener: TcpListener, board: Arc<Board>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let board = board.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| handle(board.clone(), request));
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
        });
    }
}

/// The token in `run/board.token`, made on first use or when `fresh`. Only the owner may read it.
pub fn token(paths: &Paths, fresh: bool) -> anyhow::Result<String> {
    let file = paths.home.join("run").join("board.token");
    if !fresh && let Ok(text) = std::fs::read_to_string(&file) {
        let text = text.trim().to_string();
        if text.len() >= 32 {
            return Ok(text);
        }
    }
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    let dir = file.parent().unwrap_or(&paths.home);
    std::fs::create_dir_all(dir)?;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let mut handle = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&file)?;
    std::io::Write::write_all(&mut handle, token.as_bytes())?;
    Ok(token)
}

/// Where another device can reach this machine: addresses on private networks (Wi-Fi,
/// Ethernet), and on a tailnet (100.64.0.0/10). The default route is not used: a proxy's tunnel
/// often holds it with an address no other device can reach (198.18.0.0/15).
pub fn reachable_addresses() -> Vec<(Ipv4Addr, &'static str)> {
    let mut found: Vec<Ipv4Addr> = Vec::new();
    // SAFETY: getifaddrs hands back a list this function reads and then frees; nothing else holds it.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return Vec::new();
        }
        let mut entry = list;
        while !entry.is_null() {
            let ifa = &*entry;
            if !ifa.ifa_addr.is_null() && i32::from((*ifa.ifa_addr).sa_family) == libc::AF_INET {
                let address = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                found.push(Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr)));
            }
            entry = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    let mut reachable: Vec<(Ipv4Addr, &'static str)> = found
        .into_iter()
        .filter_map(|ip| {
            let [a, b, ..] = ip.octets();
            if ip.is_private() {
                Some((ip, "your phone, on this network"))
            } else if a == 100 && (64..128).contains(&b) {
                Some((ip, "your tailnet"))
            } else {
                None
            }
        })
        .collect();
    reachable.dedup();
    reachable
}

/// A query string's pairs, decoded.
pub fn pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect()
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn param(query: &str, key: &str) -> Option<String> {
    pairs(query).into_iter().find(|(k, _)| k == key).map(|(_, v)| v).filter(|v| !v.is_empty())
}

enum Access {
    Granted,
    /// The token came in the link: set the cookie, and drop the token from the address bar.
    FromLink(String),
    Denied,
}

fn authorize(board: &Board, request: &Request<Incoming>) -> Access {
    let cookie = request
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().strip_prefix(&format!("{COOKIE}=")).map(str::to_string))
        .any(|value| same(&value, &board.token));
    if cookie {
        return Access::Granted;
    }
    let query = request.uri().query().unwrap_or_default();
    let mut rest = Vec::new();
    let mut linked = false;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        match pair.strip_prefix("token=") {
            Some(value) if same(&decode(value), &board.token) => linked = true,
            Some(_) => {}
            None => rest.push(pair),
        }
    }
    if linked { Access::FromLink(rest.join("&")) } else { Access::Denied }
}

/// Comparison that takes as long whatever the first difference, so timing reveals nothing.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(super) fn secure(response: &mut Response<Body>) {
    let headers = response.headers_mut();
    // The page's own script and module only; it shows the agent's record, which may hold
    // anything a web page or a tool printed.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; style-src-attr 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; manifest-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

pub(super) fn respond(status: StatusCode, content_type: &str, body: impl Into<Bytes>) -> Response<Body> {
    let mut response = Response::new(Full::new(body.into()).boxed());
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    secure(&mut response);
    response
}

/// A JSON answer, with every secret of the store taken out: what the board shows comes from
/// models and tools, which may have written one.
pub(super) fn json(board: &Board, status: StatusCode, value: &impl Serialize) -> Response<Body> {
    let text = serde_json::to_string(value).unwrap_or_default();
    respond(status, "application/json", board.scrub(&text))
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "json" | "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

/// A file of the page, compressed when the browser takes gzip and a compressed copy exists.
/// `asked` is the request's `If-None-Match`.
fn asset(name: &str, gzip: bool, asked: Option<&str>) -> Option<Response<Body>> {
    let find = |wanted: &str| assets::ASSETS.iter().find(|(n, _)| *n == wanted).map(|(_, bytes)| *bytes);
    let compressed = gzip.then(|| find(&format!("{name}.gz"))).flatten();
    let bytes = compressed.or_else(|| find(name))?;
    Some(file_response(name, bytes, compressed.is_some(), asked))
}

/// The tag of an embedded file's bytes, worked out once per file: what the browser sends back to
/// learn it still has the file.
fn etag_of(bytes: &'static [u8]) -> String {
    static TAGS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<usize, String>>> =
        std::sync::OnceLock::new();
    let tags = TAGS.get_or_init(Default::default);
    let key = bytes.as_ptr() as usize ^ bytes.len();
    tags.lock()
        .unwrap()
        .entry(key)
        .or_insert_with(|| {
            let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
            let hex: String = digest.as_ref()[..16].iter().map(|b| format!("{b:02x}")).collect();
            format!("\"{hex}\"")
        })
        .clone()
}

/// Whether `If-None-Match` names `tag` (a list, weak tags, or `*`).
fn not_modified(asked: Option<&str>, tag: &str) -> bool {
    asked.is_some_and(|asked| {
        asked.split(',').map(|one| one.trim().trim_start_matches("W/")).any(|one| one == tag || one == "*")
    })
}

/// A file of the page (decision D99): tagged, and kept by the browser, which asks again on each
/// load and gets `304 Not Modified` while it has the file. Data and locked pages stay
/// `no-store`.
pub(super) fn file_response(name: &str, bytes: &'static [u8], gzip: bool, asked: Option<&str>) -> Response<Body> {
    let tag = etag_of(bytes);
    let mut response = if not_modified(asked, &tag) {
        let mut response = respond(StatusCode::NOT_MODIFIED, content_type(name), Bytes::new());
        response.headers_mut().remove(header::CONTENT_TYPE);
        response
    } else {
        respond(StatusCode::OK, content_type(name), Bytes::from_static(bytes))
    };
    let headers = response.headers_mut();
    if gzip && response_is_full(headers) {
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    if let Ok(tag) = HeaderValue::from_str(&tag) {
        headers.insert(header::ETAG, tag);
    }
    response
}

/// A response with a body (not a 304, which has no type and no encoding).
fn response_is_full(headers: &hyper::HeaderMap) -> bool {
    headers.contains_key(header::CONTENT_TYPE)
}

const LOCKED: &str = "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>NyPi board</title><body style=\"font:16px system-ui;margin:3em auto;max-width:32em;padding:0 1em\"><h1>Locked</h1><p>Open the link <code>nypi board</code> printed; it carries the token. On a phone, scan or paste the LAN link.</p>";

const NOT_BUILT: &str = "<!doctype html><meta charset=utf-8><title>NyPi board</title><body style=\"font:16px system-ui;margin:3em auto;max-width:32em;padding:0 1em\"><h1>The page is not built</h1><p>Run <code>scripts/build-board.sh</code>, then rebuild <code>nypi</code>. The API under <code>/api/</code> works meanwhile.</p>";

async fn handle(board: Arc<Board>, request: Request<Incoming>) -> Result<Response<Body>, Infallible> {
    let path = request.uri().path().to_string();
    // The page's files hold no data; a locked visitor still needs none of them.
    match authorize(&board, &request) {
        Access::Granted => {}
        Access::FromLink(rest) => {
            let location = if rest.is_empty() { path.clone() } else { format!("{path}?{rest}") };
            let mut response = respond(StatusCode::SEE_OTHER, "text/plain", "");
            let cookie = format!("{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=31536000", board.token);
            let headers = response.headers_mut();
            headers.insert(header::LOCATION, HeaderValue::from_str(&location).unwrap_or(HeaderValue::from_static("/")));
            headers.insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap_or(HeaderValue::from_static("")));
            return Ok(response);
        }
        Access::Denied if path.starts_with("/api/") => {
            return Ok(json(&board, StatusCode::UNAUTHORIZED, &serde_json::json!({ "error": "locked" })));
        }
        Access::Denied => return Ok(respond(StatusCode::UNAUTHORIZED, "text/html; charset=utf-8", LOCKED)),
    }
    Ok(routes(board, request, None).await)
}

/// What the board answers a request it let in; `session` is the browser session's key when the
/// request came through the entry (decision D94), so a live stream ends when it is removed.
async fn routes(board: Arc<Board>, request: Request<Incoming>, session: Option<String>) -> Response<Body> {
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or_default().to_string();
    let gzip = request
        .headers()
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|e| e.trim().starts_with("gzip")));
    let asked = request.headers().get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).map(str::to_string);
    match (request.method(), path.as_str()) {
        (&Method::GET, "/api/hello") => json(&board, StatusCode::OK, &serde_json::json!({ "csrf": board.csrf })),
        (&Method::GET, "/api/now") => json(&board, StatusCode::OK, &*board.now.borrow().clone()),
        (&Method::GET, "/api/activity") => activity(&board, &query).await,
        (&Method::GET, "/api/topology" | "/api/system") => topology(&board).await,
        (&Method::GET, "/api/chat") => {
            let limit = param(&query, "limit").and_then(|n| n.parse().ok()).unwrap_or(planes::CHAT_TURNS);
            let mut view = read_planes(&board, move |paths, settings| planes::chat_view(paths, settings, limit)).await;
            view.live = board.hub.plane("chat");
            json(&board, StatusCode::OK, &view)
        }
        (&Method::GET, "/api/principal") => {
            let mut view =
                read_planes(&board, |paths, settings| planes::principal_view(paths, settings, chrono::Utc::now()))
                    .await;
            view.card.live = board.hub.plane("principal");
            json(&board, StatusCode::OK, &view)
        }
        (&Method::GET, "/api/vault") => match param(&query, "path") {
            Some(path) => {
                let secrets = board.secrets();
                let note = read_planes(&board, move |paths, _| planes::vault_note(paths, &path, &secrets)).await;
                match note {
                    Some(note) => json(&board, StatusCode::OK, &note),
                    None => json(&board, StatusCode::NOT_FOUND, &serde_json::json!({ "error": "no such note" })),
                }
            }
            None => json(
                &board,
                StatusCode::OK,
                &read_planes(&board, |paths, _| planes::vault_view(paths, &Ledger::new(paths.ledger()).load())).await,
            ),
        },
        (&Method::GET, _) if path.starts_with("/api/activation/") => {
            let id = decode(&path["/api/activation/".len()..]);
            match read_planes(&board, move |paths, _| planes::activation_detail(paths, &id)).await {
                Some(detail) => json(&board, StatusCode::OK, &detail),
                None => json(&board, StatusCode::NOT_FOUND, &serde_json::json!({ "error": "no such activation" })),
            }
        }
        (&Method::GET, "/api/live") => live(&board, &query, session),
        (&Method::POST, "/api/action") => act(&board, request).await,
        (&Method::GET, _) if path.starts_with("/api/task/") => {
            let id = decode(&path["/api/task/".len()..]);
            match story(&board, id).await {
                Some(detail) => json(&board, StatusCode::OK, &detail),
                None => json(&board, StatusCode::NOT_FOUND, &serde_json::json!({ "error": "no such task" })),
            }
        }
        (_, _) if path.starts_with("/api/") => {
            json(&board, StatusCode::NOT_FOUND, &serde_json::json!({ "error": "not found" }))
        }
        (&Method::GET, _) => match path.strip_prefix("/assets/").and_then(|name| asset(name, gzip, asked.as_deref())) {
            Some(response) => response,
            // Any other address is a screen of the page, which routes it.
            None => asset("index.html", false, asked.as_deref())
                .unwrap_or_else(|| respond(StatusCode::OK, "text/html; charset=utf-8", NOT_BUILT)),
        },
        _ => respond(StatusCode::METHOD_NOT_ALLOWED, "text/plain", ""),
    }
}

async fn activity(board: &Arc<Board>, query: &str) -> Response<Body> {
    let hours: i64 = param(query, "hours").and_then(|h| h.parse().ok()).unwrap_or(24).clamp(1, 24 * 90);
    let filter = ActivityFilter::from_query(query);
    let paths = board.paths.clone();
    let rows = tokio::task::spawn_blocking(move || {
        let oldest = (chrono::Utc::now() - chrono::Duration::hours(hours)).to_rfc3339();
        let tasks = Ledger::new(paths.ledger()).load();
        let all = audit_log::load(&paths.audit(), (hours / 24 + 2) as usize);
        let records: Vec<&Value> = records::since(&all, &oldest).collect();
        records::activity(&tasks, &records, &oldest, &filter, ledger::process_alive, 400)
    })
    .await
    .unwrap_or_default();
    json(board, StatusCode::OK, &rows)
}

/// Read what the planes keep, off the async threads: it is files on disk.
async fn read_planes<T: Default + Send + 'static>(
    board: &Arc<Board>,
    read: impl FnOnce(&Paths, &nypi_core::config::Settings) -> T + Send + 'static,
) -> T {
    let paths = board.paths.clone();
    tokio::task::spawn_blocking(move || {
        let settings = nypi_core::config::Settings::load(&paths, &paths.home).unwrap_or_default();
        read(&paths, &settings)
    })
    .await
    .unwrap_or_default()
}

/// The program as a map, with every module's state as of now.
async fn topology(board: &Arc<Board>) -> Response<Body> {
    let shared = board.clone();
    let map = tokio::task::spawn_blocking(move || {
        super::topology::build(&super::topology::gather(&shared, chrono::Utc::now()))
    })
    .await
    .unwrap_or_default();
    json(board, StatusCode::OK, &map)
}

/// A task's story, with the connected process that runs it now.
async fn story(board: &Arc<Board>, id: String) -> Option<nypi_board_proto::TaskDetail> {
    let paths = board.paths.clone();
    let live = board.hub.running(&id);
    let secrets = board.secrets();
    let mut detail = tokio::task::spawn_blocking(move || {
        let tasks = Ledger::new(paths.ledger()).load();
        let created = tasks.get(&ledger::TaskId(id.clone()))?.created.clone();
        let days = chrono::DateTime::parse_from_rfc3339(&created)
            .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_days().max(0) as usize + 2)
            .unwrap_or(2);
        let records = audit_log::load(&paths.audit(), days);
        let mut detail = records::task_detail(&tasks, &id, &records, ledger::process_alive)?;
        if let Some(task) = tasks.get(&ledger::TaskId(id.clone())) {
            planes::task_extras(&paths, task, &mut detail, &secrets);
        }
        Some(detail)
    })
    .await
    .ok()??;
    detail.live = live;
    Some(detail)
}

async fn act(board: &Arc<Board>, request: Request<Incoming>) -> Response<Body> {
    let csrf = request.headers().get(CSRF_HEADER).and_then(|v| v.to_str().ok()).unwrap_or_default();
    if !same(csrf, &board.csrf) {
        return json(
            board,
            StatusCode::FORBIDDEN,
            &ActionResult { ok: false, message: "Reload the board and try again.".into() },
        );
    }
    let Ok(body) = Limited::new(request.into_body(), MAX_ACTION).collect().await else {
        return json(board, StatusCode::BAD_REQUEST, &ActionResult { ok: false, message: "Too long.".into() });
    };
    let Ok(action) = serde_json::from_slice::<Action>(&body.to_bytes()) else {
        return json(board, StatusCode::BAD_REQUEST, &ActionResult { ok: false, message: "Not an action.".into() });
    };
    let result = |sent: Reach, done: &str, idle: &str| match sent {
        Reach::Delivered => ActionResult { ok: true, message: done.into() },
        Reach::Idle => ActionResult { ok: false, message: idle.into() },
        Reach::Gone => {
            ActionResult { ok: false, message: "That is no longer waiting: the process moved on or ended.".into() }
        }
    };
    let outcome = match action {
        Action::Approve { id } => result(board.hub.answer(&id, true, None), "Approved.", ""),
        Action::Deny { id, reason } => {
            let reason = reason.map(|r| r.trim().to_string()).filter(|r| !r.is_empty());
            result(board.hub.answer(&id, false, reason), "Denied.", "")
        }
        Action::Stop { process } => result(board.hub.stop(&process), "Stopping.", "Nothing is running there."),
        Action::Message { process, text } if text.trim().is_empty() => {
            let _ = process;
            ActionResult { ok: false, message: "Write a message first.".into() }
        }
        Action::Message { process, text } => result(
            board.hub.message(&process, text),
            "Sent. Lucia reads it before her next step.",
            "Nothing is running there; reply where the conversation is.",
        ),
        Action::Acknowledge { key } => {
            board.acknowledge(key);
            ActionResult { ok: true, message: "Cleared.".into() }
        }
        Action::Say { text } if text.trim().is_empty() => {
            ActionResult { ok: false, message: "Write a message first.".into() }
        }
        Action::Say { text } => say(&board.paths, &text).await,
        Action::Admit { code, exec_only } => {
            let trust = if exec_only { nypi_hub::pairing::Trust::ExecOnly } else { nypi_hub::pairing::Trust::Personal };
            join_answer(board, &code, nypi_hub::pairing::JoinAnswer::Admit { trust })
        }
        Action::Refuse { code } => join_answer(board, &code, nypi_hub::pairing::JoinAnswer::Refuse),
    };
    json(board, StatusCode::OK, &outcome)
}

/// Say `text` to Lucia as the owner, in the daemon's conversation (decision D86): the board is
/// one more interface, so chat answers it as it answers any.
async fn say(paths: &Paths, text: &str) -> ActionResult {
    let sent = async {
        let Some(mut link) = nypi_hub::link::Link::hub(paths, None, false).await? else {
            return Ok(false);
        };
        link.send(nypi_hub::link::message(&nypi_hub::link::new_id(), text)).await?;
        let _ = link.leave().await;
        anyhow::Ok(true)
    }
    .await;
    match sent {
        Ok(true) => ActionResult { ok: true, message: "Sent. Chat answers on the Chat screen.".into() },
        Ok(false) => ActionResult { ok: false, message: "The hub is not running: start it with nypi hub up.".into() },
        Err(error) => ActionResult { ok: false, message: format!("Not sent: {error:#}") },
    }
}

/// A file's length and time of change, to see that it grew without reading it.
fn fingerprint(path: &std::path::Path) -> Option<(u64, std::time::SystemTime)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// Undoes a viewer's watch when its stream ends.
struct Watching {
    board: Arc<Board>,
    process: Option<String>,
}

impl Drop for Watching {
    fn drop(&mut self) {
        self.board.hub.watch(self.process.as_deref(), false);
    }
}

fn event(board: &Board, event: &LiveEvent) -> Bytes {
    let name = match event {
        LiveEvent::Now { .. } => "now",
        LiveEvent::Task { .. } => "task",
        LiveEvent::Delta { .. } => "delta",
        LiveEvent::Chat { .. } => "chat",
        LiveEvent::Activation { .. } => "activation",
        LiveEvent::Plane { .. } => "plane",
    };
    let data = board.scrub(&serde_json::to_string(event).unwrap_or_default());
    Bytes::from(format!("event: {name}\ndata: {data}\n\n"))
}

/// The user's answer to a join request, given on the board (decision D93).
fn join_answer(board: &Board, code: &str, answer: nypi_hub::pairing::JoinAnswer) -> ActionResult {
    let outcome = match nypi_hub::pairing::answer(&board.paths, code, answer) {
        Ok(request) => match answer {
            nypi_hub::pairing::JoinAnswer::Admit { trust } => {
                ActionResult { ok: true, message: format!("Admitted {} as {}.", request.name, trust.label()) }
            }
            nypi_hub::pairing::JoinAnswer::Refuse => {
                ActionResult { ok: true, message: format!("Refused {}.", request.name) }
            }
        },
        Err(why) => ActionResult { ok: false, message: why },
    };
    board.hub.nudge();
    outcome
}

/// The live stream: the home screen whenever it changes; with `watch=<process>` (or `*`, every
/// process) the reply text as it is written; with `task=<id>` that task's story as it grows;
/// with `chat=1` the conversation's turns when one is added; with `activation=<id>` that
/// activation's story as it grows, with the Principal's stack (decision D86).
fn live(board: &Arc<Board>, query: &str, session: Option<String>) -> Response<Body> {
    let watch = param(query, "watch");
    let task = param(query, "task");
    let chat = param(query, "chat").is_some();
    let activation = param(query, "activation");
    let (tx, rx) = mpsc::channel::<Bytes>(256);
    let board = board.clone();
    tokio::spawn(async move {
        let everyone = watch.as_deref() == Some("*");
        let process = watch.filter(|w| w != "*");
        let _watching = (process.is_some() || everyone).then(|| {
            board.hub.watch(process.as_deref(), true);
            Watching { board: board.clone(), process: process.clone() }
        });
        let mut now = board.now.subscribe();
        let mut changes = board.hub.subscribe();
        let current = LiveEvent::Now { now: (**now.borrow_and_update()).clone() };
        if tx.send(event(&board, &current)).await.is_err() {
            return;
        }
        let mut story_due: Option<tokio::time::Instant> = None;
        let mut keepalive = tokio::time::interval(KEEPALIVE);
        keepalive.tick().await;
        // The planes' logs are files another process writes: looked at now and then.
        let mut planes_look = tokio::time::interval(STORY_EVERY);
        let watched: Vec<std::path::PathBuf> = {
            let paths = &board.paths;
            let mut files = Vec::new();
            if chat {
                files.push(paths.chat_turns());
            }
            if let Some(id) = &activation {
                files.extend([paths.activation_log(id), paths.principal_status()]);
            }
            files
        };
        let mut seen: Vec<Option<(u64, std::time::SystemTime)>> = vec![None; watched.len()];
        let mut session_look = tokio::time::interval(SESSION_LOOK);
        loop {
            let story_wait = async {
                match story_due {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            let message = tokio::select! {
                changed = now.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let now = (**now.borrow_and_update()).clone();
                    Some(event(&board, &LiveEvent::Now { now }))
                }
                change = changes.recv() => {
                    match change {
                        Ok(Change::Delta { process: from, text }) if process.as_deref() == Some(from.as_str()) => {
                            Some(event(&board, &LiveEvent::Delta { process: from, text }))
                        }
                        Ok(Change::Story { process: from }) if task.is_some() && story_due.is_none() => {
                            let runs = task.as_deref().and_then(|t| board.hub.running(t));
                            if runs.as_deref() == Some(from.as_str()) {
                                story_due = Some(tokio::time::Instant::now() + STORY_EVERY);
                            }
                            None
                        }
                        Ok(Change::Stored) if task.is_some() && story_due.is_none() => {
                            story_due = Some(tokio::time::Instant::now() + STORY_EVERY);
                            None
                        }
                        // What a plane is doing goes to every viewer: the home screen's cards,
                        // the chat page, and the activation being written (decision D88).
                        Ok(Change::Plane(live)) => Some(event(&board, &LiveEvent::Plane { live })),
                        Err(broadcast::error::RecvError::Closed) => break,
                        _ => None,
                    }
                }
                _ = story_wait => {
                    story_due = None;
                    match task.clone() {
                        Some(id) => story(&board, id).await.map(|task| event(&board, &LiveEvent::Task { task: Box::new(task) })),
                        None => None,
                    }
                }
                _ = planes_look.tick(), if !watched.is_empty() => {
                    let now: Vec<_> = watched.iter().map(|file| fingerprint(file)).collect();
                    if now == seen {
                        None
                    } else {
                        seen = now;
                        planes_event(&board, chat, activation.clone()).await
                    }
                }
                _ = keepalive.tick() => Some(Bytes::from_static(b": still here\n\n")),
                // A browser session removed with `nypi devices remove` sees no more.
                _ = session_look.tick(), if session.is_some() => {
                    if !session.as_deref().is_some_and(|key| public::admitted(&board.paths, key)) {
                        break;
                    }
                    None
                }
            };
            // The page closed: stop.
            if let Some(message) = message
                && tx.send(message).await.is_err()
            {
                break;
            }
        }
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|bytes| (Ok::<_, Infallible>(Frame::data(bytes)), rx))
    });
    let mut response = Response::new(BodyExt::boxed(StreamBody::new(stream)));
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    secure(&mut response);
    response
}

/// What changed in the planes a viewer follows, as stream events.
async fn planes_event(board: &Arc<Board>, chat: bool, activation: Option<String>) -> Option<Bytes> {
    let live = board.hub.plane("principal");
    let chat_live = board.hub.plane("chat");
    let events = read_planes(board, move |paths, settings| {
        let mut events = Vec::new();
        if chat {
            let view = ChatView { live: chat_live, ..planes::chat_view(paths, settings, planes::CHAT_TURNS) };
            events.push(LiveEvent::Chat { chat: Box::new(view) });
        }
        if let Some(detail) = activation.and_then(|id| planes::activation_detail(paths, &id)) {
            let principal = planes::principal_view(paths, settings, chrono::Utc::now()).card;
            let principal = nypi_board_proto::PrincipalCard { live: live.clone(), ..principal };
            events.push(LiveEvent::Activation { activation: Box::new(detail), principal: Box::new(principal) });
        }
        events
    })
    .await;
    let bytes: Vec<Bytes> = events.iter().map(|live| event(board, live)).collect();
    (!bytes.is_empty()).then(|| Bytes::from(bytes.concat()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn reachable_addresses_skip_loopback_and_proxy_tunnels() {
        let addresses = super::reachable_addresses();
        println!("{addresses:?}");
        for (address, _) in addresses {
            let [a, b, ..] = address.octets();
            let tunnel = a == 198 && (18..20).contains(&b);
            assert!(!address.is_loopback() && !tunnel, "{address}");
        }
    }
}
