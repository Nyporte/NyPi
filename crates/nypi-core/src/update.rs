//! Releases, and how a machine trusts one (decision D98). Every release of the public repository
//! carries `manifest.json` (the version, its tag, and one archive per target with its size and
//! SHA-256) and `manifest.json.sig`, an Ed25519 signature over the manifest's exact bytes. A
//! machine believes a manifest only when the signature is made by the release key built into the
//! program (`release_public_key`), and an archive only when its size and SHA-256 are the signed
//! ones. Downloading, unpacking, and putting a binary in place are the program's (`nypi-cli`).

use std::collections::BTreeMap;

use base64::Engine;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};

/// The release signing key's public half (base64, 32 bytes) of the public repository, pasted
/// here by its maintainer so that a build from source checks official releases with no
/// environment of its own (decision D99).
pub const DEFAULT_RELEASE_PUBLIC_KEY: Option<&str> = Some("fXOwgtpclR1H86OQ4bLqNsAkcvyuYhFupE2WNrKl0W4=");

/// The release key this program checks releases with: `NYPI_RELEASE_PUBKEY=<base64>` at build
/// time when set (a release pipeline, or a fork), else `DEFAULT_RELEASE_PUBLIC_KEY`. Without
/// either, no update is taken in the background, and by hand only with `--allow-unsigned`.
pub fn release_public_key() -> Option<&'static str> {
    option_env!("NYPI_RELEASE_PUBKEY").filter(|key| !key.trim().is_empty()).or(DEFAULT_RELEASE_PUBLIC_KEY)
}

/// The public repository releases come from, as `Cargo.toml` names it; a fork changes that line,
/// or `update.repo` in `settings.json`.
pub fn default_repo() -> String {
    let url = env!("CARGO_PKG_REPOSITORY");
    url.trim_end_matches('/').trim_end_matches(".git").trim_start_matches("https://github.com/").to_string()
}

/// One archive of a release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    pub file: String,
    pub sha256: String,
    pub size: u64,
}

/// `manifest.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub version: String,
    pub tag: String,
    #[serde(default)]
    pub published_at: String,
    #[serde(default)]
    pub notes_url: String,
    /// By target triple, such as `aarch64-apple-darwin`.
    pub assets: BTreeMap<String, Asset>,
}

fn decode(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(text.trim()).ok()
}

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The manifest in `bytes`, when `signature` (base64) is the release key's (`public_key`,
/// base64) over exactly those bytes.
pub fn verify(bytes: &[u8], signature: &str, public_key: &str) -> Result<Manifest, String> {
    let key = decode(public_key).filter(|key| key.len() == 32).ok_or("the release key is not an Ed25519 key")?;
    let signature = decode(signature).ok_or("the signature is not base64")?;
    UnparsedPublicKey::new(&ED25519, &key)
        .verify(bytes, &signature)
        .map_err(|_| "the release manifest is not signed by the release key".to_string())?;
    read_unsigned(bytes)
}

/// The manifest in `bytes`, its signature not looked at (a development build, by hand only).
pub fn read_unsigned(bytes: &[u8]) -> Result<Manifest, String> {
    serde_json::from_slice(bytes).map_err(|error| format!("the release manifest does not read: {error}"))
}

/// A release signing key: the private half as PKCS#8 (kept by the maintainer, never committed)
/// and the public half as base64 (built into the program).
pub struct Keypair {
    pub private_pkcs8: Vec<u8>,
    pub public: String,
}

pub fn generate() -> Result<Keypair, String> {
    let random = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&random).map_err(|_| "no random bytes".to_string())?;
    let public = public_of(pkcs8.as_ref())?;
    Ok(Keypair { private_pkcs8: pkcs8.as_ref().to_vec(), public })
}

/// The public half of a PKCS#8 private key, base64.
pub fn public_of(private_pkcs8: &[u8]) -> Result<String, String> {
    let pair = Ed25519KeyPair::from_pkcs8(private_pkcs8).map_err(|_| "not an Ed25519 private key".to_string())?;
    Ok(encode(pair.public_key().as_ref()))
}

/// The signature of `bytes` (base64) with the private key.
pub fn sign(private_pkcs8: &[u8], bytes: &[u8]) -> Result<String, String> {
    let pair = Ed25519KeyPair::from_pkcs8(private_pkcs8).map_err(|_| "not an Ed25519 private key".to_string())?;
    Ok(encode(pair.sign(bytes).as_ref()))
}

/// The private key from its file or variable: base64 of the PKCS#8 bytes, or the raw bytes.
pub fn read_private(text_or_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let text = String::from_utf8_lossy(text_or_bytes);
    let bytes = decode(&text).unwrap_or_else(|| text_or_bytes.to_vec());
    public_of(&bytes)?;
    Ok(bytes)
}

/// `major.minor.patch`, with a pre-release (`-rc.1`) that sorts before the release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

impl Version {
    /// `v0.4.1`, `0.4.1`, `0.5.0-rc.1`; build metadata (`+…`) is ignored.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim().trim_start_matches('v');
        let text = text.split('+').next()?;
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) if !pre.is_empty() => (core, Some(pre.to_string())),
            Some(_) => return None,
            None => (text, None),
        };
        let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
        let (major, minor, patch) = (parts.next()??, parts.next()??, parts.next()??);
        if parts.next().is_some() {
            return None;
        }
        Some(Version { major, minor, patch, pre })
    }

    /// This program's own version.
    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version { major: 0, minor: 0, patch: 0, pre: None })
    }

    pub fn newer_than(&self, other: &Version) -> bool {
        self.cmp_to(other) == std::cmp::Ordering::Greater
    }

    fn cmp_to(&self, other: &Version) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch)).then_with(|| {
            match (&self.pre, &other.pre) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(_), None) => std::cmp::Ordering::Less,
                (Some(a), Some(b)) => pre_release_order(a, b),
            }
        })
    }
}

/// SemVer 2.0.0 §11: pre-release identifiers compared dot by dot, numbers as numbers and below
/// words; when every shared one is equal, the one with more identifiers is the later
/// (`rc.10` after `rc.2`, `rc.1` after `rc`).
fn pre_release_order(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let number = |part: &str| {
        (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())).then(|| part.parse::<u64>().ok()).flatten()
    };
    let (left, right): (Vec<&str>, Vec<&str>) = (a.split('.').collect(), b.split('.').collect());
    for (x, y) in left.iter().zip(&right) {
        let order = match (number(x), number(y)) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => x.cmp(y),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

/// Where releases are downloaded from: the public repository's release URLs, which need no API,
/// token, or rate limit; or, in tests, a server on loopback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    base: String,
}

impl Source {
    /// `https://github.com/<owner>/<repo>/releases`.
    pub fn github(repo: &str) -> Result<Source, String> {
        let plain = |part: &str| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        match repo.trim().split_once('/') {
            Some((owner, name)) if plain(owner) && plain(name) => {
                Ok(Source { base: format!("https://github.com/{owner}/{name}/releases") })
            }
            _ => Err(format!("update.repo reads <owner>/<repo>, not {repo:?}")),
        }
    }

    /// A release server on loopback, for tests: plain HTTP is taken only there.
    pub fn loopback(base: &str) -> Result<Source, String> {
        let base = base.trim_end_matches('/');
        let ok =
            ["http://127.0.0.1:", "http://[::1]:", "http://localhost:"].iter().any(|start| base.starts_with(start));
        if !ok {
            return Err(format!("{base} is not a loopback address"));
        }
        Ok(Source { base: base.to_string() })
    }

    /// The manifest of the latest release, or of `tag`; and its signature.
    pub fn manifest(&self, tag: Option<&str>) -> (String, String) {
        let url = match tag {
            None => format!("{}/latest/download/manifest.json", self.base),
            Some(tag) => format!("{}/download/{tag}/manifest.json", self.base),
        };
        (url.clone(), format!("{url}.sig"))
    }

    pub fn asset(&self, tag: &str, file: &str) -> String {
        format!("{}/download/{tag}/{file}", self.base)
    }

    /// The release's page, for the user.
    pub fn page(&self, tag: &str) -> String {
        format!("{}/tag/{tag}", self.base)
    }

    pub fn is_https(&self) -> bool {
        self.base.starts_with("https://")
    }
}

/// The tag a version is released under.
pub fn tag_of(version: &str) -> String {
    let version = version.trim();
    if version.starts_with('v') { version.to_string() } else { format!("v{version}") }
}

/// Whether `now` (local time, minutes after midnight) is within `window` (`"02:00-05:00"`, which
/// may run past midnight). No window: any time.
pub fn in_window(window: Option<&str>, now_minutes: u32) -> bool {
    let Some(window) = window.map(str::trim).filter(|window| !window.is_empty()) else { return true };
    let minutes = |text: &str| -> Option<u32> {
        let (hours, minutes) = text.trim().split_once(':')?;
        let (hours, minutes): (u32, u32) = (hours.parse().ok()?, minutes.parse().ok()?);
        (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
    };
    let Some((start, end)) = window.split_once('-').and_then(|(a, b)| Some((minutes(a)?, minutes(b)?))) else {
        return false;
    };
    if start <= end { (start..end).contains(&now_minutes) } else { now_minutes >= start || now_minutes < end }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(version: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": version, "tag": tag_of(version), "publishedAt": "2026-11-20T00:00:00Z", "notesUrl": "",
            "assets": { "aarch64-apple-darwin": { "file": "nypi-0.4.1-aarch64-apple-darwin.tar.gz", "sha256": "ab", "size": 3 } },
        }))
        .unwrap()
    }

    /// keygen, sign, verify; a changed byte or another key fails.
    #[test]
    fn a_signed_manifest_verifies_with_its_key_only() {
        let pair = generate().unwrap();
        let bytes = manifest("0.4.1");
        let signature = sign(&pair.private_pkcs8, &bytes).unwrap();
        let read = verify(&bytes, &signature, &pair.public).unwrap();
        assert_eq!((read.version.as_str(), read.tag.as_str()), ("0.4.1", "v0.4.1"));
        let mut changed = bytes.clone();
        changed[5] ^= 1;
        assert!(verify(&changed, &signature, &pair.public).is_err(), "a changed byte");
        let other = generate().unwrap();
        assert!(verify(&bytes, &signature, &other.public).unwrap_err().contains("not signed by the release key"));
        assert!(verify(&bytes, "not base64!", &pair.public).is_err());
        let text = base64::engine::general_purpose::STANDARD.encode(&pair.private_pkcs8);
        assert_eq!(read_private(text.as_bytes()).unwrap(), pair.private_pkcs8, "the key as base64 in a file");
    }

    #[test]
    fn versions_compare_as_releases_do() {
        let v = |text: &str| Version::parse(text).unwrap();
        assert!(v("v0.4.1").newer_than(&v("0.4.0")));
        assert!(v("0.10.0").newer_than(&v("0.9.9")));
        assert!(v("0.5.0").newer_than(&v("0.5.0-rc.1")), "a release after its candidate");
        assert!(v("0.5.0-rc.10").newer_than(&v("0.5.0-rc.2")), "pre-release numbers as numbers");
        assert!(v("0.5.0-rc.1").newer_than(&v("0.5.0-rc")), "more identifiers after equal ones");
        assert!(v("0.5.0-beta").newer_than(&v("0.5.0-alpha.9")));
        assert!(v("0.5.0-alpha").newer_than(&v("0.5.0-1")), "a word after a number");
        assert!(!v("0.5.0-rc.2").newer_than(&v("0.5.0-rc.2")));
        assert!(!v("0.4.0").newer_than(&v("0.4.0")));
        assert!(Version::parse("0.4").is_none() && Version::parse("x.y.z").is_none());
        assert_eq!(v("1.2.3-rc.1+build").to_string(), "1.2.3-rc.1");
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn releases_come_from_https_or_loopback_only() {
        let github = Source::github("alice/nypi").unwrap();
        assert!(github.is_https());
        assert_eq!(github.manifest(None).0, "https://github.com/alice/nypi/releases/latest/download/manifest.json");
        assert_eq!(
            github.manifest(Some("v0.4.1")).1,
            "https://github.com/alice/nypi/releases/download/v0.4.1/manifest.json.sig"
        );
        assert_eq!(
            github.asset("v0.4.1", "a.tar.gz"),
            "https://github.com/alice/nypi/releases/download/v0.4.1/a.tar.gz"
        );
        assert!(Source::github("alice").is_err() && Source::github("a/b/c").is_err());
        assert!(Source::loopback("http://127.0.0.1:8080").is_ok());
        assert!(Source::loopback("http://example.com").is_err(), "plain HTTP elsewhere");
        let repo = default_repo();
        assert!(Source::github(&repo).is_ok(), "the repository in Cargo.toml: {repo}");
    }

    #[test]
    fn the_window_may_run_past_midnight() {
        let at = |h: u32, m: u32| h * 60 + m;
        assert!(in_window(None, at(13, 0)));
        assert!(in_window(Some("02:00-05:00"), at(3, 30)));
        assert!(!in_window(Some("02:00-05:00"), at(5, 0)));
        assert!(in_window(Some("23:00-01:00"), at(0, 30)) && in_window(Some("23:00-01:00"), at(23, 5)));
        assert!(!in_window(Some("23:00-01:00"), at(12, 0)));
        assert!(!in_window(Some("soon"), at(3, 0)), "a window that does not read is never open");
    }
}
