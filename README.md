# NyPi

NyPi is a personal-agent framework written in Rust, built as one `nypi` binary. The agent that runs on it has a
per-user name (the default is **Lucia**). It started as a reimplementation of [Pi](https://github.com/earendil-works/pi)
(TypeScript) and keeps Pi's kernel: the turn loop, tools, sessions, compaction, and skills. On top of it NyPi is a
"chief of staff" that runs all day across your machines, in three planes:

- **Chat** answers you at once. It reads everything and changes nothing.
- **The Principal** decides: it plans, keeps the stores (goals, memory, the vault, schedules), starts tasks, and
  reviews what comes back. It never touches a device itself.
- **The task pool's workers** act: files, the shell, the network, each task under a brief, with the evidence of what it
  did kept by the harness.

## What it does

- **Never silent**: every run is a task in a durable ledger, with a status (Done, Blocked, Failed, Continuing); a
  watchdog takes up interrupted work again, and every notice goes through an outbox until it reaches you.
- **Memory and notes**: episodic memory with local embeddings, a Markdown vault of notes and project plans, a portrait
  of you learned from your own words, and reflections after tasks.
- **Goals and schedules**: goals with campaigns and reviews, reminders, recurring tasks, condition triggers, and idle
  time spent on what is due.
- **Specialists and tools**: native specialists and external agent CLIs (Codex, Claude Code), MCP servers (stdio and
  HTTP), and your own programs as tools.
- **Safety**: an Inspector with an append-only audit log, a guard, approvals for risky calls, the OS sandbox (Seatbelt
  on macOS, bubblewrap on Linux), protected paths no tool can read or write, and a secret store whose values the
  model never sees.
- **The board**: a web page, compiled to WebAssembly and built into the binary, showing what needs you, what runs now,
  every task as a story, and the whole system as a map, with approve, deny, stop, and message.
- **Channels**: Telegram and an inbound webhook, beside the terminal (line or full-screen) and an RPC mode.
- **Across machines**: one **hub** holds the conversation and all state; a **node** on each other machine carries out
  calls under that machine's own rules. Machines join with a six-digit code you confirm, link directly on the local
  network (found by mDNS) whenever they can, and otherwise through an optional **relay** behind Cloudflare or a reverse
  proxy, with end-to-end pinned TLS 1.3 the relay cannot read. `nypi hub here` moves the hub to another of your
  machines, which keeps a standby copy of its state.

## Install

On macOS (Apple silicon) or Linux (x86_64, aarch64), from the signed releases:

```bash
curl -fsSL https://raw.githubusercontent.com/Nyporte/NyPi/main/scripts/install.sh | sh
```

It checks the archive against the release's `SHA256SUMS`, installs `~/.nypi/bin/nypi`, and links `~/.local/bin/nypi`
to it.

From source, with Rust (stable); the board's page is built separately into the binary:

```bash
./scripts/build-board.sh && cargo build --release
```

The binary is `target/release/nypi`. Configuration lives in `~/.nypi/` (`settings.json`, `models.json`); model keys
are read from the environment or from env files. See [docs/usage.md](docs/usage.md).

## Quick start

On one machine:

```bash
nypi                         # talk, in this folder
```

```bash
nypi -p "summarize README.md"
```

```bash
nypi hub up                  # keep Lucia running, with the board, as a service
```

A second machine on the same network joins the hub, and shows a code you confirm on the hub:

```bash
nypi node up
```

To reach the hub from anywhere, run the relay on a machine with a public name (for example a small VPS behind a
Cloudflare Tunnel or Nginx), and give the hub that name once; it is kept in `settings.json`:

```bash
nypi relay up --hub-key <the fingerprint nypi hub up showed>
```

```bash
nypi hub up --entry nypi.example.com:443
```

Updates come from the same releases: `nypi update --check` says whether there is one, `nypi update` puts it in place
on this machine (its signature, size, and SHA-256 checked first, and rolled back by the supervisor if the new
version does not stay up), and `nypi update --all` on the hub updates every machine in turn. By default each machine
checks every six hours and tells you once of a new release (`update.mode`: `off`, `notify`, or `auto`).

## Layout

```
crates/nypi-ai          messages, streaming, providers, the model catalog
crates/nypi-agent       the turn loop, the tool trait, hooks, queues
crates/nypi-vault       memory, the vault and its index, assets, the secret store
crates/nypi-core        tools, sessions, prompts, settings, the Inspector, the runtime, the three planes
crates/nypi-hub         the hub: the conversation, jobs, channels, devices, the relay
crates/nypi-cli         the `nypi` binary: interfaces, the supervisor, the board's service
crates/nypi-node        what carries out calls on a device (nypi-node-proto: its wire types)
crates/nypi-eval        the offline benchmark
crates/nypi-board-ui    the board's page (WebAssembly)
docs/                   the design documents (normative)
scripts/                the installer, the board's build, the model catalog generator, git hooks
```

Read [docs/principles.md](docs/principles.md) for how NyPi behaves, [docs/architecture.md](docs/architecture.md) for
what exists and why, [docs/usage.md](docs/usage.md) for setup and commands, [docs/devices.md](docs/devices.md) for the
hub, nodes, and relay, and [docs/roadmap.md](docs/roadmap.md) for what is done and what is open. Security:
[SECURITY.md](SECURITY.md).

## Credit and license

NyPi is derived from [Pi](https://github.com/earendil-works/pi) by Mario Zechner, whose design it ports and extends
([docs/pi-analysis.md](docs/pi-analysis.md) says what was kept, changed, or dropped). MIT, as upstream Pi: see
[LICENSE](LICENSE).
