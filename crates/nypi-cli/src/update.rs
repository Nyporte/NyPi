//! `nypi update` (decision D98): this machine's NyPi from the public repository's releases,
//! never put in place unchecked and never left half replaced. The pipeline: fetch the release's
//! manifest and its signature, check the signature against the release key built into this
//! program (`nypi_core::update`), download the archive for this machine's target while hashing
//! it, refuse it unless its size and SHA-256 are the signed ones, unpack the one `nypi` it holds
//! (no other path, no link), run it with `--version` and refuse it unless it says the version
//! expected, keep the binary in place as `nypi.prev`, note the update as pending, and rename the
//! new one over `~/.nypi/bin/nypi`. The supervisor confirms a pending update once its services
//! stay up, and puts `nypi.prev` back when they do not.

use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use nypi_core::config::{Paths, Settings};
use nypi_core::update::{Manifest, Source, Version};
use serde::{Deserialize, Serialize};

/// The target triple this binary was built for: which archive of a release is this machine's.
pub const TARGET: &str = env!("NYPI_TARGET");
/// How long a new binary gets to say its version.
const PREFLIGHT_WAIT: Duration = Duration::from_secs(5);
/// The largest manifest or signature taken.
const MAX_SMALL: usize = 1 << 20;

/// The binary the supervisor runs.
pub fn binary(paths: &Paths) -> PathBuf {
    paths.home.join("bin").join("nypi")
}

fn previous(paths: &Paths) -> PathBuf {
    paths.home.join("bin").join("nypi.prev")
}

fn pending_file(paths: &Paths) -> PathBuf {
    paths.home.join("run").join("update-pending.json")
}

fn state_file(paths: &Paths) -> PathBuf {
    paths.home.join("run").join("update-state.json")
}

/// An update put in place that the supervisor has not confirmed yet.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Pending {
    pub from_version: String,
    pub to_version: String,
    pub started_at: String,
    pub checked_at: String,
    /// Put in place by the supervisor itself, unasked: the user is told when it is confirmed.
    pub automatic: bool,
    /// The supervisor has stopped what ran the old binary once (decision D99).
    #[serde(default)]
    pub restarted: bool,
    /// How many times the supervisor has started while this update was pending.
    #[serde(default)]
    pub supervisor_starts: u32,
}

/// What the updater remembers between checks.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    /// The manifest's `ETag`, and the version it named, so an unchanged release costs a 304.
    pub etag: Option<String>,
    pub latest: Option<String>,
    pub last_check: Option<String>,
    /// The version the user was last told of, once.
    pub notified: Option<String>,
    /// A version that did not stay up here: never applied again in the background.
    pub blocked_version: Option<String>,
    pub last_applied: Option<String>,
}

fn read_json<T: serde::de::DeserializeOwned + Default>(file: &Path) -> T {
    std::fs::read_to_string(file).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn write_json<T: Serialize>(file: &Path, value: &T) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let draft = file.with_extension("json.tmp");
    std::fs::write(&draft, serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?)?;
    std::fs::rename(draft, file)
}

pub fn pending(paths: &Paths) -> Option<Pending> {
    pending_file(paths).exists().then(|| read_json(&pending_file(paths)))
}

pub fn save_pending(paths: &Paths, pending: &Pending) {
    let _ = write_json(&pending_file(paths), pending);
}

pub fn clear_pending(paths: &Paths) {
    let _ = std::fs::remove_file(pending_file(paths));
}

pub fn state(paths: &Paths) -> State {
    read_json(&state_file(paths))
}

pub fn save_state(paths: &Paths, state: &State) {
    let _ = write_json(&state_file(paths), state);
}

/// What `apply` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    /// Nothing newer: this is the version.
    UpToDate { version: String },
    /// The new binary is in place, pending the supervisor's confirmation.
    Swapped { from: String, to: String },
}

/// What `check` found.
#[derive(Clone, Debug)]
pub struct Checked {
    pub current: Version,
    pub latest: Manifest,
    pub newer: bool,
    pub page: String,
}

/// What `apply` may do.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// This version (`v0.4.1` or `0.4.1`), older ones too; else the latest, when it is newer.
    pub version: Option<String>,
    /// Take a manifest with no valid signature: a development build without a release key, by
    /// hand only.
    pub allow_unsigned: bool,
}

/// Where releases come from, and how they are checked.
pub struct Updater {
    paths: Paths,
    source: Source,
    /// The release key (base64); None in a development build.
    key: Option<String>,
    target: String,
    current: Version,
    http: reqwest::Client,
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("nypi/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .build()
        .unwrap_or_default()
}

impl Updater {
    /// This machine's updater: the repository in its settings, the key built into the program.
    pub fn new(paths: &Paths, settings: &Settings) -> anyhow::Result<Updater> {
        let source = Source::github(&settings.update.repo).map_err(anyhow::Error::msg)?;
        let key = nypi_core::update::release_public_key().map(str::to_string);
        Ok(Updater {
            paths: paths.clone(),
            source,
            key,
            target: TARGET.into(),
            current: Version::current(),
            http: http(),
        })
    }

    /// An updater for tests: a loopback source, a test key, a target, and the version to call
    /// this one.
    #[cfg(test)]
    pub fn with(paths: &Paths, source: Source, key: Option<String>, target: &str, current: Version) -> Updater {
        Updater { paths: paths.clone(), source, key, target: target.into(), current, http: http() }
    }

    async fn get(&self, url: &str, limit: usize) -> anyhow::Result<Vec<u8>> {
        let response = self.http.get(url).send().await?;
        anyhow::ensure!(response.status().is_success(), "{url} answered {}", response.status());
        let bytes = response.bytes().await?;
        anyhow::ensure!(bytes.len() <= limit, "{url} is larger than a manifest is");
        Ok(bytes.to_vec())
    }

    /// The release manifest (latest, or `tag`), its signature checked. `etag`: what the last
    /// check saw; None comes back when the release has not changed since.
    pub async fn manifest(
        &self,
        tag: Option<&str>,
        allow_unsigned: bool,
        etag: Option<&str>,
    ) -> anyhow::Result<Option<(Manifest, Option<String>)>> {
        let (url, signature_url) = self.source.manifest(tag);
        let mut request = self.http.get(&url);
        if let Some(etag) = etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let response = request.send().await?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        anyhow::ensure!(response.status().is_success(), "no release manifest at {url} ({})", response.status());
        let etag = response.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_string);
        let bytes = response.bytes().await?.to_vec();
        anyhow::ensure!(bytes.len() <= MAX_SMALL, "the release manifest is too large");
        let manifest = match &self.key {
            Some(key) => {
                let signature = self.get(&signature_url, MAX_SMALL).await?;
                let signature = String::from_utf8_lossy(&signature).into_owned();
                nypi_core::update::verify(&bytes, &signature, key).map_err(anyhow::Error::msg)?
            }
            None if allow_unsigned => {
                eprintln!("Warning: this build has no release key; the manifest's signature is not checked.");
                nypi_core::update::read_unsigned(&bytes).map_err(anyhow::Error::msg)?
            }
            None => anyhow::bail!(
                "this build has no release key (NYPI_RELEASE_PUBKEY), so no release can be checked; a development \
                 build may pass --allow-unsigned by hand"
            ),
        };
        Ok(Some((manifest, etag)))
    }

    /// The latest release, and whether it is newer than this version.
    pub async fn check(&self, allow_unsigned: bool) -> anyhow::Result<Checked> {
        let (latest, _) =
            self.manifest(None, allow_unsigned, None).await?.ok_or_else(|| anyhow::anyhow!("no manifest"))?;
        let version = Version::parse(&latest.version).ok_or_else(|| anyhow::anyhow!("the release's version"))?;
        let page = self.source.page(&latest.tag);
        Ok(Checked { newer: version.newer_than(&self.current), current: self.current.clone(), latest, page })
    }

    /// Put a release in place: see the module's comment. Nothing of the installed binary changes
    /// unless every check passes.
    pub async fn apply(&self, options: &Options, automatic: bool) -> anyhow::Result<Applied> {
        let tag = options.version.as_deref().map(nypi_core::update::tag_of);
        let (manifest, _) = self
            .manifest(tag.as_deref(), options.allow_unsigned && !automatic, None)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no manifest"))?;
        let version = Version::parse(&manifest.version)
            .ok_or_else(|| anyhow::anyhow!("the release's version {} does not read", manifest.version))?;
        if options.version.is_none() && !version.newer_than(&self.current) {
            return Ok(Applied::UpToDate { version: self.current.to_string() });
        }
        let asset = manifest
            .assets
            .get(&self.target)
            .ok_or_else(|| anyhow::anyhow!("release {} has no build for {}", manifest.version, self.target))?;
        let bin = self.paths.home.join("bin");
        let staging = bin.join(".staging");
        std::fs::create_dir_all(&staging)?;
        let archive = staging.join(format!("{}.tar.gz", uuid::Uuid::new_v4().simple()));
        let outcome = self.put_in_place(&manifest, asset, &archive, &version, automatic).await;
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_file(bin.join("nypi.new"));
        outcome
    }

    async fn put_in_place(
        &self,
        manifest: &Manifest,
        asset: &nypi_core::update::Asset,
        archive: &Path,
        version: &Version,
        automatic: bool,
    ) -> anyhow::Result<Applied> {
        self.download(&self.source.asset(&manifest.tag, &asset.file), archive, asset).await?;
        let new = self.paths.home.join("bin").join("nypi.new");
        let (from, to) = (archive.to_path_buf(), new.clone());
        tokio::task::spawn_blocking(move || extract(&from, &to)).await?.map_err(anyhow::Error::msg)?;
        preflight(&new, version).await?;
        swap(&self.paths, &new, &self.current.to_string(), &version.to_string(), automatic)?;
        Ok(Applied::Swapped { from: self.current.to_string(), to: version.to_string() })
    }

    /// Download `url` to `to`, refusing it unless it is exactly `asset`'s size and SHA-256.
    async fn download(&self, url: &str, to: &Path, asset: &nypi_core::update::Asset) -> anyhow::Result<()> {
        let mut response = self.http.get(url).send().await?;
        anyhow::ensure!(response.status().is_success(), "{url} answered {}", response.status());
        let mut file = std::fs::File::create(to)?;
        let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
        let mut size = 0u64;
        while let Some(chunk) = response.chunk().await? {
            size += chunk.len() as u64;
            anyhow::ensure!(size <= asset.size, "the archive is larger than the signed size");
            digest.update(&chunk);
            file.write_all(&chunk)?;
        }
        file.sync_all()?;
        anyhow::ensure!(size == asset.size, "the archive is {size} bytes, not the signed {}", asset.size);
        let sha: String = digest.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect();
        anyhow::ensure!(sha.eq_ignore_ascii_case(asset.sha256.trim()), "the archive's SHA-256 is not the signed one");
        Ok(())
    }
}

/// Unpack the one `nypi` of a release archive to `to`, executable. An archive with a path that
/// leaves it, a link, or no `nypi` is refused whole.
pub fn extract(archive: &Path, to: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut found = false;
    for entry in tar.entries().map_err(|e| format!("the archive does not read: {e}"))? {
        let mut entry = entry.map_err(|e| format!("the archive does not read: {e}"))?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let parts: Vec<Component> = path.components().filter(|part| *part != Component::CurDir).collect();
        if path.is_absolute() || parts.iter().any(|part| !matches!(part, Component::Normal(_))) {
            return Err(format!("the archive has a path that leaves it: {}", path.display()));
        }
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            return Err(format!("the archive has a link: {}", path.display()));
        }
        let is_binary = parts.len() == 1 && parts[0].as_os_str() == "nypi";
        if !is_binary {
            continue;
        }
        if !kind.is_file() || found {
            return Err("the archive's nypi is not one plain file".into());
        }
        let mut out =
            std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(to).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
        out.sync_all().map_err(|e| e.to_string())?;
        drop(out);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        found = true;
    }
    if !found {
        return Err("the archive holds no nypi".into());
    }
    Ok(())
}

/// The new binary runs here and says the version expected, within five seconds; else it is
/// refused (another architecture, a missing library, a broken file).
pub async fn preflight(binary: &Path, version: &Version) -> anyhow::Result<()> {
    let run = tokio::process::Command::new(binary).arg("--version").kill_on_drop(true).output();
    let output = tokio::time::timeout(PREFLIGHT_WAIT, run)
        .await
        .map_err(|_| anyhow::anyhow!("the new binary did not answer --version in time"))??;
    let said = String::from_utf8_lossy(&output.stdout);
    anyhow::ensure!(output.status.success(), "the new binary does not run here ({})", output.status);
    anyhow::ensure!(
        said.starts_with(&format!("nypi {version}")),
        "the new binary says {:?}, not nypi {version}",
        said.lines().next().unwrap_or_default()
    );
    Ok(())
}

/// Keep the installed binary as `nypi.prev`, note the update as pending, and rename the new one
/// over it: each step a rename, so no reader ever sees half a binary.
fn swap(paths: &Paths, new: &Path, from: &str, to: &str, automatic: bool) -> anyhow::Result<()> {
    let installed = binary(paths);
    if installed.exists() {
        let draft = paths.home.join("bin").join("nypi.prev.tmp");
        std::fs::copy(&installed, &draft)?;
        std::fs::rename(&draft, previous(paths))?;
    }
    let now = chrono::Utc::now().to_rfc3339();
    let pending = Pending {
        from_version: from.into(),
        to_version: to.into(),
        started_at: now.clone(),
        checked_at: now,
        automatic,
        ..Default::default()
    };
    write_json(&pending_file(paths), &pending)?;
    std::fs::rename(new, &installed)?;
    Ok(())
}

/// `nypi update rollback`: put `nypi.prev` back. The version restored, from the pending note.
pub fn rollback(paths: &Paths) -> anyhow::Result<String> {
    let prev = previous(paths);
    anyhow::ensure!(prev.exists(), "there is no previous binary ({}) to go back to", prev.display());
    std::fs::rename(&prev, binary(paths))?;
    let from = pending(paths).map(|pending| pending.from_version).unwrap_or_default();
    clear_pending(paths);
    Ok(from)
}

/// When the binary a user typed `nypi` with is not the one the supervisor runs, the two can
/// differ after an update: a note on how to keep them one.
pub fn same_binary_note(paths: &Paths) -> Option<String> {
    let current = std::env::current_exe().ok()?;
    let installed = binary(paths);
    if current.canonicalize().ok()? == installed.canonicalize().ok()? {
        return None;
    }
    let linked = std::fs::symlink_metadata(&current).is_ok_and(|meta| meta.file_type().is_symlink());
    (!linked).then(|| {
        format!(
            "This nypi ({}) is a copy of its own; to keep it the one the supervisor runs: ln -sf {} {}",
            current.display(),
            installed.display(),
            current.display()
        )
    })
}

/// A new release in the background (supervisor, decision D98): checked every
/// `checkIntervalHours` (with this machine's own offset, so a fleet does not ask at once), and
/// told once per version; in `auto` mode put in place when `idle` says the machine is idle and
/// the window is open. True when a new binary was put in place (the supervisor then restarts).
pub async fn background(
    paths: &Paths,
    settings: &Settings,
    updater: &Updater,
    idle: impl Fn() -> bool,
    tell: impl AsyncFnOnce(String),
) -> bool {
    use nypi_core::config::UpdateMode;
    let mode = settings.update.effective_mode(settings.relay.enabled);
    if mode == UpdateMode::Off {
        return false;
    }
    let mut state = state(paths);
    let now = chrono::Utc::now();
    let every = chrono::Duration::seconds((settings.update.check_interval_hours.max(0.1) * 3600.0) as i64);
    let offset = chrono::Duration::seconds((machine_offset(paths) % 600) as i64);
    let due = state
        .last_check
        .as_deref()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .is_none_or(|last| now >= last.with_timezone(&chrono::Utc) + every + offset);
    if !due {
        return false;
    }
    state.last_check = Some(now.to_rfc3339());
    let fetched = updater.manifest(None, false, state.etag.as_deref()).await;
    let latest = match fetched {
        Ok(Some((manifest, etag))) => {
            state.etag = etag;
            state.latest = Some(manifest.version.clone());
            manifest.version
        }
        Ok(None) => match state.latest.clone() {
            Some(latest) => latest,
            None => {
                save_state(paths, &state);
                return false;
            }
        },
        Err(_) => {
            save_state(paths, &state);
            return false;
        }
    };
    let newer = Version::parse(&latest).is_some_and(|latest| latest.newer_than(&updater.current));
    let blocked = state.blocked_version.as_deref() == Some(latest.as_str());
    if !newer || blocked {
        save_state(paths, &state);
        return false;
    }
    let window_open = settings.update.auto_apply_window.as_deref().is_none_or(|window| {
        use chrono::Timelike;
        let local = chrono::Local::now();
        nypi_core::update::in_window(Some(window), local.hour() * 60 + local.minute())
    });
    if mode == UpdateMode::Auto && window_open && idle() {
        save_state(paths, &state);
        let options = Options { version: Some(latest.clone()), allow_unsigned: false };
        match updater.apply(&options, true).await {
            Ok(applied) => return matches!(applied, Applied::Swapped { .. }),
            // Said once per version, not failed silently at every check (decision D99).
            Err(error) => {
                eprintln!("automatic update to {latest} failed: {error:#}");
                let mut state = self::state(paths);
                if state.notified.as_deref() != Some(latest.as_str()) {
                    state.notified = Some(latest.clone());
                    save_state(paths, &state);
                    tell(format!(
                        "NyPi v{latest} is available, and automatic update failed ({error:#}) — run nypi update"
                    ))
                    .await;
                }
                return false;
            }
        }
    }
    if state.notified.as_deref() != Some(latest.as_str()) {
        state.notified = Some(latest.clone());
        save_state(paths, &state);
        tell(format!("NyPi v{latest} is available — run nypi update")).await;
        return false;
    }
    save_state(paths, &state);
    false
}

/// A number of this machine's own, from its home's path, so machines check at different times.
fn machine_offset(paths: &Paths) -> u64 {
    paths.home.display().to_string().bytes().fold(7u64, |acc, byte| acc.wrapping_mul(31).wrapping_add(byte as u64))
}

/// What the supervisor does with a pending update this round (decision D98).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Not yet: the services have not been up long enough.
    Wait,
    /// Every service has stayed up for 30 seconds: the update holds.
    Confirm,
    /// A service kept stopping, or never came up within 90 seconds: put the old binary back.
    RollBack,
}

pub const CONFIRM_AFTER: Duration = Duration::from_secs(30);
pub const GIVE_UP_AFTER: Duration = Duration::from_secs(90);

/// The verdict on `pending` at `now`: `healthy_for` is how long every service of this machine
/// has been up together (None while one is down), `crashed` whether one kept stopping.
pub fn verdict(
    pending: &Pending,
    now: chrono::DateTime<chrono::Utc>,
    healthy_for: Option<Duration>,
    crashed: bool,
) -> Verdict {
    if crashed {
        return Verdict::RollBack;
    }
    if healthy_for.is_some_and(|up| up >= CONFIRM_AFTER) {
        return Verdict::Confirm;
    }
    let started = chrono::DateTime::parse_from_rfc3339(&pending.started_at).map(|at| at.with_timezone(&chrono::Utc));
    let late = started.is_ok_and(|at| now - at > chrono::Duration::from_std(GIVE_UP_AFTER).unwrap_or_default());
    if late && healthy_for.is_none() { Verdict::RollBack } else { Verdict::Wait }
}

/// The update held: forget the pending note, and remember it as applied.
pub fn confirm(paths: &Paths) -> Option<Pending> {
    let pending = pending(paths)?;
    clear_pending(paths);
    let mut state = state(paths);
    state.last_applied = Some(pending.to_version.clone());
    save_state(paths, &state);
    Some(pending)
}

/// The update did not hold: put `nypi.prev` back, never try that version again in the
/// background, and say so. What to tell the user.
pub fn roll_back_failed(paths: &Paths) -> anyhow::Result<String> {
    let pending = pending(paths).unwrap_or_default();
    std::fs::rename(previous(paths), binary(paths))?;
    let mut state = state(paths);
    state.blocked_version = Some(pending.to_version.clone());
    save_state(paths, &state);
    clear_pending(paths);
    Ok(format!(
        "Failed: update to v{} did not stay healthy; rolled back to v{}.",
        pending.to_version, pending.from_version
    ))
}

/// Whether a rollback is possible: there is a previous binary.
pub fn can_roll_back(paths: &Paths) -> bool {
    previous(paths).exists()
}

/// `nypi update keygen`: a release signing key. The private half goes to `out` (owner-only);
/// the public half is printed, to be built in with `NYPI_RELEASE_PUBKEY`.
pub fn keygen(out: &Path) -> anyhow::Result<String> {
    anyhow::ensure!(!out.exists(), "{} exists already; it is not overwritten", out.display());
    let pair = nypi_core::update::generate().map_err(anyhow::Error::msg)?;
    use base64::Engine;
    let text = base64::engine::general_purpose::STANDARD.encode(&pair.private_pkcs8);
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(out)?;
    file.write_all(text.as_bytes())?;
    Ok(pair.public)
}

/// `nypi update sign <manifest> [--key <file>]`: write `<manifest>.sig`. The key comes from the
/// file, or from `NYPI_RELEASE_SIGNING_KEY` (base64), as a release pipeline gives it.
pub fn sign(manifest: &Path, key: Option<&Path>) -> anyhow::Result<PathBuf> {
    let private = match key {
        Some(file) => std::fs::read(file)?,
        None => std::env::var("NYPI_RELEASE_SIGNING_KEY")
            .map_err(|_| anyhow::anyhow!("no key: pass --key <file> or set NYPI_RELEASE_SIGNING_KEY"))?
            .into_bytes(),
    };
    let private = nypi_core::update::read_private(&private).map_err(anyhow::Error::msg)?;
    let bytes = std::fs::read(manifest)?;
    nypi_core::update::read_unsigned(&bytes).map_err(anyhow::Error::msg)?;
    let signature = nypi_core::update::sign(&private, &bytes).map_err(anyhow::Error::msg)?;
    let out = PathBuf::from(format!("{}.sig", manifest.display()));
    std::fs::write(&out, signature)?;
    Ok(out)
}

/// `nypi update --node <name>`, in the hub (decision D98): the node checks and puts the release
/// in place itself, against its own release key, and says what it did.
pub async fn update_node(
    nodes: &nypi_hub::nodes::Nodes,
    name: &str,
    version: Option<String>,
) -> anyhow::Result<String> {
    let node = nodes.get(name).ok_or_else(|| anyhow::anyhow!("no node named {name} is connected"))?;
    node.update(version, Duration::from_secs(900)).await.map_err(|why| anyhow::anyhow!("{name}: {why}"))
}

/// How long a node gets to come back on its new version.
const BACK_WAIT: Duration = Duration::from_secs(180);

/// `nypi update --all`, in the hub: every node in turn (one that runs the relay only when
/// `include_relay`), each back on the new version before the next, stopping at the first that
/// fails; then the hub itself, restarted last.
pub async fn update_all(
    paths: &Paths,
    nodes: &nypi_hub::nodes::Nodes,
    version: Option<String>,
    include_relay: bool,
) -> anyhow::Result<i32> {
    let settings = Settings::load(paths, &paths.home).unwrap_or_default();
    let updater = Updater::new(paths, &settings)?;
    let target = match version {
        Some(version) => nypi_core::update::tag_of(&version).trim_start_matches('v').to_string(),
        None => updater.check(false).await?.latest.version,
    };
    nypi_hub::outln!("Updating every machine to NyPi {target}, one by one.");
    for (name, node) in nodes.paired() {
        let report = nypi_node::NodeLink::report(&*node);
        if report.relay && !include_relay {
            nypi_hub::outln!("{name}: left alone, it runs the relay (--include-relay updates it too).");
            continue;
        }
        if report.identity.version == target {
            nypi_hub::outln!("{name}: on {target} already.");
            continue;
        }
        let said = update_node(nodes, &name, Some(target.clone())).await;
        let said = match said {
            Ok(said) => said,
            Err(why) => anyhow::bail!("stopped at {name}: {why:#}"),
        };
        nypi_hub::outln!("{name}: {said}");
        let back = tokio::time::timeout(BACK_WAIT, async {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let now = nodes.get(&name).map(|node| nypi_node::NodeLink::report(&*node).identity.version);
                if now.as_deref() == Some(target.as_str()) {
                    return;
                }
            }
        });
        if back.await.is_err() {
            anyhow::bail!("stopped at {name}: it did not come back on {target} within three minutes");
        }
        nypi_hub::outln!("{name}: back, on {target}.");
    }
    let options = Options { version: Some(target.clone()), allow_unsigned: false };
    match updater.apply(&options, false).await? {
        Applied::UpToDate { version } => nypi_hub::outln!("The hub is on {version} already."),
        Applied::Swapped { from, to } => {
            nypi_hub::outln!("The hub: {to} is in place (was {from}); it starts again in a moment.");
            // After the answer has gone back to whoever asked.
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let _ = crate::supervisor::restart();
            });
        }
    }
    Ok(0)
}

/// What a node does when the hub asks it to update (decision D98): put the release in place
/// with its own updater and key, answer, then start its supervisor again on it.
pub async fn update_for_hub(paths: &Paths, version: Option<String>) -> Result<String, String> {
    let settings = Settings::load(paths, &paths.home).unwrap_or_default();
    let updater = Updater::new(paths, &settings).map_err(|error| format!("{error:#}"))?;
    let options = Options { version, allow_unsigned: false };
    match updater.apply(&options, false).await.map_err(|error| format!("{error:#}"))? {
        Applied::UpToDate { version } => Ok(format!("on {version} already")),
        Applied::Swapped { from, to } => {
            // After the answer has gone to the hub.
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let _ = crate::supervisor::restart();
            });
            Ok(format!("{to} in place (was {from}); starting again on it"))
        }
    }
}
