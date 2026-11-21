# NyPi Plan: Post-D98 Refinements Across Updates, Security, Standby, and Relay (D99)

This plan builds on `D98` (`0a0c81859`). Execute all items below in one clean, well-tested commit (`D99`), updating
`docs/architecture.md` (body where applicable and decision `D99` in §5), `docs/roadmap.md`, `docs/overview.md`,
`docs/vision.md`, and `AGENTS.md`, and archiving `docs/d99-refinement-plan.md` into `docs/plans/d99-refinement-plan.md`
in the same commit.

Every check must pass before committing:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace` (zero external network calls)
- `./scripts/build-board.sh && cargo build`

---

## 1. Self-Update Restart Reliability, Pre-Loop Rollback, and CLI Fixes

### 1.1 Ensure all managed services actually restart onto the new binary (`supervisor.rs`, `update.rs`)

Problem: on macOS, `launchctl kickstart -k` restarts only the `supervisor` process, and `supervisor`'s exit loop only
stops child handles it spawned itself (`kept.child`). Surviving `daemon`, `board`, `node`, or `relay` processes from
before the update continue running the old binary; the new `supervisor` sees `all_alive() == true`, confirms the
update after 30 s while old binaries are still running, and `nypi update --all` times out waiting for nodes to report
the new version. In addition, if a broken binary panics or exits inside `supervisor` before its main loop runs,
`watch_update` never gets a chance to roll back.

Changes:

1. In `crates/nypi-cli/src/update.rs`, add two `#[serde(default)]` fields to `Pending`:
   - `restarted: bool` — whether the new supervisor has stopped pre-update service processes once.
   - `supervisor_starts: u32` — how many times `supervisor::run` has started while this `Pending` was on disk.
   Add a small helper `save_pending(paths, &pending)`.
2. At the very top of `supervisor::run` (before the `alive()` check or loop):
   - If `update::pending(paths)` is `Some(mut pending)`:
     - Increment `pending.supervisor_starts += 1` and save it.
     - If `pending.supervisor_starts > 3` and `update::can_roll_back(paths)`: call `roll_back_failed(paths)`, post the
       resulting `AlertLevel::Error` notice via `notify::post` + `notify::flush`, and exit with code `75` so
       launchd/systemd immediately starts the restored `nypi.prev`.
3. In `supervisor::round`:
   - When `update::pending(paths)` is `Some(mut pending)` and `!pending.restarted`:
     - Before `keep_services`, stop all running child services—both `state.*.child` handles AND any live PIDs recorded
       in `daemon::heartbeat(paths)`, `board::running(paths)`, `node::read_status(paths)`, and `relay::running(paths)`
       (reusing `stop(service, kept, pid).await`).
     - Set `pending.restarted = true`, reset `pending.started_at = Utc::now().to_rfc3339()`, save `pending`, and let
       `keep_services` immediately launch fresh processes from the newly installed binary.
4. In `supervisor::stop_all` (and the exit cleanup at the end of `supervisor::run`):
   - Pass each service's recorded PID from disk (`daemon::heartbeat`, `board::running`, `node::read_status`,
     `relay::running`) to `stop(...)` so services started by an earlier supervisor instance are also stopped cleanly
     when the supervisor stops or rolls back.

### 1.2 Fix interactive `nypi update --allow-unsigned`, silent `auto` failures, and SemVer pre-release ordering

1. `crates/nypi-cli/src/update.rs` & `crates/nypi-cli/src/machine.rs`:
   - Change `Updater::check(&self, allow_unsigned: bool)` to pass `allow_unsigned` to `self.manifest(...)`, and pass
     `allow_unsigned` from `machine::update_here` (line 301) so interactive `nypi update --allow-unsigned` (and
     `nypi update --check --allow-unsigned`) works instead of bailing before the `[y/N]` prompt.
2. `crates/nypi-cli/src/update.rs` (`background`, lines 455–459):
   - When `mode == UpdateMode::Auto && window_open && idle()` and `updater.apply(&options, true).await` returns
     `Err(error)`: log the error (`eprintln!`) and, if `state.notified.as_deref() != Some(latest.as_str())`, record
     `state.notified = Some(latest.clone())`, save state, and call
     `tell(format!("NyPi v{latest} is available, and automatic update failed ({error:#}) — run nypi update")).await`
     instead of failing silently every check interval.
3. `crates/nypi-core/src/update.rs`:
   - Add `pub const DEFAULT_RELEASE_PUBLIC_KEY: Option<&str> = None;` and define:
     ```rust
     pub fn release_public_key() -> Option<&'static str> {
         option_env!("NYPI_RELEASE_PUBKEY").filter(|k| !k.is_empty()).or(DEFAULT_RELEASE_PUBLIC_KEY)
     }
     ```
     so once the maintainer pastes their public key into `DEFAULT_RELEASE_PUBLIC_KEY`, source builds (`cargo build
     --release`) verify official releases without needing environment variables.
   - Fix `Version::cmp_to` pre-release comparison to follow SemVer 2.0.0 §11: split `a` and `b` on `.`, compare
     dot-separated identifiers as `u64` when both are ASCII digits (numeric identifiers sort lower than non-numeric),
     and compare by segment count when all shared segments match (`0.5.0-rc.10 > 0.5.0-rc.2`).

---

## 2. Release Pipeline (`release.yml`) and Installer (`scripts/install.sh`)

1. `.github/workflows/release.yml`:
   - Add a fast version check in `build` when `github.ref_type == 'tag'`: read the workspace `version` from
     `Cargo.toml` and fail immediately if `"v$cargo_version" != "$GITHUB_REF_NAME"`, preventing a tag mismatch that
     would later fail every client's `--version` pre-flight check.
   - Change the `aarch64-unknown-linux-gnu` matrix entry to run on `ubuntu-22.04` with `gcc-aarch64-linux-gnu`
     installed (`sudo apt-get update && sudo apt-get install -y gcc-aarch64-linux-gnu`,
     `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc`, skipping `./dist/nypi --version` only when
     cross-compiling `aarch64` on `x86_64`), avoiding unavailable `ubuntu-22.04-arm` runners on public repos while
     preserving glibc 2.35 compatibility.
   - In `Sign the manifest`: if `github.ref_type != 'tag'` (`workflow_dispatch` dry run) and
     `NYPI_RELEASE_SIGNING_KEY` is empty, print a notice and skip signing rather than failing the dry run; on a tag
     push, still require `NYPI_RELEASE_SIGNING_KEY`.
2. `scripts/install.sh`:
   - Validate the resolved `version` after querying `/releases/latest`: if `version` is empty, contains `://`, or
     contains `/`, print `No release has been published at https://github.com/$repo/releases yet.` to stderr and
     exit `1`.

---

## 3. Security Boundary Fixes (`protected.rs`, `inspector.rs`)

1. `crates/nypi-core/src/protected.rs`:
   - In `Protected::new`, protect both macOS and Linux user service locations:
     - Add `~/.config/systemd/user/nypi-supervisor.service` alongside
       `~/Library/LaunchAgents/dev.nypi.supervisor.plist` in `files`.
     - Include `~/Library/LaunchAgents` and `~/.config/systemd/user` in `protected.folders` (write-denied by both the
       guard and OS sandboxes—Seatbelt and bubblewrap—while still readable) so no tool or subprocess can plant a new
       background service unit to run outside the sandbox on login/reload.
     - Update the diagnostic message in `write_denied` (or keep clear wording covering both skill folders and user
       service folders) and add unit tests covering `~/.config/systemd/user/nypi-supervisor.service` and planting a
       new unit in `~/.config/systemd/user/evil.service` and `~/Library/LaunchAgents/evil.plist`.
2. `crates/nypi-core/src/inspector.rs`:
   - Tighten rule 9 in `Guard::new` so `nypi` invoked via a path (`/path/to/nypi`) or with global flags before the
     subcommand (e.g. `nypi --mode text update`, `~/.nypi/bin/nypi --hide-thinking hub down`,
     `nypi -m json devices admit 123456`) is still matched and refused, without false-positiving on unrelated commands
     such as `grep update src/main.rs` or `cargo test update`. Add unit tests in `inspector.rs`.

---

## 4. Standby Diff Efficiency, Relay Client IP Hardening, and Board Asset Caching

1. **Incremental metadata cache for standby sync** (`crates/nypi-hub/src/state.rs`, `crates/nypi-hub/src/daemon/handover.rs`):
   - Add a `StateCache` (`HashMap<PathBuf, CachedFile { len: u64, modified: SystemTime, sha: String }>`) passed into a
     cached variant `difference_cached(paths, settings, have, &mut cache)` used by `serve_copy` across rounds (keep
     `difference(...)` calling `difference_cached` with a fresh cache for one-shot callers and existing tests):
     - For any file whose `(len, modified)` matches `cache` AND whose cached `(len, sha)` equals `have.files.get(&path)`,
       skip reading the file completely (zero disk read, zero allocation).
     - For `.jsonl` files that grew (`held.size < meta.len()`), read only the tail bytes from `held.size` when `cache`
       confirms the file previously had `(held.size, held.sha)` on this hub (otherwise read the full file as today to
       verify the prefix), compute the new full SHA-256 (or stream-hash the file in 64 KB chunks without holding two
       full copies in RAM), update `cache`, and emit `Change::Append`.
   - Add a unit test verifying that unchanged files are not re-read/re-hashed when `(len, modified)` is unchanged and
     that `.jsonl` appends still produce identical `Piece` entries and final SHA-256 checksums.

2. **Relay proxy-header hardening** (`crates/nypi-core/src/config.rs`, `crates/nypi-hub/src/relay.rs`):
   - Add `#[serde(default)] pub trusted_header: Option<String>` to `RelaySettings` (default `None`).
   - In `Relay::client(&self, head: &Head, peer: SocketAddr) -> IpAddr`:
     - Trust forwarded headers **only** when `self.settings.trust_proxy && peer.ip().is_loopback()` (or when
       `peer.ip()` is loopback/private reverse proxy—specifically, never trust client-supplied headers on a direct
       public internet socket; keep `peer.ip().is_loopback()` so local Nginx / Caddy / `cloudflared` on `127.0.0.1` /
       `::1` works and all loopback `network_tests` pass).
     - When `trusted_header` is `Some(name)`: read only `head.get(name)` (taking the rightmost comma-separated IP if a
       list).
     - When `trusted_header` is `None`: check `cf-connecting-ip`, then `x-real-ip`, then the **last** (rightmost,
       proxy-appended) valid IP in `x-forwarded-for` (`rsplit(',').find_map(...)` rather than `.split(',').next()`, so
       a client spoofing `X-Forwarded-For: 1.2.3.4` in front of Nginx cannot forge its IP).
   - Add unit tests covering: non-loopback peer ignores `CF-Connecting-IP` even when `trust_proxy` is true; multi-hop
     `X-Forwarded-For: 1.2.3.4, 198.51.100.8` takes `198.51.100.8`; `trusted_header = Some("x-real-ip")` ignores a
     spoofed `CF-Connecting-IP` and uses `X-Real-IP`.

3. **Board static asset `ETag` / `304` and redirect query preservation** (`crates/nypi-cli/src/board/web.rs`,
   `crates/nypi-cli/src/board/public.rs`):
   - In `web.rs` (`asset` / `routes`): compute a stable quoted `ETag` for each embedded static asset (e.g. from SHA-256
     of its uncompressed bytes, or build hash + asset name). On authorized static asset requests (`GET` of `index.html`,
     `board.js`, `board_bg.wasm`, `board.css`, icons, manifest):
     - Set `ETag` and `Cache-Control: private, no-cache` (replacing `no-store` for static bundle files only; `/api/*`,
       locked pages, and admission pages MUST keep `Cache-Control: no-store`).
     - When the request's `If-None-Match` matches the asset's `ETag`, return `304 Not Modified` with an empty body (and
       the same security/`ETag`/`Cache-Control` headers), avoiding re-transferring hundreds of KB of WebAssembly over
       the relay on every reload.
   - In `public.rs` (`admission`, line 108): preserve any query string (`path?query`) in the `303 See Other` `Location`
     header when redirecting an admitted browser.
   - Add a board unit test in `crates/nypi-cli/src/board/tests.rs` verifying `ETag` + `304 Not Modified` on static
     assets while `/api/*` continues to send `Cache-Control: no-store`.

---

## 5. Documentation Alignment

- Rewrite `docs/vision.md` so it matches the live system (three planes—Chat, Principal, worker task pool; vault,
  campaigns, evidence dossiers, and single primary stream; Hub, Nodes, and optional HTTP/WebSocket Relay with inner
  pinned TLS 1.3; `"the user"` throughout, zero `"owner"`, `"MemoryValve"` as a typing-recall gate replaced by the
  accurate current design).
- Record decision `D99` in `docs/architecture.md` §5, update `docs/roadmap.md` and `docs/overview.md` if any phrasing
  changed, and archive `docs/d99-refinement-plan.md` to `docs/plans/d99-refinement-plan.md`.
