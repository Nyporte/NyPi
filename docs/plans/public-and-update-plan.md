# NyPi Plan: Public Repository Readiness (D97) and Verified Self-Update (D98)

This plan builds on `D96` (`97f66339d`). It prepares NyPi to be published as a clean public open-source repository
and adds a verified, rollback-safe update mechanism across the hub, nodes, and relay so every machine updates from
that public repository's GitHub Releases.

Execute the two parts in order as two clean, tested commits: `D97` and `D98`. Every commit must pass:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace` (no network calls: update flows run against a local loopback mock HTTP server)
- `./scripts/build-board.sh && cargo build`

Update `AGENTS.md`, `docs/architecture.md` (body and decision log), `docs/devices.md`, `docs/board.md`,
`docs/usage.md`, `docs/roadmap.md`, and `docs/overview.md` in the same commits. After `D98` lands, archive
`docs/public-and-update-plan.md` into `docs/plans/`.

Before starting, confirm two values supplied by the user in the task prompt (or keep clearly marked placeholders if
omitted):

1. `REPO` — the public GitHub repository (`<owner>/<repo>`, e.g. `alice/nypi`).
2. `COPYRIGHT_HOLDER` — the author name/handle to add beside upstream Pi's copyright in `LICENSE`.

---

## Part 1 (Decision D97): Public Repository Hygiene and Optional Entry

Traces to Principles A4 (traceability) and A5 (priority response): a public build must carry no personal domain,
endpoint, or credential, while still letting a user save their own entry once in `~/.nypi/settings.json` and keep
it across every future upgrade.

### 1.1 Make `entry` optional (LAN-first out of the box, no hardcoded personal domain)

In `crates/nypi-core/src/config.rs`:

- Replace the hardcoded `DEFAULT_ENTRY` string with an optional compile-time default:
  ```rust
  /// Optional compile-time entry (`NYPI_DEFAULT_ENTRY=host:port`), unset in public builds.
  pub const DEFAULT_ENTRY: Option<&str> = option_env!("NYPI_DEFAULT_ENTRY");
  ```
- Change `Settings::entry(&self) -> Option<String>`:
  1. `NYPI_ENTRY` environment variable when set and non-empty;
  2. `self.entry` from `~/.nypi/settings.json` when set and non-empty;
  3. `DEFAULT_ENTRY` when compiled in;
  4. otherwise `None`.

Handle `entry == None` cleanly everywhere (zero error spam, LAN-only mode works out of the box):

- `nypi hub up` (`crates/nypi-cli/src/machine.rs`):
  - Without `--entry` (and no saved `entry`): succeeds cleanly, listens on `hub.listen` (`7681`) and announces via
    mDNS (`_nypi._tcp`). Prints the hub key fingerprint, explains that machines on the same local network can join
    with `nypi node up`, and prints the one-line command to add a public entry later:
    `nypi hub up --entry <your-domain:443>`.
  - With `--entry <host:port>`: persists `entry` to `~/.nypi/settings.json` (via `keep_entry`, surviving every
    future binary update), checks whether another hub is already registered there, and prints the `nypi relay up`
    instructions just as in D94.
  - Running `nypi hub up --entry <host:port>` on an existing hub updates `settings.json`'s `entry` and keeps the hub
    running so the supervisor/daemon picks up the new entry without re-creating keys.
- `nypi node up` (`crates/nypi-cli/src/machine.rs` and `crates/nypi-hub/src/reach.rs`):
  - When `entry` is `None`: discovers the hub on the local network via mDNS (`_nypi._tcp`). If no hub answers on
    the local network, returns a clear actionable error telling the user to run on the same network as the hub or
    pass `--entry <host:port>`.
  - When `--entry` is passed: persists it to `~/.nypi/settings.json` and uses direct/mDNS first, falling back to the
    entry.
- `nypi hub run` (`crates/nypi-hub/src/daemon/entry.rs`):
  - When `entry` is `None`: does not start a relay control connection loop (no reconnect warnings).
- `nypi devices invite`:
  - Prints the ready-to-paste `nypi node up --code <code>` command, appending `--entry <entry>` automatically when
    the hub has an `entry` configured.
- `nypi status` and `nypi hub here`:
  - Show `entry: none (local network only)` when `None`, and work over direct/mDNS links without requiring an entry.

### 1.2 Scrub all personal domains, custom gateway names, and examples

Run a full repository scan (`git grep`) and replace every personal reference across code, tests, examples, and docs:

- Replace any personal domain in unit tests (`crates/nypi-hub/src/ws.rs`), comments (`crates/nypi-core/src/config.rs`),
  and docs (`docs/architecture.md`, `docs/board.md`, `docs/devices.md`, `docs/usage.md`, `docs/overview.md`) with
  RFC 2606 example domains (`nypi.example.com`, `api.example.com`).
- Replace personal gateway provider identifiers in test fixtures (`crates/nypi-ai/src/overflow.rs`,
  `crates/nypi-cli/src/board/topology.rs`) and docs (`docs/usage.md`, `crates/nypi-core/src/config.rs`) with neutral
  placeholders (`gateway`, `GATEWAY_API_KEY`).
- Replace the personal gateway's example models file with `docs/examples/custom-gateway-models.json` using
  `https://api.example.com/v1` and `GATEWAY_API_KEY`.
- Scrub personal domains from `docs/v4-plan.md` and move it to `docs/plans/v4-plan.md` (since D93–D96 are complete).
- Verify zero personal IPs, hostnames, home paths, emails, or real credentials exist in any tracked file.

### 1.3 Open-source metadata, documentation, and ignore rules

- `Cargo.toml`:
  - Set `[workspace.package]` `version = "0.4.0"`, `repository = "https://github.com/<REPO>"`,
    `homepage = "https://github.com/<REPO>"`, `description`, and `readme = "README.md"`.
- `LICENSE`:
  - Keep `Copyright (c) 2025 Mario Zechner` (MIT attribution to upstream Pi) and add
    `Copyright (c) 2026 <COPYRIGHT_HOLDER>`.
- `README.md`:
  - Rewrite to accurately reflect NyPi as it is today:
    - What NyPi is (single-binary compiled Rust personal-agent framework with a three-plane architecture: Chat,
      Principal, and worker task pool).
    - Key capabilities: durable task ledger & watchdog, Markdown vault & episodic memory, goals & schedules,
      specialists & MCP tools, OS sandbox & secret store, embedded WebAssembly board, Telegram & webhook channels,
      and multi-device Hub / Node / Relay networking (direct-first with mDNS, end-to-end pinned TLS 1.3 through
      Cloudflare/reverse-proxy WebSocket relay, seamless hub handover `nypi hub here`).
    - Installation (one-line `scripts/install.sh` from GitHub Releases + building from source with
      `./scripts/build-board.sh && cargo build --release`).
    - Quick start (single machine, LAN hub + node, optional VPS relay behind Cloudflare/Nginx, self-update).
    - Crate layout, links to `docs/`, clear credit to upstream Pi, and MIT license.
- `SECURITY.md`:
  - Short security policy: how to report vulnerabilities privately, and a concise map of NyPi's enforced boundaries
    (`Protected`, `SecretScrubber`, `exec-only` vs `personal`, inner pinned TLS 1.3, signed releases).
- `.gitignore`:
  - Add `*.pem`, `*.key`, `nypi-release.key`, `*.bak*`, `/dist/` so signing keys or local backups are never
    committed accidentally.

### 1.4 Tests for Part 1

- `Settings::entry()` returns `None` by default (when neither `NYPI_ENTRY`, `settings.json`, nor
  `NYPI_DEFAULT_ENTRY` is set), and respects `NYPI_ENTRY` > `settings.json` > compile-time default.
- `nypi hub up` without `--entry` succeeds and writes `instance.json`; `daemon/entry.rs` stays quiet when `entry`
  is `None`.
- `nypi node up` without `--entry` discovers a loopback/LAN hub and returns a clear hint when none is found.
- `nypi devices invite` includes `--entry` in its printed hint only when an entry is set.

---

## Part 2 (Decision D98): Verified Self-Update, CI/Release Pipeline, and Safe Rollout

Traces to Principles A3 (alertability), A4 (traceability), A5 (priority response), and B6 (the action chain never
breaks silently): every machine can update safely from the public GitHub repository's releases, refuses unverified
or broken binaries before touching the running installation, and rolls back automatically if a new binary crashes on
startup.

### 2.1 Trust model and invariants

- **Signed release manifests**: every GitHub Release includes:
  - `nypi-<version>-<target>.tar.gz` (single `nypi` binary with embedded WebAssembly board)
  - `SHA256SUMS`
  - `manifest.json` (`version`, `tag`, `publishedAt`, `notesUrl`, `assets` map keyed by target triple with `file`,
    `sha256`, `size`)
  - `manifest.json.sig` (base64 Ed25519 signature over the exact bytes of `manifest.json`, made with `ring`)
- **Pinned public key**: the Ed25519 release public key is embedded in `crates/nypi-core/src/update.rs` (one
  constant, overridable in tests only). `nypi` verifies `manifest.json.sig` first, then verifies the downloaded
  archive's exact byte size and SHA-256 against the signed manifest.
- **No API rate limits and no tokens needed**: fetch directly from public release download URLs:
  - latest: `https://github.com/<repo>/releases/latest/download/manifest.json` (and `.sig`)
  - specific version: `https://github.com/<repo>/releases/download/<tag>/manifest.json` (and `.sig`)
  - binary archive: `https://github.com/<repo>/releases/download/<tag>/<file>`
  - Only HTTPS is allowed in production (loopback `http://127.0.0.1:...` allowed only in tests via injected base
    URL / `NYPI_UPDATE_BASE_URL` under `#[cfg(test)]` or test helper).
- **The agent cannot update or alter the binary**:
  - Add `nypi update` to the Inspector guard blocklist in `crates/nypi-core/src/inspector.rs` (beside `nypi hub`,
    `nypi node`, `nypi relay`, `nypi devices`).
  - `~/.nypi/bin/` remains inside `Protected::home` (read-only to tools and the sandbox).

### 2.2 Build metadata and fleet version visibility

- Add `crates/nypi-cli/build.rs` setting:
  - `NYPI_TARGET` (`std::env::var("TARGET")`)
  - `NYPI_GIT_SHA` (short `git rev-parse --short HEAD`, falling back to `"unknown"` when built outside git)
- `nypi --version` prints: `nypi <version> (<target> <sha>)`.
- Include the machine's `nypi` version in node `Hello` / `NodeStatus` (`nypi-node-proto`, optional `#[serde(default)]`
  field for wire compatibility) and display each machine's version in `nypi status`, `nypi devices`, and the board.

### 2.3 Update settings (`UpdateSettings` in `crates/nypi-core/src/config.rs`)

Add `update: UpdateSettings` to `Settings`:

```json
{
  "update": {
    "repo": "<owner>/<repo>",
    "mode": "notify",
    "checkIntervalHours": 6,
    "autoApplyWindow": null,
    "autoRelay": false
  }
}
```

- `repo`: defaults to `<owner>/<repo>` derived from `CARGO_PKG_REPOSITORY` so forks update from their own repo by
  changing one line in `Cargo.toml` or `settings.json`.
- `mode`:
  - `"off"`: disable background checks (`nypi update` still works manually).
  - `"notify"` (default): supervisor checks every `checkIntervalHours` (with per-machine jitter and `ETag` caching
    in `~/.nypi/run/update-state.json`). When a newer verified release exists, raises one deduplicated notice per
    version through the outbox (`NyPi vX.Y.Z is available — run nypi update`).
  - `"auto"`: supervisor downloads, verifies, pre-flight checks, swaps, and restarts automatically **only when the
    machine is idle** (no active Principal activation, no running task in the ledger, no in-flight node call, and
    within `autoApplyWindow` such as `"02:00-05:00"` if configured).
- **Relay safety rule**: when `settings.relay.enabled` is true on this machine, `"auto"` downgrades to `"notify"`
  unless `update.autoRelay` is explicitly `true`, so a VPS relay never silently restarts mid-rollout.

### 2.4 CLI commands and atomic swap (`crates/nypi-cli/src/update.rs`)

Add workspace dependencies `flate2 = { version = "1", default-features = false, features = ["rust_backend"] }` and
`tar = "0.4"` (pure Rust, small, no C toolchain needed).

Commands:

| Command | Where | What it does |
|---|---|---|
| `nypi update --check` | any machine | Fetch and verify `manifest.json` + `.sig`, compare SemVer against current version, print status and release URL. |
| `nypi update [--version vX.Y.Z] [--yes]` | this machine | Download, verify Ed25519 signature and SHA-256, run pre-flight `--version`, back up `~/.nypi/bin/nypi` to `nypi.prev`, write `update-pending.json`, atomically replace `~/.nypi/bin/nypi`, and restart the supervisor cleanly. |
| `nypi update rollback` | this machine | Atomically restore `~/.nypi/bin/nypi.prev` -> `~/.nypi/bin/nypi`, clear `update-pending.json`, and restart the supervisor. |
| `nypi update --node <name> [--version vX.Y.Z]` | hub or personal node | Ask the named connected node to run its own verified `update` and report the outcome. |
| `nypi update --all [--version vX.Y.Z] [--include-relay]` | hub or personal node | Rolling fleet update: nodes one by one (waiting for each to reconnect healthy on the new version; skipping machines running the relay unless `--include-relay`), then the hub last after pausing new work cleanly. Stops at once if any node fails or rolls back. |
| `nypi update keygen [--out <path>]` | maintainer machine | Generate an Ed25519 release signing keypair (`0o600` private key file + base64 public key printed for embedding). |
| `nypi update sign <manifest.json> [--key <path>]` | maintainer / CI | Sign `manifest.json` using `--key` or `NYPI_RELEASE_SIGNING_KEY` env var and write `<manifest.json>.sig`. |

Single-machine apply pipeline (must never leave a partial binary on disk):

1. Fetch `manifest.json` and `manifest.json.sig`.
2. Verify Ed25519 signature against the pinned public key. (A manual interactive `--allow-unsigned` flag is accepted
   only when the embedded public key is unset in a dev build, printing a clear warning; background/supervisor updates
   always require a valid signature.)
3. Parse SemVer (`major.minor.patch`, optional pre-release): if not newer than current version and `--version` was
   not explicitly passed, report already up to date and exit `0`.
4. Locate asset for `NYPI_TARGET`; download to `~/.nypi/bin/.staging/<uuid>.tar.gz` while streaming SHA-256 and byte
   count; reject on size or hash mismatch.
5. Extract only the single `nypi` executable entry (reject path traversal or symlinks inside the tarball) to
   `~/.nypi/bin/nypi.new` with mode `0o755`.
6. **Pre-flight smoke test**: execute `~/.nypi/bin/nypi.new --version` with a 5-second timeout and verify exit code
   `0` and that stdout begins with `nypi <expected_version>`. If it fails (wrong architecture, missing shared
   library, corrupt binary), delete `nypi.new` and abort without touching `~/.nypi/bin/nypi`.
7. **Atomic swap**:
   - Copy existing `~/.nypi/bin/nypi` (when present) to `~/.nypi/bin/nypi.prev.tmp` and rename to `nypi.prev`.
   - Write `~/.nypi/run/update-pending.json` (`fromVersion`, `toVersion`, `startedAt`, `checkedAt`).
   - `rename("~/.nypi/bin/nypi.new", "~/.nypi/bin/nypi")`.
   - Clean up `.staging/`.
8. **Restart**:
   - If the supervisor service is installed, stop/restart it cleanly via `supervisor::restart` (macOS `launchctl
     kickstart -k`, Linux `systemctl --user restart nypi-supervisor.service`) after waiting briefly for active work
     to hit a safe boundary when possible.
   - If `std::env::current_exe()` is a separate regular file outside `~/.nypi/bin/nypi` (not a symlink to it), print
     a short note suggesting symlinking it to `~/.nypi/bin/nypi` so CLI and supervisor always stay in sync.

### 2.5 Automatic crash rollback in `crates/nypi-cli/src/supervisor.rs`

- On every round, if `~/.nypi/run/update-pending.json` exists:
  - **Confirm**: once every enabled service on this machine (`daemon` + `board`, or `node`, and/or `relay`) has been
    `Health::Alive` continuously for 30 seconds, remove `update-pending.json`, record `lastApplied` in
    `~/.nypi/run/update-state.json`, and emit a brief info log (and outbox notice if applied automatically).
  - **Roll back**: if any enabled service hits `CRASH_STARTS` or fails to reach `Health::Alive` within 90 seconds
    after `startedAt`, and `~/.nypi/bin/nypi.prev` exists:
    1. Stop running child services cleanly.
    2. Atomically rename `~/.nypi/bin/nypi.prev` -> `~/.nypi/bin/nypi`.
    3. Write `blockedVersion = pending.to_version` in `~/.nypi/run/update-state.json` so background auto-update will
       never retry the broken version in a loop.
    4. Remove `update-pending.json`.
    5. Push an `AlertLevel::Error` notice to the outbox:
       `Failed: update to v<to> did not stay healthy; rolled back to v<from>.`
    6. Exit the supervisor process so launchd / systemd immediately restarts it from the restored binary.

### 2.6 Fleet rolling update (`NodeCommand::Update` and `Admin`)

- Add `NodeCommand::Update { version: Option<String> }` (or equivalent message) in `nypi-node-proto` and handle it
  in `crates/nypi-node` / `crates/nypi-cli/src/node.rs`:
  - The node finishes any in-flight tool calls cleanly (preserving the lease journal from D74), invokes the local
    verified updater (`update::apply`), and replies with accepted/error before restarting.
  - The node **always verifies the Ed25519 signature and SHA-256 locally** against its own pinned public key.
- Allow `update --node ...` and `update --all ...` in `crates/nypi-hub/src/protocol.rs` (`admin_allowed`, `Personal`
  peers only, refused for `exec-only` as in D96). Plain `nypi update` (without `--node` or `--all`) always runs
  locally on the machine where the user typed it.

### 2.7 GitHub Actions workflows and POSIX installer

- `.github/workflows/ci.yml`:
  - Runs on push to `main` and pull requests:
    - `cargo fmt --all -- --check`
    - `cargo clippy --workspace --all-targets -- -D warnings`
    - `cargo test --workspace`
    - Hygiene check ensuring no personal domains, raw private key headers outside test files, or forbidden strings
      were reintroduced.
- `.github/workflows/release.yml`:
  - Triggered on tags matching `v*.*.*` (plus manual `workflow_dispatch` dry run):
    1. `board` job (`ubuntu-22.04`): runs `./scripts/build-board.sh` and uploads `crates/nypi-cli/assets/board/` as
       an artifact.
    2. `build` job matrix (each downloads `board` artifact into `crates/nypi-cli/assets/board/` before building so
       every binary embeds the same WebAssembly board):
       - `aarch64-apple-darwin` (`macos-14`)
       - `x86_64-unknown-linux-gnu` (`ubuntu-22.04` — ensures glibc 2.35 compatibility on Ubuntu 22.04 and newer)
       - `aarch64-unknown-linux-gnu` (`ubuntu-22.04-arm`, or cross-compile on `ubuntu-22.04` if ARM runner is
         unavailable)
       Packages `nypi-<version>-<target>.tar.gz` containing `nypi`.
    3. `publish` job (`ubuntu-22.04`): downloads all tarballs, builds `SHA256SUMS` and `manifest.json`, runs
       `nypi update sign manifest.json` using secrets.`NYPI_RELEASE_SIGNING_KEY` to produce `manifest.json.sig`,
       and creates/uploads the GitHub Release via `softprops/action-gh-release` (or `gh release create`).
- `scripts/install.sh`:
  - POSIX `sh` script (`curl -fsSL https://raw.githubusercontent.com/<REPO>/main/scripts/install.sh | sh`):
    - Detects OS (`Darwin` / `Linux`) and arch (`arm64`/`aarch64` / `x86_64`).
    - Downloads `nypi-<version>-<target>.tar.gz` and `SHA256SUMS` from GitHub Releases (latest or `NYPI_VERSION`).
    - Verifies SHA-256 (`shasum -a 256` on macOS, `sha256sum` on Linux).
    - Extracts to `~/.nypi/bin/nypi`, creates/updates symlink `~/.local/bin/nypi -> ~/.nypi/bin/nypi`, and prints a
      short note if `~/.local/bin` is not in `PATH`.

### 2.8 Tests for Part 2 (loopback mock HTTP only, zero network calls)

- `keygen` + `sign` + `verify` round-trip succeeds; mutated manifest bytes or wrong public key fail verification.
- End-to-end `update::apply` against a local `hyper` mock server serving `manifest.json`, `manifest.json.sig`, and a
  tarball containing a runnable stub `nypi` script:
  - Happy path: verifies signature & SHA-256, passes `--version` pre-flight check, saves `nypi.prev`, atomically
    swaps `nypi`, writes `update-pending.json`.
  - Tampered signature: rejected before downloading the archive; existing binary untouched.
  - Tampered archive (SHA-256 mismatch): rejected before extraction; existing binary untouched.
  - Tarball path traversal (`../evil`): rejected before extraction.
  - Failing pre-flight binary (non-zero exit or mismatched version): rejected; existing binary untouched.
  - Downgrade refused unless `--version` is explicitly passed.
- Rollback tests:
  - `nypi update rollback` restores `nypi.prev` -> `nypi` and clears `update-pending.json`.
  - Supervisor rollback logic: pending update whose service stays down past deadline restores `nypi.prev`, records
    `blockedVersion` in `update-state.json`, clears `update-pending.json`, and writes an `AlertLevel::Error` notice
    to the outbox; next background check skips `blockedVersion`.
  - Supervisor confirmation logic: pending update whose services stay healthy clears `update-pending.json`.
- Relay guard: `mode = "auto"` with `relay.enabled = true` and `autoRelay = false` notifies instead of auto-applying.
- Inspector guard test: `nypi update` and `nypi update --yes` are refused when invoked by an agent.
- Fleet tests: `Hello` carries version into `nypi devices`; `NodeCommand::Update` updates a node over loopback;
  `exec-only` peer is refused on `Admin` update commands.
