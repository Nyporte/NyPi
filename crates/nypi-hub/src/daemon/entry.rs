//! The hub at the entry (decision D94): its control connection to the relay, kept open with
//! backoff, and the data connections the relay asks it for. A data connection of kind `link` is
//! a machine's link, handed to `remote.rs` exactly as one that came straight over TCP: the
//! pinned TLS runs inside it. One of kind `board` is a browser's request, handed to the board's
//! public socket, where every request needs a browser session.

use super::*;
use crate::relay::{self, Control};
use crate::ws::{self, Entry, Message};
use base64::Engine;
use nypi_node::identity::Identity;

/// The longest wait between two tries to register with the relay.
const LONGEST_WAIT: Duration = Duration::from_secs(60);
/// The relay pings every 25 seconds; silence for this long means it is gone.
const SILENCE: Duration = Duration::from_secs(80);

/// What the hub needs at the entry.
pub(crate) struct AtEntry {
    pub(crate) entry: Entry,
    pub(crate) remote: Arc<remote::Remote>,
    pub(crate) board: PathBuf,
}

/// How often a hub that follows its settings looks at the entry they name.
const ENTRY_LOOK: Duration = if cfg!(test) { Duration::from_millis(100) } else { Duration::from_secs(10) };

/// Keep the hub registered at its entry until dropped (decision D97): `entry` when it does not
/// `follow` the settings; else whatever `settings.json` names now, so an entry saved later with
/// `nypi hub up --entry` is taken without a restart. With no entry nothing is tried, and nothing
/// is said: the hub is on the local network only.
pub(crate) async fn follow(remote: Arc<remote::Remote>, board: PathBuf, entry: Option<String>, follow: bool) {
    let now = |paths: &Paths| match follow {
        true => nypi_core::config::Settings::load(paths, &paths.home).ok().and_then(|settings| settings.entry()),
        false => entry.clone(),
    };
    loop {
        let current = now(&remote.paths);
        let changed = async {
            loop {
                tokio::time::sleep(ENTRY_LOOK).await;
                if follow && now(&remote.paths) != current {
                    return;
                }
            }
        };
        match current.as_deref().and_then(Entry::parse) {
            Some(entry) => {
                let at = Arc::new(AtEntry { entry, remote: remote.clone(), board: board.clone() });
                tokio::select! {
                    _ = keep(at) => {}
                    _ = changed => log("the entry changed; the hub registers at the one the settings name now"),
                }
            }
            None => changed.await,
        }
    }
}

/// Keep the hub registered at the entry until dropped.
pub(crate) async fn keep(at: Arc<AtEntry>) {
    let mut wait = Duration::from_secs(1);
    let mut told = false;
    loop {
        let started = tokio::time::Instant::now();
        match session(&at).await {
            Ok(()) => log(&format!("the relay at {} closed the hub's connection", at.entry.url())),
            Err(error) if !told || started.elapsed() > LONGEST_WAIT => {
                log(&format!("not registered at {}: {error:#}", at.entry.url()));
                told = true;
            }
            Err(_) => {}
        }
        if started.elapsed() > LONGEST_WAIT {
            wait = Duration::from_secs(1);
        }
        tokio::time::sleep(crate::reach::jittered(wait)).await;
        wait = (wait * 2).min(LONGEST_WAIT);
    }
}

async fn session(at: &Arc<AtEntry>) -> anyhow::Result<()> {
    let (mut reader, mut writer) = ws::dial(&at.entry, relay::HUB_PATH, &[]).await?;
    let Some(Message::Text(first)) = reader.next().await? else { anyhow::bail!("the relay sent no challenge") };
    let Control::Challenge { nonce } = serde_json::from_str(&first)? else {
        anyhow::bail!("the relay sent no challenge")
    };
    let paths = &at.remote.paths;
    let key = Identity::load_or_create(&paths.hub_key())?;
    let instance = crate::instance::Instance::load(paths).unwrap_or_default();
    let epoch = instance.epoch.max(1);
    let signature = key.sign(&relay::registration(&nonce, epoch, &instance.instance_id));
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    let register = Control::Register {
        public_key: encode(&key.public_key()),
        epoch,
        instance_id: instance.instance_id.clone(),
        signature: encode(&signature),
    };
    writer.send(&Message::Text(serde_json::to_string(&register)?)).await?;
    let answer = tokio::time::timeout(Duration::from_secs(10), reader.next()).await??;
    match answer.map(|message| match message {
        Message::Text(text) => serde_json::from_str::<Control>(&text).ok(),
        _ => None,
    }) {
        Some(Some(Control::Registered { seen, max_kbps_per_link })) => {
            log(&format!("registered at {} at epoch {epoch} (the relay sees this hub at {seen})", at.entry.url()));
            if let Ok(seen) = seen.parse() {
                *at.remote.directions.seen.lock().unwrap() = Some(seen);
            }
            at.remote.relay_kbps.store(max_kbps_per_link, std::sync::atomic::Ordering::SeqCst);
        }
        Some(Some(Control::Refused { reason, epoch: Some(held) })) if held > epoch => {
            // Exactly one hub (decision D95): another holds the entry at a higher epoch.
            at.remote.handover.step_down(held);
            anyhow::bail!("the relay refused this hub: {reason}")
        }
        Some(Some(Control::Refused { reason, .. })) => anyhow::bail!("the relay refused this hub: {reason}"),
        _ => anyhow::bail!("the relay did not answer the registration"),
    }
    let (say, mut said) = tokio::sync::mpsc::unbounded_channel::<Message>();
    let writing = tokio::spawn(async move {
        let mut pings = tokio::time::interval(Duration::from_secs(25));
        loop {
            let message = tokio::select! {
                Some(message) = said.recv() => message,
                _ = pings.tick() => Message::Ping(Vec::new()),
            };
            if writer.send(&message).await.is_err() {
                break;
            }
        }
    });
    let ended = loop {
        match tokio::time::timeout(SILENCE, reader.next()).await {
            Err(_) => break Err(anyhow::anyhow!("the relay went silent")),
            Ok(Err(error)) => break Err(error.into()),
            Ok(Ok(None | Some(Message::Close))) => break Ok(()),
            Ok(Ok(Some(Message::Ping(bytes)))) => {
                let _ = say.send(Message::Pong(bytes));
            }
            Ok(Ok(Some(Message::Text(text)))) => {
                if let Ok(Control::Open { id, token, kind, from }) = serde_json::from_str(&text) {
                    let at = at.clone();
                    tokio::spawn(async move {
                        if let Err(error) = open(&at, &id, &token, &kind, from).await {
                            log(&format!("a relayed {kind} connection was not opened: {error}"));
                        }
                    });
                }
            }
            Ok(Ok(Some(_))) => {}
        }
    };
    writing.abort();
    ended
}

/// Open the data connection the relay asked for, and hand it on.
async fn open(at: &Arc<AtEntry>, id: &str, token: &str, kind: &str, from: String) -> std::io::Result<()> {
    let path = format!("{}{id}", relay::DATA_PATH);
    let (reader, writer) = ws::dial(&at.entry, &path, &[(relay::TOKEN_HEADER, token)]).await?;
    let mut stream = ws::into_stream(reader, writer);
    match kind {
        "link" => at.remote.take(stream, from, remote::Via::Relayed).await,
        "board" => {
            let mut board = UnixStream::connect(&at.board).await.map_err(|error| {
                std::io::Error::new(error.kind(), "the board is not running, so browsers cannot reach it")
            })?;
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut board).await;
        }
        _ => {}
    }
    Ok(())
}
