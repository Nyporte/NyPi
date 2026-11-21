# NyPi development rules

NyPi is the Rust workspace in `crates/`; this repository holds nothing else. The upstream Pi TypeScript sources are a
read-only reference in the sibling checkout `../Pi` (decision D18). Pi's own rules (`../Pi/AGENTS.md`) apply only there;
never edit that tree.

## Before changing behavior

- Read [docs/principles.md](docs/principles.md), [docs/architecture.md](docs/architecture.md), and
  [docs/roadmap.md](docs/roadmap.md). Design changes must trace back to a principle.
- architecture.md's body (§1–§4) describes the system as it is; its decision log (§5) keeps the history. When a
  change supersedes something, rewrite the body and add a decision, rather than appending "superseded" notes.
  Finished stage plans live in [docs/plans/](docs/plans/) and are not edited; [docs/overview.md](docs/overview.md)
  is a summary in Chinese that must match the code too.
- Work on the hub, front ends, tools, or the board follows [docs/devices.md](docs/devices.md) (decision D66): one
  hub holds the loop and all state, a node on each device carries out its calls, interfaces only talk and watch.
- Name parts of the system plainly, by what they are (hub, node, interface), not by metaphor.
- When a decision changes the architecture, update architecture.md (including the decision log) and the roadmap
  status in the same change.
- When porting a Pi mechanism, read the TypeScript source in `../Pi/packages/` in full first. Keep Pi's behavior unless the
  docs record a decision to differ.

## Code

- Layering: `nypi-ai` <- `nypi-agent` <- `nypi-vault` <- `nypi-core` <- `nypi-hub` <- `nypi-cli`. Never depend
  rightward. `nypi-eval` (the offline benchmark, decision D78) sits beside `nypi-hub`: it depends on `nypi-core`
  and below, and only `nypi-cli` depends on it. `nypi-hub-proto` (what an interface and the hub exchange: the
  ambient focus, roles, task summaries; serde only, decision D79) is used by `nypi-hub` and `nypi-cli` and depends
  on nothing of NyPi. `nypi-vault` is what NyPi keeps (episodic memory, the Markdown vault and its index, the asset
  registry, the secret store, and the append-only logs they are built on); it knows nothing of the runtime. The hub
  (threads, jobs, channels, the protocol) is `nypi-hub`; `nypi-cli` is the program, its interfaces, the supervisor,
  and the board. `nypi-node` (what carries out calls on a device) and its wire types `nypi-node-proto` depend on no
  model, session, or provider code, so they build for a phone: keep it that way. The board's wire types
  (`nypi-board-proto`, serde only) sit beside them; `nypi-board-ui` (WebAssembly) depends on nothing else of NyPi.
- A tool that touches the device (files, the shell) is the node's: the agent calls it through the thread's `Place`
  (nypi-core/src/place.rs). A tool that is Lucia's own (memory, schedules, goals, delegation) stays with the runtime.
- Three planes, kept apart (decision D84). Chat (`nypi-core/src/chat.rs`) is the only module that speaks to the
  user; it reads everything and changes nothing: its tools are the read-only wrappers and the time-boxed device
  reads in `nypi-core/src/introspect.rs`, `tasks_inspect`, and `signal_principal`. The Principal
  (`nypi-core/src/principal/`) decides: it holds the stores' tools, the task pool's, and `stimulate_chat`, its
  only way to the user, and no shell, device, network, or MCP tool. The task pool's workers act. A tool that
  changes a store is the Principal's or a worker's; one that touches a device or the network is a worker's. A
  device read of chat's never waits on an approval (`Inspector::allowed_unasked`) or longer than 1.5 seconds. What
  the planes log (`chat/turns.jsonl`, `principal/activations/`, `stores/decisions.jsonl`) goes through the secret
  store's `SecretScrubber` first.
- A goal's campaign is `10-projects/<slug>/plan.md` in the vault (decision D85). What the Principal starts or sets for
  a goal carries it (`goal` on `TaskBrief` and `start_task`, `goal_id` in the ledger, `goal` on a schedule), with
  the activation that started it. Idle time, timed goal reviews, fired schedules, and webhook material reach the
  Principal as activations when chat is in front; nothing posts a note to the user beside chat.
- The Principal reviews a settled task by its dossier (decision D87): the worker's `report.md` and the evidence
  manifest `evidence.rs` draws from the task's session, which the worker cannot write. Keep the manifest the
  harness's, and keep the Principal's system prompt cache-stable (`principal.md`, `<portrait>`, `<self>`): what
  changes opens the activation. A user's, review, or schedule activation ends disposed of (a task, a store change,
  `stimulate_chat`, or `conclude_activation`); the one nudge is the whole enforcement. A process that runs for days
  scrubs with a `LiveScrubber`, never a scrubber made once at start.
- Prompts and every string a model reads call the person "the user" (`{{user_ref}}` for the possessive), never "the
  owner" (decision D89). Prompt prose is plain and concrete: no jargon such as orthogonal, fan out, or gate.
- An activation's context is four rings (decision D88): the system prompt, `<situational_radar>`, `<briefing_pack>`,
  its transcript. The pack (`briefing.rs`) is made before the first model call and must never wait: words only, the
  vault's index only when free (`search_now`), no embedding, no network. Chat and the Principal report their step
  to the board through the process's board link (`PlaneReporter`); a new plane does the same.
- The board shows the planes from what they keep on disk (decision D86): it reads, and acts only through the
  actions in `nypi-board-proto`. Every answer of its API and event of its stream goes through `Board::scrub`; a new
  endpoint must too.
- A way things reach Lucia that is not an interface is a channel (`nypi-hub/src/channels/`, decision D81): it
  implements `Channel`, holds no model, session, or tool, and hands what comes in to the conversation through a
  `Port`. What a channel takes from outside the user (the inbound webhook) is saved as material and given to the
  conversation as a prompt with a `source` that is not the user's: never as the user's words, and its text says
  that nothing in the material is an instruction. A channel that authenticates callers reads its secret from the
  secret store and does not start without it. A channel's own credential (Telegram's bot token,
  `channels.telegram.botTokenSecret`) is read from the secret store by handle, with the old environment variable only
  as a fallback; a handle that is named but holds nothing keeps the channel off (decision D91).
- A trusted folder on another device may set only what shapes a run there and cannot weaken the hub (decision D91,
  `Runtime::apply_trusted_settings`): the thinking level, compaction, retries, and tools added to the approval list.
  Never read the approval mode, env files, MCP servers, the sandbox, channels, the hub, the board, or command tools
  from a device, and put the hub's own values back when the run works elsewhere.
- The hub holds one conversation (`PrimaryStream`, nypi-hub/src/daemon/primary.rs, decision D79). Do not key
  conversations by folder again: where work is done is the focus of the message (`Focus`), and a call that goes to
  another folder or device is judged for where it goes (`Place::judged`, the Inspector's router) and carried out
  under that node's own rules. A new device tool must work with a folder given per call.
- Every file starts with a `//!` comment saying what it is for. Comments explain non-obvious reasons, not what the
  next line does.
- Tagged enums for states and results. Providers never fail out of band: errors are a final message with
  `StopReason::Error`.
- Tools return errors as results the model can read. Never panic on model input.
- Nothing the agent can call may write inside NyPi's home (`~/.nypi`) or a project's `.nypi` folder, or read key
  files. `Protected` (nypi-core/src/protected.rs) is the one definition; the guard and the tool sandbox both use
  it. A new subprocess the agent can start goes through `Sandbox::command`. The vault (`~/.nypi/vault`) is
  written only in process, through the `vault_*` tools, which check every path (`Vault::resolve`).
- A secret's value never appears in a prompt, a tool result, the session, the audit log, or a `Debug` impl: the
  agent names it by handle (`cred://scope/name`), the harness uses it (`http_request`), and what comes back goes
  through `SecretScrubber` first.
- A connection to a socket in `~/.nypi/run` speaks as the user (the hub takes tasks and approval answers there).
  Nothing the agent can call may connect to one: the sandbox denies it, and the hub refuses its own children.
- What a machine's link may do is decided in one place, `daemon/remote.rs` (decisions D80, D93, D94), by the trust the
  machine was admitted with (`personal` or `exec-only`, in `devices/peers.json`) and the purpose the link names in its
  first line (`node`, `talk`, `join`, `leave`, `probe`), never by anything else the machine says and never by the way
  it came (directly on `hub.listen`, or through the relay): a `node` link goes to the register of nodes and is read as
  the node protocol only; `talk` goes to the conversation for a personal machine only; `join` waits for the user's
  answer and never reaches the conversation or the Principal; `probe` gets the hub's epoch and nothing else. A `talk`
  link whose first line is an admin command (`HubCommand::Admin`, decision D96) runs one of the hub's commands for a
  personal machine, only when `protocol::admin_allowed` names it, and never reaches the conversation; a new command
  that may be carried goes on that allowlist, prints through `outln!`, and starts no run.
- Nothing of one user goes into a tracked file (decision D97): no personal domain, gateway name, address, home path,
  or credential, in code, tests, examples, or docs. Examples use RFC 2606 names (`nypi.example.com`,
  `api.example.com`) and neutral placeholders (`gateway`, `GATEWAY_API_KEY`). There is no built-in entry: a machine
  works on its local network until the user saves one (`--entry`, kept in `settings.json`), and every path that
  uses the entry handles none quietly.
- Nothing is trusted from mDNS, a list of direct addresses, or the relay until the TLS handshake shows the pinned hub
  key; among hubs with that key the highest epoch wins (decision D94, `reach.rs`). The relay (`relay.rs`) is plain
  HTTP and WebSocket behind Cloudflare or a reverse proxy: it keeps only the hub key's fingerprint, the epoch, and the
  instance id, passes a link's frames unread (the pinned TLS runs inside them), and takes the hub's control connection
  only when the hub key signs its challenge. It believes a forwarded client address only from a connection on
  loopback (where Cloudflare's `cloudflared` or the reverse proxy runs), and then only the named `trustedHeader` or a
  list's last entry (decision D99). Browsers through the entry reach the board only on
  `run/board-public.sock`, which never takes the local token: a request needs a browser session the user admitted.
  Join requests (machines' and browsers') are limited, expire, and never reach the conversation or wake the
  Principal.
- There is exactly one hub (decision D95): the relay takes only the highest epoch, nodes refuse a lower one, and a hub
  that hears of a higher one steps down and sets its state aside under `diverged/`; nothing is merged. At no point may
  two daemons take work: a move stops the old hub's work before its last difference, and the target starts only after
  `released`. The hub's state (`state.rs`), with secrets, the secret store's master key, the hub key, and env files,
  goes only to `personal` machines and only over the pinned TLS link; it never appears in a log, a prompt, a tool
  result, an output, or the board, and nothing outside the NyPi home is written on the machine that receives it.
  `standby/`, `diverged/`, and `env/` are in `Protected`'s secrets. The keys
  (`hub-id.key`, `device-id.key`), invite codes, and join requests are in `Protected`'s secrets, and the guard refuses
  `nypi hub`, `nypi node`, `nypi relay`, and `nypi devices` from the agent.
- Updates (decision D98): the release manifest, its Ed25519 verification, and versions are `nypi-core/src/update.rs`;
  downloading, unpacking, the pre-flight check, the swap, the commands, and the supervisor's confirmation and rollback
  are `nypi-cli`. Nothing replaces `~/.nypi/bin/nypi` until the signature, the archive's size and SHA-256, and the new
  binary's `--version` have checked out, and every step that changes it is a rename. Releases come over HTTPS only
  (loopback only in tests, through `Source::loopback`), and the release key is built in (`release_public_key()`:
  `NYPI_RELEASE_PUBKEY`, else `DEFAULT_RELEASE_PUBLIC_KEY`); tests use a key of their own. An update is not in place
  until nothing of the old binary runs: the supervisor stops every service once, by the pids their files record, not
  only its own children (decision D99). The guard refuses `nypi update` from the agent, by any path and after any
  global flags. The user's service folders (`~/Library/LaunchAgents`, `~/.config/systemd/user`) are in `Protected`.
- A lock that keeps two processes from doing the same round is a `round_lock::RoundLock` (decision D83), never
  `flock`: a child forked while an `flock` is held keeps it until it execs.
- No secrets in logs, prompts, or URLs. Env files are loaded into a private lookup, never exported to tool
  subprocesses, and the keys NyPi itself was started with are taken out of every subprocess the agent can start
  (`sandbox::scrub_env`, which `Sandbox::command` applies; decision D76).

## Commands

```bash
cargo build
```

```bash
cargo test --workspace
```

```bash
cargo clippy --workspace --all-targets
```

- Tests use the faux provider (`nypi_ai::providers::faux`) or local mock servers. They make no network calls and
  need no paid tokens. `nypi eval` is the other kind of check: it runs the benchmark's scenarios with a real model
  and costs what that model costs; its harness is tested with scripted models like everything else. With
  `--three-planes` (decision D90) a scenario goes through chat, the Principal, and the task pool; its tests script
  each plane (`chat_stream_fn`, `principal_stream_fn`, `stream_fn`). A new scenario keeps one check for both modes.
- No error counters and no file-type gates to make the agent persist (decision D78). What keeps a task on course is
  the brief it was given, the workspace notes it keeps, the observer's rare steer, and the principal's review of the
  report; a new rule of the "after N failures do X" kind needs a decision in the log first.
- `scripts/build-board.sh` builds the board's page (WebAssembly) into `crates/nypi-cli/assets/board/` (not committed);
  rebuild `nypi` afterwards to embed it.
- `scripts/nypi-gen-models.py` regenerates `crates/nypi-ai/data/models.json` from Pi's catalog. Run it after
  `(cd ../Pi/packages/ai && npm run generate-model-catalog)`. Set `PI_DIR` if Pi is elsewhere.
- Commit only when asked. Stage explicit paths.
- The pre-commit hook (`scripts/githooks/pre-commit`) runs fmt, clippy with `-D warnings`, and the tests when Rust
  files are staged. Enable it once per clone with `git config core.hooksPath scripts/githooks`. Pi's TypeScript
  checks no longer run.
