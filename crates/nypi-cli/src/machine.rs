//! The commands about this machine (decision D93): `nypi hub up|down`, `nypi node up|down|leave`,
//! `nypi status`, and `nypi devices` with its answers to join requests and its invites. A machine
//! is the hub or a node of one, as `instance.json` says; `up` writes that file and installs the
//! supervisor, which keeps the right services running.

use std::io::BufRead;

use nypi_core::config::{Paths, Settings};
use nypi_hub::instance::{self, Instance, Mode};
use nypi_hub::pairing::{self, JoinAnswer, Peers, Purpose, Trust};
use nypi_node::identity::Identity;

use crate::supervisor;

fn settings(paths: &Paths) -> Settings {
    Settings::load(paths, &paths.home).unwrap_or_default()
}

/// Keep `--entry` in the settings, so the supervisor's services use it too, and every later
/// version of the program (decision D97). The entry now: None for the local network only.
fn keep_entry(paths: &Paths, entry: Option<&str>) -> anyhow::Result<Option<String>> {
    if let Some(entry) = entry.map(str::trim).filter(|entry| !entry.is_empty()) {
        anyhow::ensure!(
            nypi_hub::ws::Entry::parse(entry).is_some(),
            "the entry reads host:port, https://host, or http://host:port"
        );
        Settings::save_global(paths, &["entry"], serde_json::json!(entry))?;
        return Ok(Some(entry.to_string()));
    }
    Ok(settings(paths).entry())
}

/// Refuse an entry where a hub with another key is registered already.
async fn entry_is_free(paths: &Paths, entry: &str) -> anyhow::Result<()> {
    let hub = Identity::load_or_create(&paths.hub_key())?;
    if let Some(other) = hub_at(paths, entry).await
        && other != hub.fingerprint()
    {
        anyhow::bail!(
            "the entry {entry} already has a hub, with another key ({}). Use nypi hub here on a machine of that hub \
             to move it, or another entry (--entry)",
            short(&other)
        );
    }
    Ok(())
}

/// `nypi hub up`.
pub async fn hub_up(paths: &Paths, entry: Option<&str>, name: Option<&str>) -> anyhow::Result<i32> {
    match Instance::load(paths) {
        Some(instance) if instance.mode == Mode::Node => anyhow::bail!(
            "this machine is a node of a hub (key {}). nypi hub here moves the hub to it; nypi node leave unpairs it \
             first",
            short(&instance.hub_fingerprint)
        ),
        Some(mut instance) => {
            // A new entry for a hub that runs: saved, and its daemon registers there within seconds.
            if let Some(new) = entry.map(str::trim).filter(|entry| !entry.is_empty()) {
                entry_is_free(paths, new).await?;
                keep_entry(paths, Some(new))?;
                nypi_hub::outln!("The entry is now {new}; the hub registers there within seconds.");
            }
            if instance.down {
                instance.down = false;
                instance.save(paths)?;
                nypi_hub::outln!("The hub is started again.");
            } else {
                nypi_hub::outln!("This machine is the hub already.");
            }
            supervisor::install(paths)?;
            describe_hub(paths, &instance);
        }
        None => {
            let instance = become_hub(paths, entry, name).await?;
            supervisor::install(paths)?;
            nypi_hub::outln!("This machine is now the hub ({}), at epoch {}.", instance.name, instance.epoch);
            describe_hub(paths, &instance);
        }
    }
    watch_requests(paths).await;
    Ok(0)
}

/// Make this machine the hub: its key and `instance.json`, at epoch 1, and the entry when one is
/// given (decision D97: none is needed; the hub is then on the local network only).
async fn become_hub(paths: &Paths, entry: Option<&str>, name: Option<&str>) -> anyhow::Result<Instance> {
    if let Some(entry) = entry.map(str::trim).filter(|entry| !entry.is_empty()) {
        anyhow::ensure!(
            nypi_hub::ws::Entry::parse(entry).is_some(),
            "the entry reads host:port, https://host, or http://host:port"
        );
        entry_is_free(paths, entry).await?;
    }
    keep_entry(paths, entry)?;
    let instance = Instance::new_hub(paths, &name.map(instance::plain_name).unwrap_or_else(instance::machine_name))?;
    instance.save(paths)?;
    Ok(instance)
}

fn describe_hub(paths: &Paths, instance: &Instance) {
    nypi_hub::outln!("  hub key  {}", instance.hub_fingerprint);
    match settings(paths).entry() {
        Some(entry) => {
            nypi_hub::outln!("  entry    {entry}");
            nypi_hub::outln!("On the machine behind the entry (a VPS with Cloudflare Tunnel or a reverse proxy):");
            nypi_hub::outln!("  nypi relay up --hub-key {}", instance.hub_fingerprint);
            nypi_hub::outln!("  nypi node up --entry {entry}        # admit it as exec-only");
            nypi_hub::outln!("and point the entry's name at http://127.0.0.1:7682 there, with WebSocket on.");
            nypi_hub::outln!(
                "Other machines join with: nypi node up --entry {entry}   (then answer each request here or anywhere \
                 you are)"
            );
        }
        None => {
            nypi_hub::outln!("  entry    none (local network only)");
            nypi_hub::outln!(
                "Machines on this network join with: nypi node up   (then answer each request here or anywhere you \
                 are)"
            );
            nypi_hub::outln!("To be reached from anywhere, add an entry later: nypi hub up --entry <your-domain:443>");
        }
    }
}

/// The fingerprint of the hub at `entry`, when one answers within a few seconds.
async fn hub_at(paths: &Paths, entry: &str) -> Option<String> {
    let way = pairing::Way::Relayed(entry.to_string());
    let probed = tokio::time::timeout(std::time::Duration::from_secs(5), pairing::probe(paths, &way, None, 0)).await;
    probed.ok()?.ok().map(|(fingerprint, _)| fingerprint)
}

fn short(fingerprint: &str) -> String {
    fingerprint.chars().take(16).collect()
}

/// Show join requests as they come, and take `/admit` and `/refuse` typed here, until Ctrl-C or
/// the end of input. The hub keeps running either way: the supervisor holds it.
async fn watch_requests(paths: &Paths) {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return;
    }
    nypi_hub::outln!("Waiting for join requests (Ctrl-C leaves; the hub keeps running).");
    let (lines, mut typed) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let mut shown = std::collections::BTreeSet::new();
    let mut look = tokio::time::interval(std::time::Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return,
            line = typed.recv() => match line {
                Some(line) => {
                    let line = line.trim();
                    match nypi_hub::commands::join_command(paths, line) {
                        Some(said) => nypi_hub::outln!("{said}"),
                        None if line.is_empty() => {}
                        None => nypi_hub::outln!("Type /admit <code>, /admit <code> exec-only, or /refuse <code>."),
                    }
                }
                None => return,
            },
            _ = look.tick() => {
                for request in pairing::pending(paths) {
                    if shown.insert(request.id.clone()) {
                        nypi_hub::outln!("{}", request.question());
                    }
                }
            }
        }
    }
}

/// `nypi node up`.
pub async fn node_up(
    paths: &Paths,
    name: Option<&str>,
    code: Option<&str>,
    entry: Option<&str>,
) -> anyhow::Result<i32> {
    if let Some(mut instance) = Instance::load(paths) {
        anyhow::ensure!(!instance.is_hub(), "this machine is the hub; nypi hub down stops it");
        if instance.down {
            instance.down = false;
            instance.save(paths)?;
            nypi_hub::outln!("The node is started again.");
        } else {
            nypi_hub::outln!("This machine is a node of its hub already ({}).", instance.trust.label());
        }
        supervisor::install(paths)?;
        return Ok(0);
    }
    let entry = keep_entry(paths, entry)?;
    let name = name.map(instance::plain_name).unwrap_or_else(instance::machine_name);
    let id = instance::new_id();
    let ways = nypi_hub::reach::ways_to_join(paths, entry.as_deref(), &nypi_hub::reach::Reach::local()).await?;
    let first = ways.first().map(pairing::Way::label).unwrap_or_default();
    nypi_hub::outln!("Asking the hub at {first} to admit this machine as {name}…");
    let joined = pairing::join(paths, &ways, &name, code, &id, |code| {
        nypi_hub::outln!(
            "Confirmation code: {}\nOn a machine you already trust (the hub, a personal node, Telegram, or the board), \
             admit the request that shows this same code. Waiting up to {} minutes; Ctrl-C gives up.",
            nypi_node::identity::spaced(code),
            pairing::REQUEST_MINUTES
        );
    })
    .await?;
    let instance = Instance {
        mode: Mode::Node,
        name: joined.name.clone(),
        instance_id: id,
        epoch: joined.epoch.max(1),
        hub_fingerprint: joined.hub_fingerprint.clone(),
        trust: joined.trust,
        direct: joined.direct.clone(),
        down: false,
    };
    instance.save(paths)?;
    supervisor::install(paths)?;
    nypi_hub::outln!(
        "Admitted as {} ({}). The hub's key: {}",
        instance.name,
        instance.trust.label(),
        instance.hub_fingerprint
    );
    match instance.trust {
        Trust::Personal => {
            nypi_hub::outln!("This machine now carries out the hub's calls, and nypi here talks to the hub.")
        }
        Trust::ExecOnly => nypi_hub::outln!("This machine now carries out the hub's calls; it cannot talk to the hub."),
    }
    Ok(0)
}

/// This machine's own direct addresses, as it will offer them once it is the hub.
fn own_direct(settings: &Settings) -> Vec<String> {
    let port = settings
        .hub
        .listen
        .iter()
        .find_map(|address| address.parse::<std::net::SocketAddr>().ok())
        .map_or(nypi_core::config::HUB_PORT, |address| address.port());
    let directions = nypi_hub::direct::Directions::default();
    directions.refresh(Some(port), settings.hub.public_address.as_deref());
    directions.now()
}

/// `nypi hub here [--force]` (decision D95).
pub async fn hub_here(paths: &Paths, force: bool, yes: bool) -> anyhow::Result<i32> {
    let settings = settings(paths);
    let reach = nypi_hub::reach::Reach::local();
    let instance = if force {
        if let Some(why) = nypi_hub::standby::force_refused(paths, &settings, &reach).await {
            anyhow::bail!("{why}");
        }
        let age = nypi_hub::standby::copy_age(paths)
            .map_or("of unknown age".to_string(), |age| format!("{} minutes old", age.num_minutes()));
        nypi_hub::outln!("The hub does not answer. This machine's standby copy of its state is {age}:");
        nypi_hub::outln!("anything the hub did after it is lost here, and the old hub steps down if it comes back.");
        if !yes {
            use std::io::Write;
            nypi_hub::out!("Take over as the hub? [y/N] ");
            std::io::stdout().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !answer.trim().eq_ignore_ascii_case("y") {
                nypi_hub::outln!("Nothing changed.");
                return Ok(1);
            }
        }
        nypi_hub::standby::force(paths)?
    } else {
        nypi_hub::outln!(
            "Moving the hub here: it stops its work, sends what is new since this machine's copy, and steps down…"
        );
        nypi_hub::standby::take_over(paths, &settings, &reach, own_direct(&settings)).await?
    };
    supervisor::install(paths)?;
    nypi_hub::outln!("This machine is the hub now ({}), at epoch {}.", instance.name, instance.epoch);
    nypi_hub::outln!("Machines link to it without joining again; the old hub is a node of it.");
    Ok(0)
}

/// `nypi update` on this machine (decision D98): check, or put a release in place and start the
/// supervisor again on it.
pub async fn update_here(
    paths: &Paths,
    check: bool,
    version: Option<String>,
    yes: bool,
    allow_unsigned: bool,
) -> anyhow::Result<i32> {
    use std::io::IsTerminal;
    let updater = crate::update::Updater::new(paths, &settings(paths))?;
    if check || (!yes && version.is_none() && std::io::stdin().is_terminal()) {
        let checked = updater.check(allow_unsigned).await?;
        let latest = &checked.latest.version;
        if !checked.newer {
            nypi_hub::outln!("NyPi {} is up to date (the latest release is {latest}).", checked.current);
            return Ok(0);
        }
        nypi_hub::outln!("NyPi {latest} is available (this is {}): {}", checked.current, checked.page);
        if check {
            return Ok(0);
        }
        nypi_hub::out!("Update this machine to {latest}? [y/N] ");
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            nypi_hub::outln!("Nothing changed.");
            return Ok(1);
        }
    }
    let options = crate::update::Options { version, allow_unsigned };
    match updater.apply(&options, false).await? {
        crate::update::Applied::UpToDate { version } => nypi_hub::outln!("NyPi {version} is the latest release."),
        crate::update::Applied::Swapped { from, to } => {
            nypi_hub::outln!(
                "NyPi {to} is in place (was {from}). The supervisor checks that it stays up, and puts {from} back if \
                 not; nypi update rollback does it by hand."
            );
            supervisor::restart()?;
            if let Some(note) = crate::update::same_binary_note(paths) {
                nypi_hub::outln!("{note}");
            }
        }
    }
    Ok(0)
}

/// `nypi hub down` and `nypi node down`: stop the service, keep the data.
pub fn down(paths: &Paths, which: &str) -> anyhow::Result<i32> {
    let Some(mut instance) = Instance::load(paths) else { anyhow::bail!("this machine is neither a hub nor a node") };
    anyhow::ensure!(instance.mode.label() == which, "this machine is a {}, not a {which}", instance.mode.label());
    instance.down = true;
    instance.save(paths)?;
    if !supervisor::services(Some(&instance), settings(paths).relay.enabled).any() {
        supervisor::uninstall(paths)?;
    }
    nypi_hub::outln!("The {which} is stopped; its data is kept. nypi {which} up starts it again.");
    Ok(0)
}

/// `nypi relay up`: this machine runs the relay behind the entry (decision D94), beside
/// whatever instance it is.
pub async fn relay_up(paths: &Paths, hub_key: Option<&str>, listen: Option<&str>) -> anyhow::Result<i32> {
    let mut relay = settings(paths).relay;
    if let Some(key) = hub_key.map(str::trim).filter(|key| !key.is_empty()) {
        let key = key.to_ascii_lowercase();
        anyhow::ensure!(
            key.len() == 64 && key.chars().all(|c| c.is_ascii_hexdigit()),
            "the hub key is the 64-character fingerprint nypi hub up showed"
        );
        relay.hub_key = Some(key);
    }
    if let Some(listen) = listen.map(str::trim).filter(|listen| !listen.is_empty()) {
        relay.listen = listen.to_string();
    }
    // Opened once here, so a port that cannot be had is said now and not in a log.
    if nypi_hub::relay::running(paths).is_none() {
        drop(
            tokio::net::TcpListener::bind(&relay.listen)
                .await
                .map_err(|e| nypi_hub::relay::bind_error(&relay.listen, e))?,
        );
    }
    relay.enabled = true;
    Settings::save_global(paths, &["relay"], serde_json::to_value(&relay)?)?;
    supervisor::install(paths)?;
    match &relay.hub_key {
        Some(key) => nypi_hub::outln!("The relay runs on {}, for the hub key {key}.", relay.listen),
        None => nypi_hub::outln!(
            "The relay runs on {}. The first hub to register claims it; compare the key in nypi status with what nypi              hub up showed.",
            relay.listen
        ),
    }
    nypi_hub::outln!(
        "Point the entry's name at http://{} here: a Cloudflare Tunnel (cloudflared), or a reverse proxy with WebSocket          on. This machine's address need not be public.",
        relay.listen
    );
    Ok(0)
}

/// `nypi relay down`: the relay stops; its file is kept.
pub fn relay_down(paths: &Paths) -> anyhow::Result<i32> {
    Settings::save_global(paths, &["relay", "enabled"], serde_json::json!(false))?;
    // The last service of this machine: nothing is left for the supervisor to keep.
    if Instance::load(paths).is_none_or(|instance| instance.down) {
        supervisor::uninstall(paths)?;
    }
    nypi_hub::outln!("The relay is stopped. nypi relay up starts it again.");
    Ok(0)
}

/// `nypi node leave`.
pub async fn node_leave(paths: &Paths) -> anyhow::Result<i32> {
    let Some(instance) = Instance::load(paths).filter(|instance| !instance.is_hub()) else {
        anyhow::bail!("this machine is not a node of a hub")
    };
    let ways = instance::ways(&instance, &settings(paths));
    let told = match pairing::link(paths, &instance.hub_fingerprint, &ways, Purpose::Leave, 0).await {
        Err(error) if error.to_string().contains("left the hub") => "the hub forgot it".to_string(),
        Err(error) => {
            format!("the hub was not told ({error:#}); remove it there with nypi devices remove {}", instance.name)
        }
        Ok(_) => "the hub was told".to_string(),
    };
    if !settings(paths).relay.enabled {
        supervisor::uninstall(paths)?;
    }
    let _ = std::fs::remove_file(paths.instance());
    let _ = std::fs::remove_file(paths.device_key());
    let _ = std::fs::remove_file(paths.node_status());
    nypi_hub::outln!("This machine left its hub ({told}). Its key is forgotten; nypi node up joins again.");
    Ok(0)
}

/// `nypi status`.
pub async fn status(paths: &Paths) -> anyhow::Result<i32> {
    let settings = settings(paths);
    nypi_hub::outln!("NyPi {} ({}).", env!("CARGO_PKG_VERSION"), crate::update::TARGET);
    if let Some(pending) = crate::update::pending(paths) {
        nypi_hub::outln!(
            "Update to {} pending: the supervisor keeps it once every service stays up, or puts {} back.",
            pending.to_version,
            pending.from_version
        );
    }
    match Instance::load(paths) {
        None => nypi_hub::outln!("This machine is neither a hub nor a node: nypi hub up or nypi node up."),
        Some(instance) => {
            let state = if instance.down { " (stopped)" } else { "" };
            nypi_hub::outln!(
                "This machine is the {} {}{state}: {}, epoch {}, entry {}.",
                instance.mode.label(),
                instance.name,
                instance.trust.label(),
                instance.epoch,
                settings.entry().unwrap_or_else(|| "none (local network only)".into())
            );
            nypi_hub::outln!("Hub key: {}", instance.hub_fingerprint);
            if instance.is_hub() {
                let alive = nypi_hub::daemon::alive(paths, chrono::Utc::now());
                let links = if alive { nypi_hub::daemon::links_up(paths) } else { Vec::new() };
                let mut machines: Vec<&str> = links.iter().map(|link| link.key.as_str()).collect();
                machines.sort();
                machines.dedup();
                let direct = links.iter().filter(|link| link.path == nypi_hub::daemon::Via::Direct).count();
                nypi_hub::outln!(
                    "The hub is {}; {} machine(s) linked, {direct} of {} link(s) direct; {} join request(s) waiting.",
                    if alive { "running" } else { "NOT running" },
                    machines.len(),
                    links.len(),
                    pairing::pending(paths).len()
                );
            } else {
                match crate::node::read_status(paths).filter(|status| nypi_core::ledger::process_alive(status.pid)) {
                    Some(status) if status.connected => {
                        let path = if status.path == "relayed" {
                            format!("relayed through {}", status.address)
                        } else {
                            format!("direct ({})", status.address)
                        };
                        nypi_hub::outln!("Linked to the hub, {path}, since {}.", status.since)
                    }
                    Some(_) => nypi_hub::outln!("The node runs, and is not linked to the hub now."),
                    None => nypi_hub::outln!("The node is NOT running."),
                }
                if instance.trust == Trust::Personal {
                    match nypi_hub::standby::copy_age(paths) {
                        Some(age) => {
                            nypi_hub::outln!("Standby copy of the hub's state: {} minutes old.", age.num_minutes())
                        }
                        None => nypi_hub::outln!("No standby copy of the hub's state yet."),
                    }
                }
            }
        }
    }
    if settings.relay.enabled {
        let state = nypi_hub::relay::Relay::new(paths, settings.relay.clone()).state();
        let key = settings.relay.hub_key.clone().or(state.hub_key).unwrap_or_else(|| "none yet".into());
        nypi_hub::outln!(
            "The relay is on, at {}, for the hub key {key} (epoch {}).",
            settings.relay.listen,
            state.epoch
        );
    }
    for line in supervisor::report(paths) {
        nypi_hub::outln!("{line}");
    }
    Ok(0)
}

/// `nypi devices`: the admitted machines and browsers, with whether each is linked.
pub fn devices(paths: &Paths) -> anyhow::Result<i32> {
    on_the_hub(paths)?;
    let register = nypi_hub::nodes::read_register(paths);
    let running = nypi_hub::daemon::alive(paths, chrono::Utc::now());
    let peers = Peers::open(paths).list();
    if peers.is_empty() {
        nypi_hub::outln!("No machine is admitted yet. On another machine: nypi node up");
    }
    let links = if running { nypi_hub::daemon::links_up(paths) } else { Vec::new() };
    for (key, peer) in &peers {
        let paths_now: Vec<String> = links
            .iter()
            .filter(|link| &link.key == key && link.purpose == Purpose::Node)
            .map(|link| match link.path {
                nypi_hub::daemon::Via::Direct => format!("direct from {}", link.address),
                nypi_hub::daemon::Via::Relayed => "relayed".to_string(),
            })
            .collect();
        let linked = match register.get(&peer.name) {
            Some(node) if node.connected && running && !paths_now.is_empty() => {
                format!("linked, {}", paths_now.join(", "))
            }
            Some(node) if node.connected && running => "linked".to_string(),
            Some(node) => format!("last seen {}", node.since.get(..16).unwrap_or_default().replace('T', " ")),
            None if peer.kind == pairing::PeerKind::Browser => {
                format!("admitted {}", peer.paired_at.get(..16).unwrap_or_default().replace('T', " "))
            }
            None => "not seen yet".into(),
        };
        let kind = serde_json::to_value(peer.kind).ok().and_then(|kind| kind.as_str().map(str::to_string));
        // The NyPi version it last said it runs (decision D98).
        let version = register
            .get(&peer.name)
            .map(|node| node.report.identity.version.clone())
            .filter(|version| !version.is_empty())
            .map(|version| format!("   NyPi {version}"))
            .unwrap_or_default();
        nypi_hub::outln!(
            "{}   {} {}   {}   {linked}{version}",
            peer.name,
            peer.trust.label(),
            kind.unwrap_or_default(),
            if peer.system.is_empty() { "?" } else { &peer.system }
        );
    }
    let waiting = pairing::pending(paths).len();
    if waiting > 0 {
        nypi_hub::outln!("{waiting} join request(s) waiting: nypi devices pending");
    }
    Ok(0)
}

fn on_the_hub(paths: &Paths) -> anyhow::Result<()> {
    match Instance::load(paths) {
        Some(instance) if !instance.is_hub() => anyhow::bail!("nypi devices runs on the hub"),
        _ => Ok(()),
    }
}

pub fn pending(paths: &Paths) -> anyhow::Result<i32> {
    on_the_hub(paths)?;
    let pending = pairing::pending(paths);
    if pending.is_empty() {
        nypi_hub::outln!("No join request is waiting.");
    }
    for request in pending {
        nypi_hub::outln!("{}", request.question());
    }
    Ok(0)
}

/// `nypi devices admit <code> [--exec-only]` (`exec_only` Some) and `refuse <code>` (None).
pub fn answer(paths: &Paths, code: &str, exec_only: Option<bool>) -> anyhow::Result<i32> {
    on_the_hub(paths)?;
    let answer = match exec_only {
        None => JoinAnswer::Refuse,
        Some(true) => JoinAnswer::Admit { trust: Trust::ExecOnly },
        Some(false) => JoinAnswer::Admit { trust: Trust::Personal },
    };
    let request = pairing::answer(paths, code, answer).map_err(anyhow::Error::msg)?;
    match answer {
        JoinAnswer::Admit { trust } => nypi_hub::outln!("Admitted {} as {}.", request.name, trust.label()),
        JoinAnswer::Refuse => nypi_hub::outln!("Refused {}.", request.name),
    }
    Ok(0)
}

/// `nypi devices remove <name>`: its open links are closed at once by the daemon (decision D83).
pub async fn remove(paths: &Paths, name: &str) -> anyhow::Result<i32> {
    on_the_hub(paths)?;
    let gone = Peers::open(paths).remove(name)?;
    if gone == 0 {
        nypi_hub::outln!("No machine or browser named {name} is admitted.");
        return Ok(1);
    }
    let closed = match nypi_hub::link::peers_changed(paths).await {
        Ok(Some(closed)) => format!("; {closed} open link(s) of it were closed"),
        Ok(None) => String::new(),
        Err(error) => format!("; the hub was not told ({error:#}), so a link it holds ends within seconds"),
    };
    nypi_hub::outln!("{name} is removed{closed}.");
    Ok(0)
}

/// `30m`, `1h`, `2d`.
pub fn duration(text: &str) -> anyhow::Result<chrono::Duration> {
    let text = text.trim();
    let (number, unit) = text.split_at(text.len().saturating_sub(1));
    let number: i64 = number.parse().map_err(|_| anyhow::anyhow!("a duration reads 30m, 1h, or 2d"))?;
    match unit {
        "m" => Ok(chrono::Duration::minutes(number)),
        "h" => Ok(chrono::Duration::hours(number)),
        "d" => Ok(chrono::Duration::days(number)),
        _ => anyhow::bail!("a duration reads 30m, 1h, or 2d"),
    }
}

/// `nypi devices invite <name>`.
pub fn invite(paths: &Paths, name: &str, exec_only: bool, expires: &str) -> anyhow::Result<i32> {
    on_the_hub(paths)?;
    let trust = if exec_only { Trust::ExecOnly } else { Trust::Personal };
    let code = pairing::invite(paths, name, trust, duration(expires)?)?;
    nypi_hub::outln!("On {name}: {}", invite_line(&code, settings(paths).entry().as_deref()));
    nypi_hub::outln!("It works once, for {expires}, and admits the machine as {} with no question.", trust.label());
    Ok(0)
}

/// The command a machine joins with an invite: with the entry when the hub has one (decision
/// D97), else for a machine on the hub's own network.
fn invite_line(code: &str, entry: Option<&str>) -> String {
    match entry {
        Some(entry) => format!("nypi node up --code {code} --entry {entry}"),
        None => format!("nypi node up --code {code}   (on the hub's network)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hub needs no entry (decision D97): it is made, and is on the local network only, until
    /// one is given; one given is saved in the settings, where every later version reads it.
    #[tokio::test]
    async fn a_hub_needs_no_entry() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        std::fs::write(paths.settings(), r#"{ "envFiles": [] }"#).unwrap();
        let hub = become_hub(&paths, None, Some("mini")).await.unwrap();
        assert_eq!((hub.mode, hub.epoch, hub.name.as_str()), (Mode::Hub, 1, "mini"));
        assert_eq!(Instance::load(&paths), Some(hub));
        assert_eq!(settings(&paths).entry_with(None, None), None);
        assert!(keep_entry(&paths, Some("not an entry://x")).is_err());
        assert_eq!(keep_entry(&paths, Some("hub.example.com:443")).unwrap().as_deref(), Some("hub.example.com:443"));
        assert_eq!(settings(&paths).entry_with(None, None).as_deref(), Some("hub.example.com:443"));
    }

    /// An invite names the entry only when the hub has one (decision D97).
    #[test]
    fn an_invite_names_the_entry_only_when_there_is_one() {
        assert_eq!(
            invite_line("nypi-ab", Some("hub.example.com:443")),
            "nypi node up --code nypi-ab --entry hub.example.com:443"
        );
        let local = invite_line("nypi-ab", None);
        assert!(local.starts_with("nypi node up --code nypi-ab") && !local.contains("--entry"), "{local}");
    }

    #[test]
    fn durations_read_as_people_write_them() {
        assert_eq!(duration("30m").unwrap(), chrono::Duration::minutes(30));
        assert_eq!(duration("1h").unwrap(), chrono::Duration::hours(1));
        assert_eq!(duration("2d").unwrap(), chrono::Duration::days(2));
        assert!(duration("soon").is_err());
    }

    /// What the supervisor runs (decisions D93, D94): the hub's services on the hub, the node on
    /// a node, nothing when stopped, and the hub's on a machine that is neither yet; the relay
    /// beside any of them, and alone on a machine that is only a relay.
    #[test]
    fn the_supervisor_runs_what_this_machine_is() {
        use supervisor::{Services, services};
        let hub = Instance { mode: Mode::Hub, ..Default::default() };
        let node = Instance { mode: Mode::Node, ..Default::default() };
        let only = |hub, node, relay| Services { hub, node, relay };
        assert_eq!(services(Some(&hub), false), only(true, false, false));
        assert_eq!(services(Some(&node), false), only(false, true, false));
        assert_eq!(services(Some(&node), true), only(false, true, true), "a VPS: an exec-only node and the relay");
        assert_eq!(services(Some(&Instance { down: true, ..node.clone() }), false), Services::default());
        assert_eq!(services(Some(&Instance { down: true, ..node }), true), only(false, false, true));
        assert_eq!(services(None, false), only(true, false, false));
        assert_eq!(services(None, true), only(false, false, true), "a machine that is only a relay");
    }
}
