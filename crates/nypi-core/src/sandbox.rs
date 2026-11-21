//! The tool sandbox (decisions D62 and D76): every subprocess the agent starts (`bash`, command
//! tools, a schedule's command condition) runs where the operating system keeps it away from
//! what `Protected` names, and without the secrets NyPi's own process was started with. The
//! Inspector guard reads a command's text, which a shell can always spell another way (`cd
//! ~/.nypi && rm audit/*`, a script, a symlink); the kernel checks the file a call really
//! reaches, so the boundary holds whatever the command looks like.
//!
//! - macOS: `sandbox-exec` with a Seatbelt profile that allows everything except writing to
//!   what is protected, reading its secrets, and connecting to the sockets in NyPi's run folder,
//!   where a message counts as the user's (decision D64).
//! - Linux: `bwrap` (bubblewrap), when it is installed and the kernel lets an unprivileged user
//!   make a namespace: the whole system as it is, with what is protected mounted read-only and
//!   the secrets and the run folder masked. It protects what exists when the command starts; a
//!   `.nypi` folder made later is the guard's to refuse, so there the guard keeps reading the
//!   command's text as well.
//! - The environment, everywhere: variables that hold keys and tokens are taken out of the
//!   child's environment unless the owner's settings let one through (`sandbox.allowEnv`).
//!
//! The harness itself is not sandboxed: it is what writes those files. Where no sandbox is
//! available (other systems, or NyPi already running inside one), the guard's checks are all
//! there is, and the user is told.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

use crate::protected::Protected;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    /// Use the sandbox when the system has one.
    #[default]
    Auto,
    Off,
}

/// The `sandbox` setting: `"auto"` or `"off"` as before, or an object that also names the
/// secret-looking environment variables tool subprocesses may keep.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSettings {
    pub mode: SandboxMode,
    /// Names of environment variables that look like secrets (`GITHUB_TOKEN`, `X_API_KEY`) and
    /// are passed to tool subprocesses all the same.
    pub allow_env: Vec<String>,
}

impl<'de> Deserialize<'de> for SandboxSettings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", default)]
        struct Full {
            mode: SandboxMode,
            allow_env: Vec<String>,
        }
        impl Default for Full {
            fn default() -> Self {
                Full { mode: SandboxMode::Auto, allow_env: Vec::new() }
            }
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Mode(SandboxMode),
            Full(Full),
        }
        Ok(match Either::deserialize(deserializer)? {
            Either::Mode(mode) => SandboxSettings { mode, allow_env: Vec::new() },
            Either::Full(full) => SandboxSettings { mode: full.mode, allow_env: full.allow_env },
        })
    }
}

/// What the operating system does around a tool's subprocess.
#[derive(Clone, Default)]
enum Kind {
    #[default]
    None,
    /// macOS: the Seatbelt profile.
    Seatbelt(Arc<str>),
    /// Linux: bubblewrap's read-only mounts, the masks that must always lie on top of them, NyPi's
    /// home, and the user's home below which project `.nypi` folders are kept read-only.
    Bubblewrap {
        program: Arc<Path>,
        ro_args: Arc<[OsString]>,
        mask_args: Arc<[OsString]>,
        home: Arc<Path>,
        user_home: Option<Arc<Path>>,
    },
}

/// How to start a tool's subprocess. Cheap to clone; `Sandbox::default()` starts it directly,
/// still without the secrets in the environment.
#[derive(Clone, Default)]
pub struct Sandbox {
    kind: Kind,
    allow_env: Arc<[String]>,
}

/// A path as a Seatbelt string literal.
fn literal(path: &Path) -> String {
    format!("\"{}\"", path.display().to_string().replace('\\', "\\\\").replace('"', "\\\""))
}

/// A path as the start of a Seatbelt regex, every character taken literally.
fn regex_prefix(path: &Path) -> String {
    let mut escaped = String::new();
    for c in path.display().to_string().chars() {
        if "\\.^$*+?()[]{}|\"".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// The Seatbelt profile: allow everything, then deny writes to what is protected and reads of
/// its secrets. Paths must be real paths (no symlinks), which `Protected` guarantees.
pub fn profile(protected: &Protected) -> String {
    let mut write = vec![format!("(subpath {})", literal(&protected.home))];
    write.extend(protected.files.iter().map(|file| format!("(literal {})", literal(file))));
    write.extend(protected.folders.iter().map(|folder| format!("(subpath {})", literal(folder))));
    if let Some(user_home) = &protected.user_home {
        // A `.nypi` folder at any depth below the user's home, and everything in it, in any case
        // (a case-insensitive disk finds `.NYPI` when NyPi looks for `.nypi`).
        write.push(format!("(regex #\"^{}/(.*/)?\\.[nN][yY][pP][iI](/.*)?$\")", regex_prefix(user_home)));
    }
    let read: Vec<String> = protected.secrets.iter().map(|secret| format!("(subpath {})", literal(secret))).collect();
    // The sockets in the run folder take the user's word (the daemon's messages and approval
    // answers, decision D64): a tool that could connect would speak as the user.
    let sockets = format!("(remote unix-socket (subpath {}))", literal(&protected.home.join("run")));
    // A deny with no filter denies everything, so a list that came out empty is left out.
    let read = if read.is_empty() { String::new() } else { format!("(deny file-read*\n  {})\n", read.join("\n  ")) };
    format!(
        "(version 1)\n(allow default)\n(deny file-write*\n  {})\n{read}(deny network-outbound\n  {})\n",
        write.join("\n  "),
        sockets
    )
}

/// Bubblewrap's arguments for these protected paths: the system as it is (`--dev-bind / /`),
/// then, over it, what may not be written mounted read-only and what may not be read masked (an
/// empty folder over a folder, `/dev/null` over a file). Later mounts lie over earlier ones, so
/// the run folder and the key files inside NyPi's home are masked after the home is made
/// read-only. Only what exists can be mounted over; the rest is the guard's.
pub fn bubblewrap_args(protected: &Protected) -> Vec<OsString> {
    let (mut args, masks) = bubblewrap_parts(protected);
    args.extend(masks);
    args
}

fn bubblewrap_parts(protected: &Protected) -> (Vec<OsString>, Vec<OsString>) {
    let mut ro: Vec<OsString> = ["--dev-bind", "/", "/", "--die-with-parent"].iter().map(OsString::from).collect();
    let read_only = std::iter::once(&protected.home).chain(&protected.folders).chain(&protected.files);
    for path in read_only.filter(|path| path.exists()) {
        ro.extend(["--ro-bind".into(), path.clone().into_os_string(), path.clone().into_os_string()]);
    }
    let mut masks = Vec::new();
    let run = protected.home.join("run");
    for path in protected.secrets.iter().chain(std::iter::once(&run)) {
        if path.is_dir() {
            masks.extend(["--tmpfs".into(), path.clone().into_os_string()]);
        } else if path.exists() {
            masks.extend(["--ro-bind".into(), "/dev/null".into(), path.clone().into_os_string()]);
        }
    }
    (ro, masks)
}

/// The `.nypi` folders that exist at `folder`, above it, and in project folders just below it,
/// under the user's home: a project's own configuration, kept read-only for a subprocess that
/// works there.
fn nypi_folders(folder: &Path, user_home: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = folder
        .ancestors()
        .filter(|dir| user_home.is_none_or(|home| dir.starts_with(home)))
        .map(|dir| dir.join(".nypi"))
        .filter(|nypi| nypi.is_dir())
        .collect();
    if user_home.is_none_or(|home| folder.starts_with(home)) {
        let mut budget = 512usize;
        collect_child_nypi(folder, 4, &mut budget, &mut out);
    }
    out
}

fn collect_child_nypi(dir: &Path, depth: usize, budget: &mut usize, out: &mut Vec<PathBuf>) {
    if depth == 0 || *budget == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        let Ok(kind) = entry.file_type() else { continue };
        if !kind.is_dir() {
            continue;
        }
        *budget -= 1;
        let name = entry.file_name();
        let Some(text) = name.to_str() else { continue };
        let path = entry.path();
        if text.eq_ignore_ascii_case(".nypi") {
            if !out.contains(&path) {
                out.push(path);
            }
            continue;
        }
        if text.starts_with('.') || matches!(text, "target" | "node_modules" | "dist" | "vendor") {
            continue;
        }
        collect_child_nypi(&path, depth - 1, budget, out);
    }
}

/// Whether a variable's name marks a key or a token that a tool's subprocess has no business
/// with: the model providers' and clouds' own names, and the common shapes (`*_API_KEY`,
/// `*_SECRET_KEY`, `NYPI_*_TOKEN`). Ordinary developer variables (`PATH`, `HOME`, `SSH_AUTH_SOCK`,
/// `CARGO_HOME`, ...) match none of these and are left alone.
pub fn is_secret_env(name: &str) -> bool {
    const NAMED: [&str; 6] =
        ["AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "GITHUB_TOKEN", "GH_TOKEN", "TELEGRAM_BOT_TOKEN", "HF_TOKEN"];
    let name = name.to_ascii_uppercase();
    NAMED.contains(&name.as_str())
        || name.ends_with("_API_KEY")
        || name.ends_with("_SECRET_KEY")
        || (name.starts_with("NYPI_") && name.ends_with("_TOKEN"))
}

/// Take the secrets this process was started with out of `command`'s environment, except the
/// names in `allow`. What the caller sets on the command afterwards (a tool's own `env`) stays.
pub fn scrub_env(command: &mut std::process::Command, allow: &[String]) {
    for (name, _) in std::env::vars_os() {
        let Some(text) = name.to_str() else { continue };
        if is_secret_env(text) && !allow.iter().any(|allowed| allowed.eq_ignore_ascii_case(text)) {
            command.env_remove(&name);
        }
    }
}

/// Whether this process can start sandboxed children on macOS: a working `sandbox-exec`. It
/// fails when NyPi itself already runs inside a sandbox. Checked once per process.
fn seatbelt_available() -> Result<(), String> {
    static AVAILABLE: OnceLock<Result<(), String>> = OnceLock::new();
    AVAILABLE
        .get_or_init(|| {
            let probe = std::process::Command::new(SANDBOX_EXEC)
                .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
                .stdin(std::process::Stdio::null())
                .output();
            match probe {
                Ok(output) if output.status.success() => Ok(()),
                Ok(output) => Err(String::from_utf8_lossy(&output.stderr).trim().to_string()),
                Err(error) => Err(error.to_string()),
            }
        })
        .clone()
}

/// Bubblewrap on this system, when it is installed and the kernel lets this user make a
/// namespace (the probe binds the system read-only and runs `true`). Checked once per process.
fn bubblewrap_available() -> Result<PathBuf, String> {
    static AVAILABLE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    AVAILABLE
        .get_or_init(|| {
            let fixed = Path::new("/usr/bin/bwrap");
            let on_path = std::env::var_os("PATH")
                .into_iter()
                .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
                .map(|dir| dir.join("bwrap"))
                .find(|candidate| candidate.is_file());
            let program = if fixed.is_file() { Some(fixed.to_path_buf()) } else { on_path };
            let Some(program) = program else {
                return Err("bubblewrap (bwrap) is not installed, so tool programs run without a sandbox".into());
            };
            let truth = if Path::new("/bin/true").exists() { "/bin/true" } else { "/usr/bin/true" };
            let probe = std::process::Command::new(&program)
                .args(["--ro-bind", "/", "/", truth])
                .stdin(std::process::Stdio::null())
                .output();
            match probe {
                Ok(output) if output.status.success() => Ok(program),
                Ok(output) => Err(format!(
                    "bubblewrap cannot make a sandbox here (unprivileged user namespaces may be turned off): {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
                Err(error) => Err(format!("bubblewrap did not start: {error}")),
            }
        })
        .clone()
}

impl Sandbox {
    /// The sandbox for these protected paths, and, when one was wanted but cannot be had, why:
    /// the caller tells the user that only the guard's checks apply.
    pub fn detect(settings: &SandboxSettings, protected: &Protected) -> (Self, Option<String>) {
        let allow_env: Arc<[String]> = settings.allow_env.clone().into();
        let unsandboxed = Sandbox { kind: Kind::None, allow_env: allow_env.clone() };
        if settings.mode == SandboxMode::Off {
            return (unsandboxed, None);
        }
        if cfg!(target_os = "macos") {
            return match seatbelt_available() {
                Ok(()) => (Sandbox { kind: Kind::Seatbelt(profile(protected).into()), allow_env }, None),
                Err(why) => (unsandboxed, Some(why)),
            };
        }
        if cfg!(target_os = "linux") {
            return match bubblewrap_available() {
                Ok(program) => {
                    let (ro, masks) = bubblewrap_parts(protected);
                    let kind = Kind::Bubblewrap {
                        program: program.into(),
                        ro_args: ro.into(),
                        mask_args: masks.into(),
                        home: protected.home.as_path().into(),
                        user_home: protected.user_home.as_deref().map(Arc::from),
                    };
                    (Sandbox { kind, allow_env }, None)
                }
                Err(why) => (unsandboxed, Some(why)),
            };
        }
        // Elsewhere there was never one to lose; the documentation says so.
        (unsandboxed, None)
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.kind, Kind::None)
    }

    /// Whether the kernel judges every path a subprocess reaches, including one that does not
    /// exist yet (Seatbelt). Bubblewrap covers what existed when the command started, so with
    /// it the guard goes on reading a command's text too.
    pub fn judges_every_path(&self) -> bool {
        matches!(self.kind, Kind::Seatbelt(_))
    }

    /// The same sandbox for a subprocess that works in `folder`: where the sandbox mounts over
    /// what exists (Linux), that folder's `.nypi`, those above it, and those in projects below it
    /// are kept read-only too, always underneath the secret and run-folder masks.
    pub fn in_folder(&self, folder: &Path) -> Sandbox {
        let Kind::Bubblewrap { program, ro_args, mask_args, home, user_home } = &self.kind else {
            return self.clone();
        };
        let mut ro: Vec<OsString> = ro_args.to_vec();
        for nypi in nypi_folders(folder, user_home.as_deref()) {
            if nypi != home.as_ref() {
                ro.extend(["--ro-bind".into(), nypi.clone().into_os_string(), nypi.into_os_string()]);
            }
        }
        let kind = Kind::Bubblewrap {
            program: program.clone(),
            ro_args: ro.into(),
            mask_args: mask_args.clone(),
            home: home.clone(),
            user_home: user_home.clone(),
        };
        Sandbox { kind, allow_env: self.allow_env.clone() }
    }

    /// A command that runs `program`, sandboxed when the sandbox is active, and without the
    /// secrets in this process's environment either way. Arguments added to it are the
    /// program's; environment the caller sets on it afterwards is the caller's to give.
    pub fn command(&self, program: impl AsRef<OsStr>) -> std::process::Command {
        let mut command = match &self.kind {
            Kind::Seatbelt(profile) => {
                let mut command = std::process::Command::new(SANDBOX_EXEC);
                command.arg("-p").arg(&**profile).arg(program);
                command
            }
            Kind::Bubblewrap { program: bwrap, ro_args, mask_args, .. } => {
                let mut command = std::process::Command::new(&**bwrap);
                command.args(ro_args.iter()).args(mask_args.iter()).arg("--").arg(program);
                command
            }
            Kind::None => std::process::Command::new(program),
        };
        scrub_env(&mut command, &self.allow_env);
        command
    }

    /// The secret-looking variables the owner lets tool subprocesses keep.
    pub fn allow_env(&self) -> &[String] {
        &self.allow_env
    }

    /// What to tell the model about a sandboxed tool, so a refusal reads as policy, not a bug.
    pub fn note(&self) -> &'static str {
        match self.kind {
            Kind::Seatbelt(_) => {
                " Runs in a sandbox: writing inside NyPi's own folder (~/.nypi) or any .nypi folder, and reading key \
                 files such as ~/.env, fails with \"Operation not permitted\". That is the Inspector's boundary, not \
                 an error to work around. API keys and tokens from the environment are not passed to the command."
            }
            Kind::Bubblewrap { .. } => {
                " Runs in a sandbox: NyPi's own folder (~/.nypi) and .nypi folders are read-only (\"Read-only file \
                 system\"), and key files such as ~/.env read as empty. That is the Inspector's boundary, not an \
                 error to work around. API keys and tokens from the environment are not passed to the command."
            }
            Kind::None => " API keys and tokens from the environment are not passed to the command.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protected::real_path;

    #[test]
    fn the_profile_denies_writes_to_what_is_protected_and_reads_of_secrets() {
        let protected = Protected {
            home: "/Users/a.b/.nypi".into(),
            user_home: Some("/Users/a.b".into()),
            files: vec!["/Users/a.b/Library/LaunchAgents/dev.nypi.supervisor.plist".into()],
            folders: vec!["/Users/a.b/.agents/skills".into()],
            secrets: vec!["/Users/a.b/.env".into(), "/Users/a.b/.nypi/run".into()],
        };
        let text = profile(&protected);
        assert!(text.starts_with("(version 1)\n(allow default)\n"), "{text}");
        assert!(text.contains("(subpath \"/Users/a.b/.nypi\")"));
        assert!(text.contains("(literal \"/Users/a.b/Library/LaunchAgents/dev.nypi.supervisor.plist\")"));
        assert!(text.contains("(subpath \"/Users/a.b/.agents/skills\")"), "the user's skill folders: {text}");
        assert!(
            text.contains(r#"(regex #"^/Users/a\.b/(.*/)?\.[nN][yY][pP][iI](/.*)?$")"#),
            "the dot in the name is literal: {text}"
        );
        let reads = text.split("(deny file-read*").nth(1).unwrap();
        assert!(
            reads.contains("(subpath \"/Users/a.b/.env\")") && reads.contains("(subpath \"/Users/a.b/.nypi/run\")")
        );
        assert!(
            text.ends_with("(deny network-outbound\n  (remote unix-socket (subpath \"/Users/a.b/.nypi/run\")))\n"),
            "{text}"
        );
        let none = Protected { secrets: Vec::new(), ..protected };
        assert!(!profile(&none).contains("file-read"), "no read rule at all, rather than one that denies every read");
    }

    /// The real thing, where the system has it: a shell under the profile cannot reach a
    /// protected file by any spelling, and everything else still works.
    #[test]
    fn a_sandboxed_shell_cannot_reach_protected_files_by_any_spelling() {
        let user = tempfile::tempdir().unwrap();
        let user_home = real_path(user.path());
        let home = user_home.join(".nypi");
        std::fs::create_dir_all(home.join("audit")).unwrap();
        std::fs::create_dir_all(user_home.join("code/site/.nypi")).unwrap();
        std::fs::write(home.join("audit/log.jsonl"), "kept\n").unwrap();
        std::fs::write(home.join("auth.json"), "{\"key\":\"secret\"}").unwrap();
        std::fs::write(user_home.join(".env"), "TOKEN=secret\n").unwrap();
        // A socket where the daemon's would be, and one a tool may use.
        std::fs::create_dir_all(home.join("run")).unwrap();
        let _daemon = std::os::unix::net::UnixListener::bind(home.join("run/hub.sock")).unwrap();
        let _other = std::os::unix::net::UnixListener::bind(user_home.join("ok.sock")).unwrap();
        let secrets = vec![home.join("auth.json"), user_home.join(".env")];
        let protected = Protected::from_parts(&home, Some(user_home.clone()), Vec::new(), secrets);
        let (sandbox, problem) = Sandbox::detect(&SandboxSettings::default(), &protected);
        if !sandbox.is_active() {
            eprintln!("skipped: no sandbox here ({problem:?})");
            return;
        }
        let sandbox = sandbox.in_folder(&user_home);
        let run = |script: &str| {
            let output = sandbox
                .command("bash")
                .arg("-c")
                .arg(script)
                .current_dir(&user_home)
                .env("HOME", &user_home)
                .output()
                .unwrap();
            (output.status.success(), String::from_utf8_lossy(&output.stdout).to_string())
        };
        for attempt in [
            "rm -f ~/.nypi/audit/*.jsonl && test ! -e .nypi/audit/log.jsonl",
            "cd ~/.nypi && : > audit/log.jsonl",
            "echo x >> \"$HOME/.nypi/audit/log.jsonl\"",
            "ln -s ~/.nypi ./door && echo x > door/settings.json",
            "python3 -c \"open('.nypi/trust.json','w').write('{}')\" 2>/dev/null",
            "echo '{\"approval\":{\"mode\":\"off\"}}' > code/site/.nypi/settings.json",
            "mv ~/.nypi ~/.nypi-gone",
            "python3 -c \"import os, socket; socket.socket(socket.AF_UNIX).connect(os.path.expanduser('~/.nypi/run/hub.sock'))\" 2>/dev/null",
            "cd ~/.nypi/run && python3 -c \"import socket; socket.socket(socket.AF_UNIX).connect('hub.sock')\" 2>/dev/null",
        ] {
            let (ok, out) = run(attempt);
            assert!(!ok, "this must fail under the sandbox: {attempt}\n{out}");
            assert!(!out.contains("secret"), "a secret leaked from: {attempt}");
        }
        // Seatbelt denies opening a secret file; bubblewrap masks it with `/dev/null` so it reads
        // as empty. Either way, no byte of a secret comes back.
        for attempt in ["cat ~/.env", "cat .nypi/auth.json"] {
            let (ok, out) = run(attempt);
            assert!((!ok || out.is_empty()) && !out.contains("secret"), "a secret leaked from: {attempt}\n{out}");
        }
        assert_eq!(std::fs::read_to_string(home.join("audit/log.jsonl")).unwrap(), "kept\n");
        assert!(!home.join("settings.json").exists() && !user_home.join("code/site/.nypi/settings.json").exists());

        // Ordinary work is untouched, including reading what is not secret.
        let (ok, out) = run("echo hi > code/site/notes.txt && cat code/site/notes.txt && cat .nypi/audit/log.jsonl");
        assert!(ok && out == "hi\nkept\n", "{out}");
        let (ok, _) = run("python3 -c \"import socket; socket.socket(socket.AF_UNIX).connect('ok.sock')\"");
        assert!(ok, "sockets elsewhere still work");
    }

    #[test]
    fn off_and_unavailable_run_the_program_directly() {
        let protected = Protected::from_parts(Path::new("/nonexistent/.nypi"), None, Vec::new(), Vec::new());
        let (sandbox, problem) =
            Sandbox::detect(&SandboxSettings { mode: SandboxMode::Off, allow_env: Vec::new() }, &protected);
        assert!(!sandbox.is_active() && problem.is_none());
        assert_eq!(sandbox.command("bash").get_program(), "bash");
        assert!(!sandbox.note().contains("sandbox"));
    }

    #[test]
    fn the_setting_reads_as_a_word_or_as_an_object() {
        let word: SandboxSettings = serde_json::from_str("\"off\"").unwrap();
        assert_eq!(word, SandboxSettings { mode: SandboxMode::Off, allow_env: Vec::new() });
        let object: SandboxSettings = serde_json::from_str(r#"{ "allowEnv": ["GITHUB_TOKEN"] }"#).unwrap();
        assert_eq!(object, SandboxSettings { mode: SandboxMode::Auto, allow_env: vec!["GITHUB_TOKEN".into()] });
        let again: SandboxSettings = serde_json::from_value(serde_json::to_value(&object).unwrap()).unwrap();
        assert_eq!(again, object, "what is written reads back");
    }

    #[test]
    fn names_that_mark_secrets_are_told_from_ordinary_variables() {
        for secret in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "CUSTOM_API_KEY",
            "aws_secret_access_key",
            "AWS_SESSION_TOKEN",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "TELEGRAM_BOT_TOKEN",
            "STRIPE_SECRET_KEY",
            "NYPI_BOARD_TOKEN",
        ] {
            assert!(is_secret_env(secret), "{secret}");
        }
        for ordinary in [
            "PATH",
            "HOME",
            "USER",
            "SHELL",
            "LANG",
            "LC_ALL",
            "TERM",
            "TMPDIR",
            "XDG_CONFIG_HOME",
            "SSH_AUTH_SOCK",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "GOPATH",
            "NVM_DIR",
            "NYPI_HOME",
            "NYPI_PARENT_TASK",
            "EDITOR",
        ] {
            assert!(!is_secret_env(ordinary), "{ordinary}");
        }
    }

    /// Bubblewrap's arguments, which this machine may not be able to run: what is protected is
    /// mounted read-only over the system as it is, and the secrets and the run folder are masked
    /// after that, so they lie on top.
    #[test]
    fn bubblewrap_mounts_what_is_protected_read_only_and_masks_the_secrets() {
        let user = tempfile::tempdir().unwrap();
        let user_home = real_path(user.path());
        let home = user_home.join(".nypi");
        std::fs::create_dir_all(home.join("run")).unwrap();
        std::fs::create_dir_all(user_home.join(".agents/skills")).unwrap();
        std::fs::create_dir_all(user_home.join("code/site/.nypi")).unwrap();
        std::fs::write(home.join("auth.json"), "{}").unwrap();
        std::fs::write(user_home.join(".env"), "TOKEN=1").unwrap();
        let secrets = vec![home.join("auth.json"), user_home.join(".env"), home.join("never-made.key")];
        let mut protected = Protected::from_parts(&home, Some(user_home.clone()), Vec::new(), secrets);
        protected.folders = vec![user_home.join(".agents/skills")];
        let args: Vec<String> = bubblewrap_args(&protected).iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args[..4], ["--dev-bind", "/", "/", "--die-with-parent"]);
        let place = |needle: &[String]| args.windows(needle.len()).position(|w| w == needle);
        let text = |path: &Path| path.display().to_string();
        let ro = |path: &Path| place(&["--ro-bind".into(), text(path), text(path)]);
        let home_at = ro(&home).expect("NyPi's home is read-only");
        assert!(ro(&user_home.join(".agents/skills")).is_some(), "{args:?}");
        let run_at = place(&["--tmpfs".into(), text(&home.join("run"))]).expect("the run folder is masked");
        let auth_at = place(&["--ro-bind".into(), "/dev/null".into(), text(&home.join("auth.json"))]).expect("masked");
        assert!(place(&["--ro-bind".into(), "/dev/null".into(), text(&user_home.join(".env"))]).is_some());
        assert!(run_at > home_at && auth_at > home_at, "masks lie over the read-only home");
        assert!(!args.iter().any(|arg| arg.contains("never-made")), "only what exists is mounted over");
        // A project's own `.nypi`, and those above it below the user's home, for a command there.
        let found = nypi_folders(&user_home.join("code/site/src"), Some(&user_home));
        assert_eq!(found, [user_home.join("code/site/.nypi"), home.clone()]);
        assert!(nypi_folders(Path::new("/tmp"), Some(&user_home)).is_empty());

        // `in_folder` never mounts NyPi's home a second time over the secret and run-folder masks.
        let (ro, masks) = bubblewrap_parts(&protected);
        let scoped = Sandbox {
            kind: Kind::Bubblewrap {
                program: Path::new("/usr/bin/bwrap").into(),
                ro_args: ro.into(),
                mask_args: masks.into(),
                home: protected.home.as_path().into(),
                user_home: Some(user_home.as_path().into()),
            },
            allow_env: Arc::from([]),
        }
        .in_folder(&user_home);
        let cmd_args: Vec<String> = scoped.command("sh").get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        let last_ro = cmd_args.iter().rposition(|a| a == "--ro-bind" || a == "--tmpfs").unwrap();
        assert_eq!(&cmd_args[last_ro..last_ro + 2], ["--tmpfs", &text(&home.join("run"))], "{cmd_args:?}");
        assert_eq!(cmd_args.windows(3).filter(|w| *w == ["--ro-bind", &text(&home), &text(&home)]).count(), 1);
    }

    #[test]
    fn a_command_is_started_without_the_secrets_of_this_process() {
        // SAFETY: the variables are this test's own names; nothing else reads them.
        unsafe {
            std::env::set_var("NYPI_SANDBOX_TEST_API_KEY", "sk-test-1");
            std::env::set_var("NYPI_SANDBOX_KEPT_API_KEY", "sk-test-2");
            std::env::set_var("NYPI_SANDBOX_TEST_PLAIN", "plain");
        }
        let protected = Protected::from_parts(Path::new("/nonexistent/.nypi"), None, Vec::new(), Vec::new());
        let settings = SandboxSettings { mode: SandboxMode::Off, allow_env: vec!["NYPI_SANDBOX_KEPT_API_KEY".into()] };
        let (sandbox, _) = Sandbox::detect(&settings, &protected);
        let output = sandbox
            .command("sh")
            .arg("-c")
            .arg("echo \"[$NYPI_SANDBOX_TEST_API_KEY][$NYPI_SANDBOX_KEPT_API_KEY][$NYPI_SANDBOX_TEST_PLAIN][$GIVEN_API_KEY]\"")
            .env("GIVEN_API_KEY", "given")
            .output()
            .unwrap();
        // The secret is gone, the allowed one and the ordinary one stay, and what the caller
        // sets itself is the caller's to give.
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "[][sk-test-2][plain][given]");
    }
}
