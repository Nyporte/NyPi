//! The secret store (decision D77): passwords, API keys, and tokens Lucia is trusted to use but
//! never to see. Each is named by an opaque handle, `cred://<scope>/<name>`, which is all that
//! ever appears in a prompt, a note, the asset registry, or a tool call. The values live in one
//! file, `secrets.enc`, sealed entry by entry with ChaCha20-Poly1305 under a master key that is
//! kept apart from it (the login keychain on a Mac, an owner-only file elsewhere). A tool that
//! needs a secret asks the harness to use it (`http_request`'s `auth`), under the Inspector's
//! approval, and whatever comes back is scrubbed of every secret's value before the model, the
//! session, or the audit log sees it (`SecretScrubber`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

/// How every handle begins.
pub const SCHEME: &str = "cred://";
/// A value shorter than this is not searched for in output: it would match by chance.
const MIN_SCRUBBED: usize = 6;
const KEYCHAIN_SERVICE: &str = "sh.nypi.secrets";
const KEYCHAIN_ACCOUNT: &str = "master";

/// The name of one secret: a scope (a service, a host) and a name within it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Handle {
    pub scope: String,
    pub name: String,
}

impl Handle {
    /// Read `cred://scope/name`, or `scope/name` as the owner types it.
    pub fn parse(text: &str) -> Result<Handle, String> {
        let text = text.trim();
        let rest = text.strip_prefix(SCHEME).unwrap_or(text);
        let allowed = |part: &str| {
            !part.is_empty()
                && part.len() <= 64
                && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        };
        match rest.split_once('/') {
            Some((scope, name)) if allowed(scope) && allowed(name) => {
                Ok(Handle { scope: scope.to_string(), name: name.to_string() })
            }
            _ => Err(format!(
                "{text} is not a secret handle: write cred://<scope>/<name>, each of letters, digits, `.`, `_`, `-`"
            )),
        }
    }
}

impl std::fmt::Display for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{SCHEME}{}/{}", self.scope, self.name)
    }
}

/// A secret's value. It has no `Display`, and `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Secret {
        Secret(value.into())
    }

    /// The value, for the one place that hands it to the outside (a request's header).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Serialize, Deserialize)]
struct Sealed {
    /// Base64 of the 96-bit nonce, new for every write.
    nonce: String,
    /// Base64 of the ciphertext and its tag.
    sealed: String,
    updated: String,
}

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    entries: BTreeMap<String, Sealed>,
}

impl Default for File {
    fn default() -> Self {
        File { version: 1, entries: BTreeMap::new() }
    }
}

pub struct SecretStore {
    path: PathBuf,
    key: [u8; 32],
}

impl std::fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretStore").field("path", &self.path).field("key", &"<redacted>").finish()
    }
}

impl SecretStore {
    /// The store in `path`, sealed with `key`: for tests, and for whoever provisions the key
    /// itself. Nothing is read until a secret is asked for.
    pub fn with_master_key(path: PathBuf, key: [u8; 32]) -> SecretStore {
        SecretStore { path, key }
    }

    /// The store of the NyPi home `home`, with its master key found or made (`master_key`).
    pub fn open(home: &Path) -> anyhow::Result<SecretStore> {
        Ok(SecretStore { path: home.join("secrets.enc"), key: master_key(home)? })
    }

    fn cipher(&self) -> anyhow::Result<LessSafeKey> {
        let key =
            UnboundKey::new(&CHACHA20_POLY1305, &self.key).map_err(|_| anyhow!("the master key is not usable"))?;
        Ok(LessSafeKey::new(key))
    }

    fn load(&self) -> anyhow::Result<File> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not a secret store this version reads", self.path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
            Err(error) => Err(error).with_context(|| format!("cannot read {}", self.path.display())),
        }
    }

    /// Write the whole store through a draft, owner-only, so a crash never leaves half of it.
    fn save(&self, file: &File) -> anyhow::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let draft = self.path.with_extension("enc.tmp");
        let _ = std::fs::remove_file(&draft);
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true).mode(0o600);
        let mut handle = options.open(&draft).with_context(|| format!("cannot write {}", draft.display()))?;
        std::io::Write::write_all(&mut handle, &serde_json::to_vec_pretty(file)?)?;
        handle.sync_all()?;
        std::fs::rename(&draft, &self.path).with_context(|| format!("cannot write {}", self.path.display()))
    }

    /// Keep `value` under `handle`, replacing what was there.
    pub fn set(&self, handle: &Handle, value: &str) -> anyhow::Result<()> {
        if value.is_empty() {
            bail!("an empty secret is no secret");
        }
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).map_err(|_| anyhow!("the system gave no random bytes"))?;
        let mut sealed = value.as_bytes().to_vec();
        // The handle is bound into the seal, so an entry moved under another name does not open.
        self.cipher()?
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(handle.to_string().into_bytes()),
                &mut sealed,
            )
            .map_err(|_| anyhow!("the secret could not be sealed"))?;
        let mut file = self.load()?;
        file.entries.insert(
            format!("{}/{}", handle.scope, handle.name),
            Sealed { nonce: STANDARD.encode(nonce), sealed: STANDARD.encode(sealed), updated: now() },
        );
        self.save(&file)
    }

    fn open_entry(&self, handle: &Handle, entry: &Sealed) -> anyhow::Result<Secret> {
        let nonce: [u8; NONCE_LEN] = STANDARD
            .decode(&entry.nonce)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| anyhow!("{handle} is damaged in the store"))?;
        let mut sealed = STANDARD.decode(&entry.sealed).map_err(|_| anyhow!("{handle} is damaged in the store"))?;
        let opened = self
            .cipher()?
            .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(handle.to_string().into_bytes()), &mut sealed)
            .map_err(|_| anyhow!("{handle} does not open with this master key"))?;
        Ok(Secret(String::from_utf8_lossy(opened).into_owned()))
    }

    /// The secret under `handle`, or None when there is none.
    pub fn get(&self, handle: &Handle) -> anyhow::Result<Option<Secret>> {
        let file = self.load()?;
        match file.entries.get(&format!("{}/{}", handle.scope, handle.name)) {
            Some(entry) => self.open_entry(handle, entry).map(Some),
            None => Ok(None),
        }
    }

    /// Take the secret under `handle` away. False when there was none.
    pub fn remove(&self, handle: &Handle) -> anyhow::Result<bool> {
        let mut file = self.load()?;
        let removed = file.entries.remove(&format!("{}/{}", handle.scope, handle.name)).is_some();
        if removed {
            self.save(&file)?;
        }
        Ok(removed)
    }

    /// Every handle and when it was last set. Never a value.
    pub fn list(&self) -> anyhow::Result<Vec<(Handle, String)>> {
        let file = self.load()?;
        Ok(file
            .entries
            .iter()
            .filter_map(|(name, entry)| Handle::parse(name).ok().map(|handle| (handle, entry.updated.clone())))
            .collect())
    }

    /// Every secret that opens, for the scrubber: output is searched for all of them, not only
    /// the one a call used.
    pub fn all(&self) -> Vec<(Handle, Secret)> {
        let Ok(file) = self.load() else { return Vec::new() };
        file.entries
            .iter()
            .filter_map(|(name, entry)| {
                let handle = Handle::parse(name).ok()?;
                let secret = self.open_entry(&handle, entry).ok()?;
                Some((handle, secret))
            })
            .collect()
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn key_from_hex(text: &str) -> Option<[u8; 32]> {
    let text = text.trim();
    if text.len() != 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(key)
}

fn hex(key: &[u8; 32]) -> String {
    key.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The master key of the NyPi home `home`, made on first use. On a Mac with the default home it
/// is an entry of the login keychain (`sh.nypi.secrets`), so the key is not on the same disk in
/// the clear as the store it opens. Anywhere else (Linux, a host with no keychain session, a
/// home other than the default one, `NYPI_SECRETS_KEYCHAIN=off`), or once `secrets.key` exists,
/// it is that owner-only file, which `Protected` keeps every tool from reading.
pub fn master_key(home: &Path) -> anyhow::Result<[u8; 32]> {
    let file = home.join("secrets.key");
    if let Ok(text) = std::fs::read_to_string(&file) {
        return key_from_hex(&text).ok_or_else(|| anyhow!("{} does not hold a master key", file.display()));
    }
    let default_home = dirs::home_dir().map(|user| user.join(".nypi"));
    let keychain = cfg!(target_os = "macos")
        && default_home.as_deref() == Some(home)
        && !matches!(std::env::var("NYPI_SECRETS_KEYCHAIN").as_deref(), Ok("off"));
    if keychain {
        if let Some(key) = keychain_read() {
            return Ok(key);
        }
        let key = new_key()?;
        if keychain_write(&key) {
            return Ok(key);
        }
    }
    let key = new_key()?;
    write_key_file(&file, &key)?;
    Ok(key)
}

/// Keep `key` as the master key of the NyPi home `home`, where `master_key` will find it: the
/// login keychain when `master_key` would use it, else `secrets.key`, replaced (decision D95: the
/// hub's store moved here with its key). The key is never an argument or a line of a log.
pub fn install_master_key(home: &Path, key: &[u8; 32]) -> anyhow::Result<()> {
    let file = home.join("secrets.key");
    let default_home = dirs::home_dir().map(|user| user.join(".nypi"));
    let keychain = cfg!(target_os = "macos")
        && default_home.as_deref() == Some(home)
        && !matches!(std::env::var("NYPI_SECRETS_KEYCHAIN").as_deref(), Ok("off"));
    if keychain && keychain_write(key) {
        let _ = std::fs::remove_file(&file);
        return Ok(());
    }
    let _ = std::fs::remove_file(&file);
    write_key_file(&file, key)
}

/// The key as 64 hex characters and back, for carrying it between machines.
pub fn key_to_hex(key: &[u8; 32]) -> String {
    hex(key)
}

pub fn key_from_text(text: &str) -> Option<[u8; 32]> {
    key_from_hex(text)
}

fn new_key() -> anyhow::Result<[u8; 32]> {
    let mut key = [0u8; 32];
    SystemRandom::new().fill(&mut key).map_err(|_| anyhow!("the system gave no random bytes"))?;
    Ok(key)
}

fn write_key_file(file: &Path, key: &[u8; 32]) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true).mode(0o600);
    let mut handle = options.open(file).with_context(|| format!("cannot write {}", file.display()))?;
    std::io::Write::write_all(&mut handle, hex(key).as_bytes())?;
    Ok(())
}

fn keychain_read() -> Option<[u8; 32]> {
    let output = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-a", KEYCHAIN_ACCOUNT, "-w"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    output.status.success().then(|| key_from_hex(&String::from_utf8_lossy(&output.stdout))).flatten()
}

/// Store the key through `security -i`, which reads the command from its input, so the key is
/// never an argument another process could list.
fn keychain_write(key: &[u8; 32]) -> bool {
    use std::io::Write;
    let spawned = std::process::Command::new("/usr/bin/security")
        .arg("-i")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = spawned else { return false };
    let command = format!("add-generic-password -U -s {KEYCHAIN_SERVICE} -a {KEYCHAIN_ACCOUNT} -w {}\n", hex(key));
    let wrote = child.stdin.take().is_some_and(|mut input| input.write_all(command.as_bytes()).is_ok());
    let done = child.wait().is_ok_and(|status| status.success());
    // Believed only when it reads back: a locked or absent keychain must fall back to the file.
    wrote && done && keychain_read().as_ref() == Some(key)
}

/// Takes every secret's value out of text on its way to the model, the session, and the logs,
/// leaving the handle in its place. It looks for the value as it is and as base64 of it (the
/// forms a server echoes a credential back in); a caller that sent a derived form (the
/// `user:password` of Basic auth) adds that form with `also`.
#[derive(Clone, Default)]
pub struct SecretScrubber {
    /// What to look for and what to put there, longest first, so a value that contains another
    /// is replaced whole.
    needles: Vec<(String, String)>,
}

impl std::fmt::Debug for SecretScrubber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretScrubber({} forms)", self.needles.len())
    }
}

impl SecretScrubber {
    pub fn new(secrets: impl IntoIterator<Item = (Handle, Secret)>) -> SecretScrubber {
        let mut scrubber = SecretScrubber::default();
        for (handle, secret) in secrets {
            scrubber.also(&handle, secret.expose());
        }
        scrubber
    }

    /// Also look for `form`, which gives away the secret `handle` names, and for its base64.
    pub fn also(&mut self, handle: &Handle, form: &str) {
        if form.len() < MIN_SCRUBBED {
            return;
        }
        let replacement = format!("[REDACTED:{handle}]");
        let encoded =
            [STANDARD.encode(form), STANDARD_NO_PAD.encode(form), URL_SAFE.encode(form), URL_SAFE_NO_PAD.encode(form)];
        for needle in std::iter::once(form.to_string()).chain(encoded) {
            if !self.needles.iter().any(|(known, _)| *known == needle) {
                self.needles.push((needle, replacement.clone()));
            }
        }
        self.needles.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
    }

    pub fn scrub(&self, text: &str) -> String {
        let mut clean = text.to_string();
        for (needle, replacement) in &self.needles {
            if clean.contains(needle.as_str()) {
                clean = clean.replace(needle.as_str(), replacement);
            }
        }
        clean
    }
}

/// The scrubber of a NyPi home's store, kept as current as the store (decision D87): a process
/// that runs for days (the daemon, the board) scrubs a secret `nypi secrets set` added after it
/// began. It notes the file's modification time and size, and reloads the store only when the
/// file appears or either changes; while there is no file it asks for no key (nor the keychain).
pub struct LiveScrubber {
    path: PathBuf,
    home: PathBuf,
    held: std::sync::Mutex<(Option<FileSeen>, std::sync::Arc<SecretScrubber>)>,
}

/// A file's modification time and size, as last seen.
type FileSeen = (std::time::SystemTime, u64);

impl std::fmt::Debug for LiveScrubber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LiveScrubber({})", self.path.display())
    }
}

impl LiveScrubber {
    /// The scrubber of the store in the NyPi home `home` (`secrets.enc`).
    pub fn new(home: &Path) -> LiveScrubber {
        LiveScrubber { path: home.join("secrets.enc"), home: home.to_path_buf(), held: Default::default() }
    }

    /// One that never reloads: for a store opened elsewhere, and for tests.
    pub fn fixed(scrubber: SecretScrubber) -> LiveScrubber {
        LiveScrubber {
            path: PathBuf::new(),
            home: PathBuf::new(),
            held: std::sync::Mutex::new((None, std::sync::Arc::new(scrubber))),
        }
    }

    /// The scrubber as the store reads now.
    pub fn current(&self) -> std::sync::Arc<SecretScrubber> {
        let mut held = self.held.lock().unwrap();
        if self.path.as_os_str().is_empty() {
            return held.1.clone();
        }
        let seen = std::fs::metadata(&self.path)
            .ok()
            .map(|meta| (meta.modified().unwrap_or(std::time::UNIX_EPOCH), meta.len()));
        if seen != held.0 {
            // Gone: nothing is left to scrub. Changed: read it again; a store that cannot be
            // opened keeps what was known, and is not asked again until it changes once more.
            let fresh = match seen {
                None => Some(SecretScrubber::default()),
                Some(_) => SecretStore::open(&self.home).ok().map(|store| SecretScrubber::new(store.all())),
            };
            if let Some(fresh) = fresh {
                held.1 = std::sync::Arc::new(fresh);
            }
            held.0 = seen;
        }
        held.1.clone()
    }

    pub fn scrub(&self, text: &str) -> String {
        self.current().scrub(text)
    }
}

/// Whether `text` looks like it holds a raw secret rather than a handle: a vendor's key prefix,
/// a private key's header, a bearer token, or a long run of high-entropy characters. Used to
/// keep secrets out of the asset registry and the notes that name them.
pub fn looks_secret(text: &str) -> Option<&'static str> {
    const PREFIXES: [(&str, &str); 9] = [
        ("sk-", "an API key (sk-...)"),
        ("ghp_", "a GitHub token"),
        ("gho_", "a GitHub token"),
        ("github_pat_", "a GitHub token"),
        ("xoxb-", "a Slack token"),
        ("xoxp-", "a Slack token"),
        ("AKIA", "an AWS access key"),
        ("AIza", "a Google API key"),
        ("glpat-", "a GitLab token"),
    ];
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY") {
        return Some("a private key");
    }
    for word in text.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';' | '=' | ':')) {
        let token_like =
            |w: &str| w.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '='));
        for (prefix, what) in PREFIXES {
            if word.starts_with(prefix) && word.len() >= prefix.len() + 12 && token_like(word) {
                return Some(what);
            }
        }
        if word.len() >= 32 && token_like(word) && !word.contains("://") && entropy(word) >= 4.0 {
            return Some("a long random string, as a key or token is");
        }
    }
    let lower = text.to_lowercase();
    if let Some(at) = lower.find("bearer ") {
        let token = text[at + "bearer ".len()..].split_whitespace().next().unwrap_or_default();
        if token.len() >= 16 {
            return Some("a bearer token");
        }
    }
    None
}

/// Shannon entropy of `text`, in bits per character.
fn entropy(text: &str) -> f64 {
    let mut counts: BTreeMap<char, usize> = BTreeMap::new();
    for c in text.chars() {
        *counts.entry(c).or_default() += 1;
    }
    let total = text.chars().count() as f64;
    counts.values().map(|count| *count as f64 / total).map(|p| -p * p.log2()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SecretStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::with_master_key(dir.path().join("secrets.enc"), [7u8; 32]);
        (dir, store)
    }

    #[test]
    fn handles_are_read_strictly_and_written_one_way() {
        let handle = Handle::parse("cred://github/lucia-bot").unwrap();
        assert_eq!((handle.scope.as_str(), handle.name.as_str()), ("github", "lucia-bot"));
        assert_eq!(Handle::parse("github/lucia-bot").unwrap().to_string(), "cred://github/lucia-bot");
        for bad in ["cred://github", "cred:///x", "a/b/c", "git hub/x", "../etc/passwd", ""] {
            assert!(Handle::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn secrets_are_sealed_on_disk_and_open_only_with_their_key_and_name() {
        let (dir, store) = store();
        let handle = Handle::parse("cred://api/main").unwrap();
        assert!(store.get(&handle).unwrap().is_none());
        store.set(&handle, "tok-4f9a-very-secret").unwrap();
        store.set(&Handle::parse("cred://mail/smtp").unwrap(), "hunter2-password").unwrap();
        let on_disk = std::fs::read_to_string(dir.path().join("secrets.enc")).unwrap();
        assert!(!on_disk.contains("tok-4f9a") && !on_disk.contains("hunter2"), "values are sealed: {on_disk}");
        assert_eq!(store.get(&handle).unwrap().unwrap().expose(), "tok-4f9a-very-secret");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("secrets.enc")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // Listed by handle and time only; debug output shows no value.
        let listed = store.list().unwrap();
        assert_eq!(
            listed.iter().map(|(h, _)| h.to_string()).collect::<Vec<_>>(),
            ["cred://api/main", "cred://mail/smtp"]
        );
        assert!(!format!("{:?} {store:?}", store.get(&handle).unwrap()).contains("tok-4f9a"));

        // Another key does not open it, and neither does the entry under another name.
        let other = SecretStore::with_master_key(dir.path().join("secrets.enc"), [8u8; 32]);
        assert!(other.get(&handle).is_err());
        assert!(other.all().is_empty());
        let moved = on_disk.replace("\"api/main\"", "\"api/other\"");
        std::fs::write(dir.path().join("secrets.enc"), moved).unwrap();
        assert!(store.get(&Handle::parse("api/other").unwrap()).is_err(), "the handle is part of the seal");
        std::fs::write(dir.path().join("secrets.enc"), on_disk).unwrap();

        assert!(store.remove(&handle).unwrap() && !store.remove(&handle).unwrap());
        assert_eq!(store.all().len(), 1);
    }

    #[test]
    fn the_scrubber_takes_values_out_in_every_form_a_server_echoes() {
        let handle = Handle::parse("cred://api/main").unwrap();
        let mut scrubber = SecretScrubber::new([(handle.clone(), Secret::new("tok-4f9a-very-secret"))]);
        let encoded = STANDARD.encode("tok-4f9a-very-secret");
        let echoed = format!("Authorization: Bearer tok-4f9a-very-secret\nx-debug: {encoded}\nbody ok");
        let clean = scrubber.scrub(&echoed);
        assert!(!clean.contains("tok-4f9a") && !clean.contains(&encoded), "{clean}");
        assert_eq!(clean.matches("[REDACTED:cred://api/main]").count(), 2);
        // The form a Basic header carries is the caller's to add: base64 of `user:password`
        // need not contain base64 of the password (here the user name shifts the alignment).
        let basic = STANDARD.encode("root:tok-4f9a-very-secret");
        assert!(scrubber.scrub(&basic).contains(&basic));
        scrubber.also(&handle, "root:tok-4f9a-very-secret");
        assert_eq!(scrubber.scrub(&format!("Basic {basic}")), "Basic [REDACTED:cred://api/main]");
        // A value too short to be told from ordinary text is left alone.
        let short = SecretScrubber::new([(handle, Secret::new("abc"))]);
        assert_eq!(short.scrub("abc def"), "abc def");
    }

    #[test]
    fn raw_secrets_are_told_from_handles_and_ordinary_text() {
        assert!(looks_secret("sk-proj-9aF3kLmN0pQrStUv").is_some());
        assert!(looks_secret("-----BEGIN OPENSSH PRIVATE KEY-----\nb3Blbn...").is_some());
        assert!(looks_secret("Authorization: Bearer eyJhbGciOiJIUzI1NiJ9abc").is_some());
        assert!(looks_secret("token 9f8e7d6c5b4a39281706f5e4d3c2b1a0ZyXwVuTs").is_some());
        for fine in [
            "cred://github/lucia-bot",
            "lucia-bot@example.com",
            "https://example.com/a/very/long/path/that/is/not/a/secret",
            "renews every March",
            "srv-mini.local",
        ] {
            assert_eq!(looks_secret(fine), None, "{fine}");
        }
    }

    #[test]
    fn a_home_that_is_not_the_default_keeps_its_key_in_an_owner_only_file() {
        let home = tempfile::tempdir().unwrap();
        let first = master_key(home.path()).unwrap();
        assert_eq!(master_key(home.path()).unwrap(), first, "made once, then read");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.path().join("secrets.key")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let store = SecretStore::open(home.path()).unwrap();
        let handle = Handle::parse("a/b").unwrap();
        store.set(&handle, "value-123456").unwrap();
        assert_eq!(SecretStore::open(home.path()).unwrap().get(&handle).unwrap().unwrap().expose(), "value-123456");
    }

    #[test]
    fn the_live_scrubber_takes_in_a_secret_set_after_it_began() {
        let home = tempfile::tempdir().unwrap();
        let live = LiveScrubber::new(home.path());
        assert_eq!(live.scrub("token tok_live_9f8e7d6c5b"), "token tok_live_9f8e7d6c5b");
        assert!(!home.path().join("secrets.key").exists(), "no store yet: no key was made");
        let store = SecretStore::open(home.path()).unwrap();
        store.set(&Handle::parse("cred://shop/token").unwrap(), "tok_live_9f8e7d6c5b").unwrap();
        assert_eq!(live.scrub("token tok_live_9f8e7d6c5b"), "token [REDACTED:cred://shop/token]");
        // A second secret changes the file again, and is taken in too.
        store.set(&Handle::parse("cred://mail/password").unwrap(), "hunter2-but-longer").unwrap();
        let clean = live.scrub("tok_live_9f8e7d6c5b and hunter2-but-longer");
        assert!(!clean.contains("tok_live") && !clean.contains("hunter2"), "{clean}");
        // Unchanged, the store is not read again.
        assert!(std::sync::Arc::ptr_eq(&live.current(), &live.current()));
    }
}
