# NyPi Architectural Seams, Boundary Fixes, and Macro Optimizations

This document synthesizes a full-repository code review of NyPi (including the uncommitted D68 remote-node changes in
the working tree) against the owner's original brief, [principles.md](../principles.md), [architecture.md](../architecture.md),
and [devices.md](../devices.md).

It is written as an actionable specification for implementation. Every item traces back to a principle (`A1`–`A8`,
`B1`–`B6`) or requirement (`R1`–`R10`), names the exact files and mechanisms involved, and follows the workspace rules
in `AGENTS.md` (`nypi-ai <- nypi-agent <- nypi-core <- nypi-hub <- nypi-cli`, plain names, tagged enums, never-panic
tools, and `Protected` as the single boundary definition).

## Status (2026-11-18)

Steps 1 to 7 were carried out in order; each is a decision in [architecture.md](../architecture.md) and a row in the
roadmap's "Seams found by the review" table. Line numbers below refer to the code before these changes.

| Step | Decision | Done | Left, and why |
|---|---|---|---|
| 1 | D69 | `real_path` per component; `read`'s name forms in the guard; file tools check what they open; user skill folders protected; `delegate` starts in the checked folder; boundary tests | — |
| 2 | D70 | One approval id; `Approval::Closed` to the loops; `listened` keeps the board's questions from being denied | — |
| 3 | D71 | `PlaceKey`; `remote` on tasks and schedules; routing of resumes and fired prompts; percent-encoded stand-ins; argument fix-ups for remote calls; device home in the policy, device OS in the prompt; no skills at a remote place; settings errors stop `nypi node`; fast replacement of a silent link | A device's folder configuration and schedule conditions at a remote place |
| 4 | D72 | `HubCommand`/`RuntimeCommand`; chat parity (runtime commands, `bash`, steering, follow-ups, reflections); `daemon/` split | The process-wide log switch is kept on purpose (see D72) |
| 5 | D73 | `stuckMinutes`; concurrent preparation; symmetric aborts; folder refresh before each run; standalone parity; one retry layer; redacted keys; no empty read rule; peer ancestry by system call, also on `bridge.sock` | Tokenizer hash pin (needs the upstream hash); Linux sandbox |
| 6 | D74 | Node journal, lease, `ack`, reattach; line cap; `set_origin`; Devices on the map | One board link per hub (a board-phase change) |
| 7 | D75 | Chat history per worker session; delegation wakes are a join | Conversation not bound to a place; chat speaking after tasks (each needs its own design) |

---

## Executive Diagnosis: Where NyPi Stands Against the Original Brief

Decisions D64–D67 successfully reshaped NyPi from six separate front-end loops into one hub (`nypi-hub`), device-facing
nodes (`nypi-node`, `nypi-node-proto`), thin interfaces (`nypi-cli`), one scoped `PulseBus`, background specialist
delegation, and incrementally folded append-only stores (`Folded<S>`).

Two categories of work remain:

1. **Concrete seams and boundary defects in the current code (Parts 1–2 below).** Several seams exist where two
   subsystems make slightly different assumptions—most critically in `Protected::real_path` vs. in-process file tools,
   split-brain approval IDs between `BoardLink` and the hub loops, uncommitted D68 remote-place stand-in directories,
   untyped `serde_json::Value` internal hub messages (`rpc.rs` vs. `chat_loop.rs` drift), and unconditional ledger
   heartbeats masking hung streams.
2. **Macro structural tensions inherited from Pi's folder-bound coding-CLI model (Part 3 below).** Pi's core unit is a
   single-lane coding session bound to one directory (`cwd`). Growing NyPi into one continuous, 24/7 Chief of Staff
   ("Lucia") across terminals, computers, and phones requires decoupling **who Lucia is talking to** (a persistent
   conversation) from **where work executes** (one or more `(device, folder)` places), separating lightweight
   orchestration from single-lane execution, typing the internal hub protocol, and adding durable checkpoints/indexes
   and node journals.

---

## Part 1: Immediate Security Boundary & Correctness Fixes (P0 / P1)

Fix these before extending multi-device or autonomous capabilities.

### 1.1 Fix three symlink/path-resolution bypasses in `Protected` and in-process file tools (`R5`, `D62`)

`BashTool` and `CommandTool` run under macOS Seatbelt (`sandbox-exec`), where the kernel checks the target inode.
However, `ReadTool`, `WriteTool`, and `EditTool` execute in-process inside the **unsandboxed** harness/node via
`tokio::fs`, relying entirely on `Guard` + `Protected`. Three discrepancies allow a model to read secrets (`auth.json`,
`~/.env`, `run/*`) or write inside `~/.nypi` and `.nypi`:

1. **Lexical `..` (`Component::ParentDir`) collapsing before symlink resolution in `real_path`**
   - **Where**: `crates/nypi-core/src/protected.rs:31-56` vs. `crates/nypi-core/src/tools/mod.rs:114-127`,
     `crates/nypi-core/src/tools/write.rs:35-43`, `crates/nypi-core/src/tools/edit.rs:67`,
     `crates/nypi-core/src/tools/read.rs:42`.
   - **Defect**: `real_path` pops `Component::ParentDir` lexically *before* calling `std::fs::canonicalize`, whereas
     `WriteTool`/`EditTool`/`ReadTool` pass the un-collapsed `resolve_path(&self.cwd, ...)` directly to `tokio::fs`.
   - **Reproducer**: A sandboxed `bash` call runs `ln -s ~/.nypi/audit ./escape` inside the workspace (allowed by
     Seatbelt: it writes only `./escape`). Next, `write` is called with `{"path": "escape/../settings.json", ...}` (or
     `read` with `"escape/../auth.json"`). `real_path` lexically pops `escape`, sees `<cwd>/settings.json`, and allows
     the call; `tokio::fs::write("<cwd>/escape/../settings.json", ...)` traverses the symlink in the kernel and writes
     `~/.nypi/settings.json`.
   - **Also affects**: `delegate` (`crates/nypi-core/src/inspector.rs:508-512` vs. `delegate.rs:148-150`, which uses
     `expand_home` rather than `resolve_path` and passes un-canonicalized `cwd` to unsandboxed external specialist CLIs).

2. **Dangling file symlinks treated as plain missing filenames in `real_path`**
   - **Where**: `crates/nypi-core/src/protected.rs:43-55`.
   - **Defect**: When `~/.nypi/settings.json` (or `<project>/.nypi/settings.json`) does not yet exist on disk and
     `<cwd>/harmless.txt` is a symlink to `~/.nypi/settings.json`, `std::fs::canonicalize("<cwd>/harmless.txt")` returns
     `Err(ENOENT)`. `real_path` pushes `"harmless.txt"` onto `missing`, canonicalizes `<cwd>`, and returns
     `<cwd>/harmless.txt`. `tokio::fs::write` (`O_CREAT | O_WRONLY`) follows the dangling symlink and creates
     `~/.nypi/settings.json`.

3. **Unicode fallback mismatch between `Guard::target` (`resolve_path`) and `ReadTool` (`resolve_read_path`)**
   - **Where**: `crates/nypi-core/src/inspector.rs:462-466, 506` vs. `crates/nypi-core/src/tools/read.rs:41-43` and
     `crates/nypi-core/src/tools/mod.rs:138-155`.
   - **Defect**: When the literal path does not exist, `resolve_read_path` falls back to macOS name variants (curly
     apostrophe `\u{2019}`, NFD decomposition, narrow no-break space before `AM`/`PM`), whereas `Guard::target` calls
     plain `resolve_path`. If `<cwd>/note’s.txt` (curly apostrophe) symlinks to `~/.nypi/auth.json`, calling `read`
     with `{"path": "note's.txt"}` (straight apostrophe) passes `Guard` as `<cwd>/note's.txt` and then reads
     `~/.nypi/auth.json` via the curly-apostrophe fallback.

- **Required fix**:
  - Rewrite `real_path` to resolve symlinks component-by-component (using `std::fs::symlink_metadata` / `read_link`
    with a symlink-hop cap, resolving relative symlink targets against the current resolved parent directory) so `..`
    is never applied across an unresolved symlink and dangling final-component symlinks are resolved to their real
    target path.
  - Have `Guard::check("read", ...)` resolve via `resolve_read_path`.
  - Pass `Arc<Protected>` into `ReadTool`, `WriteTool`, and `EditTool` (as `GrepTool` already does in
    `tools/grep.rs:22, 97`), resolve the final path once, verify `protected.read_denied` / `protected.write_denied`
    immediately before opening, and use the resolved path for the filesystem operation.
  - Protect `~/.agents/skills` and `~/.pi/agent/skills` (`crates/nypi-core/src/skills.rs:23`) under `Protected` writes
    or `Policy::sensitive_files`, since `default_skill_dirs` loads them unconditionally without a project trust prompt.
  - Add regression tests for all three exploit shapes in `crates/nypi-core/tests/boundary.rs`.

---

### 1.2 Unify split-brain approval IDs across `Inspector`, `BoardLink`, `rpc`, `chat_loop`, and interfaces (`A1`, `A3`, `A5`, `D56`)

- **Where**:
  - `crates/nypi-core/src/inspector.rs:236-285` (`Inspector::ask`)
  - `crates/nypi-hub/src/rpc.rs:284-292`
  - `crates/nypi-hub/src/chat_loop.rs:278-285`
  - `crates/nypi-hub/src/daemon.rs:430-463, 505-531`
  - `crates/nypi-cli/src/tui/mod.rs:144-154`
  - `crates/nypi-cli/src/run_once.rs:88-99`
- **Defect**:
  1. `Inspector::ask` mints a 12-char hex UUID (`request.id`) and sends it to `BoardLink` (`ApprovalEvent::Waiting`).
  2. `rpc.rs` and `chat_loop.rs` ignore `request.id` and emit `"id": "approval-1"`, `"approval-2"` on the hub stream.
  3. When the user approves or denies on the web **Board**, `BoardLink` calls `Inspector::answer(&uuid_id, ...)`. The
     oneshot resolves and the tool runs, **but neither `rpc.rs` nor `chat_loop.rs` is notified**, so
     `approval_resolved` is never emitted on the hub stream until the entire run settles.
  4. Consequences:
     - In `nypi --tui` (`tui/mod.rs:144`), `app.approval` stays open and **hijacks the user's next Enter keystroke as a
       denial of the already-answered approval**.
     - In `daemon.rs`, `hub.approvals` retains `"approval-1"`, replaying the stale approval to newly attached terminals
       and firing a false `"Blocked: waiting for your approval"` escalation notice after `notify.approvalSeconds`.
     - Conversely, in standalone print runs (`daemon.rs:446-451`, `run_once.rs:91-98`), when `bridge.sock` is absent,
       the loop immediately sends `approved: false`, overriding `Inspector`'s wait (`sent || listened`,
       `inspector.rs:258`) even when a running `nypi board` is connected and displaying the approval card.
- **Required fix**:
  - Use `request.id` everywhere (`rpc.rs`, `chat_loop.rs`, `daemon.rs`, `telegram_channel.rs`, `client.rs`, `tui/`).
    Telegram can still map short display numbers (`1`, `2`) locally to `request.id`.
  - Subscribe `rpc.rs` and `chat_loop.rs` (or a channel on `ApprovalRequest` / `Inspector`) to `ApprovalEvent::Closed`
    so answering from the Board immediately removes the entry from `pending`, emits `approval_resolved` (with `approved`
    outcome), clears `hub.approvals`, and dismisses the TUI/terminal prompt.
  - Before auto-denying in unattended/print local runs, check whether `Inspector` has an active approval listener
    (connected Board) or bridge.

---

### 1.3 Close the uncommitted D68 remote-node (`--on <device>`) correctness gaps (`A6`, `B6`, `R5`, `D66`, `D68`)

Before finalizing D68 (`crates/nypi-core/src/device.rs`, `crates/nypi-hub/src/nodes.rs`, `crates/nypi-cli/src/node.rs`,
and the modified files in `nypi-core`/`nypi-hub`/`nypi-cli`), resolve five concrete defects:

1. **Watchdog/daemon resume poisons `Threads::open` with a local thread**:
   - `Threads::at_device` (`crates/nypi-hub/src/daemon.rs:822-848`) sets `runtime.cwd` to the hub-local stand-in path
     `~/.nypi/places/<device>/<folder>`, and `Runtime::begin_task` (`crates/nypi-core/src/runtime/tasks.rs:35`) records
     only `cwd: self.cwd.clone()` in `LedgerEvent::Created` (there is no `device: Option<String>` on `TaskRecord`).
   - After a daemon restart, `Jobs::carry_out(Order::Resume)` (`crates/nypi-hub/src/jobs.rs:188-205`) calls
     `threads.at(Some(&task.cwd))` instead of `threads.at_device(...)`. Because `~/.nypi/places/<device>/<folder>` is a
     real directory on the hub's disk, `threads.at` opens a **local** thread (`place: None`) at that exact map key in
     `Threads::open`! Resumed tools run against the hub's protected `~/.nypi` directory, and every subsequent
     `nypi --on <device> --at <folder>` hits `self.get(&key)` at `daemon.rs:830` and attaches to the broken local thread.
   - The same happens for `ScheduleTool` (`crates/nypi-core/src/runtime/mod.rs:611`), which records the stand-in `cwd`
     and fires locally via `jobs.rs:165`.
   - **Fix**: Add `device: Option<String>` (and remote `folder`) to `LedgerEvent::Created` / `TaskRecord` and `Schedule`,
     key `Threads::open` by a typed `PlaceKey { device: Option<String>, folder: PathBuf }` (also eliminating the
     `/`-to-`-` sanitization collision in `daemon.rs:823` where `/a/b` and `/a-b` collide), and route `Order::Resume`
     and `Order::Prompt` through `at_device` when `device.is_some()`.

2. **`NodeTool::prepare` is bypassed on remote calls (`InFolder::prepare` is dead code)**:
   - `AtPlace::prepare_arguments` (`crates/nypi-core/src/place.rs:170-172`) calls `NodeLink::prepare`, which neither
     `RemoteNode` nor `ByName` (`crates/nypi-hub/src/nodes.rs:77-161`) implements, and `Node::call`
     (`crates/nypi-node/src/lib.rs:162-182`) never calls `tool.prepare(...)`. As a result, `InFolder::prepare`
     (`crates/nypi-core/src/device.rs:52-54`) is unreachable, and legacy/single-object `edit` arguments fail
     `validate_arguments` on the hub before reaching the node.
   - **Fix**: Either include argument normalization rules in tool schema/client preparation or call `tool.prepare`
     inside `Node::call` while relaxing hub-side pre-validation for `AtPlace` where needed (or ship standard device-tool
     argument normalization in a shared helper).

3. **Hub-side `Policy::sensitive_files` and `Guard` evaluate remote paths against the hub's local `HOME` and disk**:
   - `Policy::new` (`crates/nypi-core/src/inspector.rs:587-602`) builds `.ssh`, `.gnupg`, `.zshrc`, `.bashrc`, etc. from
     the **hub's** `dirs::home_dir()` (e.g., `/Users/alice`). If the remote node is `/home/ubuntu`, writing to
     `/home/ubuntu/.ssh/authorized_keys` or `/home/ubuntu/.bashrc` misses `Policy::sensitive_files` on the hub, and the
     remote node only runs `Guard` (never `Policy`), silently skipping `ApprovalMode::Risky` approval. Meanwhile
     `Guard::target` runs `std::fs::canonicalize` against the hub's local filesystem for remote paths.
   - **Fix**: Include the node's `home_dir` and `os` in `nypi_node_proto::Report` (`Identity` / `Environment`), build
     remote `Policy::sensitive_files` against the node's reported home directory (plus `~/` and `$HOME/`), and skip
     hub-local `std::fs::canonicalize` when `runtime.device().is_some()` (relying on lexical check + `Policy` on the hub
     and exact `real_path` + `Sandbox` in the remote node's `Gate`).

4. **65-second reconnect lockout after half-open SSH tunnel drops**:
   - `Nodes::link` (`crates/nypi-hub/src/nodes.rs:246-250`) refuses a connection if `connected.contains_key(&name)`.
     When an SSH tunnel flaps, `nypi node` reconnects in 1s (`node.rs:41`), while the hub's dead stream sits waiting for
     `SILENT_LIMIT` (65s, `nodes.rs:39, 273`), locking the node out for a full minute.
   - **Fix**: When a second `Hello` arrives for a connected `name`, immediately probe the existing connection (e.g.,
     short ping timeout of 2–3s) or replace the stale session once authenticated.

5. **Remote prompt & settings hygiene**:
   - In `crates/nypi-core/src/prompt.rs:224-225`, report the remote node's OS (`place.report().identity.system`) rather
     than `std::env::consts::OS`.
   - In `crates/nypi-core/src/runtime/mod.rs:471`, do not advertise hub-local `SKILL.md` file paths in `<skills>` to a
     remote thread whose `read` tool runs on another machine unless reading skills is mediated by the hub.
   - In `crates/nypi-core/src/device.rs:68`, log/surface parse errors from `Settings::load` instead of silently falling
     back via `.unwrap_or_default()`, which drops user-configured `envFiles` from `Protected`.

---

## Part 2: Architectural Seams & Internal Drift to Eliminate

### 2.1 Replace untyped JSONL `Value` plumbing inside `nypi-hub` and close `rpc.rs` vs. `chat_loop.rs` drift (`A2`, `A5`, `A8`)

- **Current seam**:
  `nypi-node-proto` and `nypi-board-proto` use exhaustive serde-tagged Rust enums, whereas the hub-to-interface and
  intra-hub protocol (`rpc.rs`, `chat_loop.rs`, `daemon.rs`, `link.rs`, `client.rs`, `tui/`, `telegram_channel.rs`)
  passes raw `String`s and `serde_json::Value` maps. Because `rpc.rs` (worker-only) and `chat_loop.rs` (chat + worker +
  thinking) re-implement command dispatch separately over untyped JSON, several regressions have crept in:
  1. **Silent loss of post-task reflection notices in chat mode**: `rpc.rs:84-86` drains
     `runtime.finished_reflections()` and emits `{"type": "reflection", ...}`; `chat_loop.rs` never calls
     `finished_reflections()`, dropping reflection summaries on every thread with `chat.model` set.
  2. **`/followup` broken in chat mode**: `client.rs:531` and `tui/mod.rs:168` pass `busy && !chat` (`false` whenever
     chat is enabled) into `typed()`, turning `/followup <text>` into an immediate prompt instead of queueing for after
     the running worker task (`steer_task` with `when: "after"` or worker follow-up queue).
  3. **~20 RPC commands fail when `chat.model` is configured**: `chat_loop.rs:213-216` rejects `set_model`,
     `cycle_model`, `set_thinking_level`, `compact`, `get_messages`, `get_session_stats`, `get_tree`, `fork`, `clone`,
     `switch_session`, `bash`, `export_html`, etc. with `"<cmd> is not available on a conversation with a chat module"`.
  4. **`daemon.rs` is a 1,977-line monolith with process-global state**: It mixes heartbeat I/O, `Hub`/`Port` broadcast
     state, `Teller`/`pump`/`watch` notification & bridge escalation, `Thread`/`Threads` lifecycle, Unix socket peer
     ancestry checks, and `daemon::Local`, plus a global `static LOGGING: AtomicBool` (`daemon.rs:109`).
- **Optimization**:
  - Introduce typed internal protocol enums (`HubCommand`, `ThreadRecord`) in a dedicated module or `nypi-hub-proto`
    crate; keep JSONL serialization strictly at socket/stdio boundaries.
  - Share idle/control command handling (`controls.rs`, `sessions.rs`, reflection draining, follow-ups) between
    worker-only and chat-enabled threads so turning `chat.model` on never removes capabilities.
  - Split `crates/nypi-hub/src/daemon.rs` by responsibility (`heartbeat.rs`, `pump.rs`, `thread.rs`, `threads.rs`,
    `local.rs`, `peer.rs`), matching the `runtime/` split from D67 part 5, and replace `static LOGGING` with a scoped
    logger/sink passed into the hub.

---

### 2.2 Eliminate the stuck-run blind spot: decouple liveness heartbeats from progress leases (`B6`, `A3`)

- **Current seam**:
  - `Runtime::spawn_heartbeat` (`crates/nypi-core/src/runtime/run.rs:329-344`) writes `LedgerEvent::Heartbeat` every 30s
    from an unconditional Tokio interval task, even if a model stream, MCP call, or tool subprocess hangs indefinitely
    without emitting an event.
  - Consequently, `watchdog.rs:109-112` (`stallSeconds = 300`) **never** marks an async-hung run in a live process as
    `Stalled`. `daemon::pump` emits `Continuing: no progress` warnings, but nothing ever cancels a wedged run:
    `Thread::busy` remains `true` forever, permanently head-of-line blocking `later: VecDeque<Sourced>` (scheduled
    prompts, goal reviews, and background specialist `Reported` reviews) on that thread.
  - Two minor copy mismatches reinforce the confusion: `crates/nypi-board-ui/src/now.rs:216` states *"The watchdog
    resumes stalled tasks"* (in code, the watchdog only resumes tasks of *dead* PIDs, never `Stalled` live PIDs), and
    `crates/nypi-core/src/watchdog.rs:126` references `run/resume-{id}.log`, which no longer exists since D67 part 3.
- **Optimization**:
  - Track `last_progress_at` alongside process liveness, or enforce configurable maximum idle ceilings on MCP calls and
    wedged turns so a run with zero progress past a hard ceiling aborts cleanly into an auto-retry or `Failed`/`Blocked`
    outcome instead of wedging the thread forever.
  - Fix the inaccurate Board copy in `now.rs:216` and remove the stale `run/resume-{id}.log` reference in
    `watchdog.rs:126`.

---

### 2.3 Fix parallel tool preparation blocking on sequential human approvals in `nypi-agent` (`A5`, `A6`)

- **Where**: `crates/nypi-agent/src/agent.rs:413-439`.
- **Defect**:
  1. In the parallel execution branch (`else` at line 418), `for call in calls` emits `ToolExecutionStart` and then
     **awaits `self.prepare(call, cancel).await` sequentially** before pushing `run_prepared` futures into `pending` for
     `join_all(pending).await`. Because `Inspector::before_tool_call` (including human approval waits!) runs inside
     `prepare()`, if call #1 needs approval (`git push`) and calls #2–#4 are read-only (`read`), calls #2–#4 sit
     completely unpolled (despite `ToolExecutionStart` already having been emitted) until the user answers call #1.
  2. On mid-batch cancellation, the sequential branch `break`s out of the loop (omitting `ToolResultMessage`s for
     remaining calls until `prepare_for_model` patches them later), whereas the parallel branch emits immediate
     `"Operation aborted"` `ToolResultMessage`s for every remaining call.
- **Optimization**:
  - Run schema validation and `Guard` synchronously up front, emit `ToolExecutionStart` when execution actually begins,
    and allow un-gated parallel tool calls to execute concurrently without being blocked behind another call's human
    approval wait (while serializing multiple approval prompts cleanly if desired). Make sequential and parallel abort
    bookkeeping symmetric.

---

### 2.4 Align standalone (`Link::local`) and daemon behavior, and refresh trusted folder config on open threads (`A2`, `R5`)

- **Where**: `crates/nypi-cli/src/main.rs:779, 848, 880-915`, `crates/nypi-hub/src/daemon.rs:617, 865-883, 1055-1090`.
- **Seams**:
  1. Standalone `RuntimeOptions` sets `elsewhere: false` (`main.rs:779`), hiding concurrent daemon work from the prompt.
  2. Standalone bails hard on an unknown `chat.model` (`main.rs:848`), whereas the daemon logs a warning and degrades to
     worker-only (`daemon.rs:617`).
  3. If a thread is already open in the daemon (especially the home thread, which never closes) when the user runs
     `nypi trust yes` or edits `.nypi/settings.json` / `.nypi/skills` / `.nypi/specialists`, the open `Thread` never
     reloads them until the daemon restarts.
- **Optimization**:
  - Enable `elsewhere: true` in standalone runs as well (reading the shared ledger is cheap via `Folded`).
  - Reload folder trust, skills, and specialist rosters before each run (alongside `refresh_mcp_tools` and
    `refresh_system_prompt` in `runtime/run.rs:173-174`), or invalidate/reopen a non-busy cached `Thread` when
    `trust.json` or `.nypi/settings.json` mtime changes.

---

## Part 3: Macro Architectural Evolution (Realizing the Full Chief-of-Staff Vision)

### 3.1 Decouple Lucia's Conversation from `Place` (One Continuous Self vs. Folder-Sharded Rooms) (`R1`, `B2`, `B4`, `D66`)

- **Structural tension**:
  Today, `Thread` equates **a conversation** with **a working folder** (`HashMap<PathBuf, Arc<Thread>>`), and `chat.rs`
  stores its conversation history only in an in-memory `Vec<Message>` (`chat.rs:168, 343`), which vanishes whenever the
  daemon restarts or `Threads::sweep` closes an idle folder thread after 30 minutes. Talking to Lucia in `~/proj-a`,
  `~/proj-b`, and Telegram feels like talking to three separate instances peering at each other through an 8-line
  `<elsewhere>` keyhole. Furthermore, `Runtime` binds to a single `Place`, preventing one task from naturally using two
  nodes (e.g., editing on the laptop and reading a verification message on the phone, `devices.md` §5).
- **Macro design**:
  1. **Persist `Chat` history** in its own lightweight rolling/compacted session file so daemon restarts and idle sweeps
     never erase conversational continuity.
  2. **Separate `Conversation` from `Place`**: Treat the home/primary conversation as Lucia's continuous Chief-of-Staff
     stream (shared by `nypi attach`, `nypi send`, Telegram, and mobile), while keeping per-folder threads as explicit
     workspace scopes that share memory, portrait, goals, and task visibility.
  3. **Multi-place tool routing**: Allow `Place` in the hub to route device calls to a default `(device, folder)` while
     exposing connected peer devices (`devices` prompt section from `Nodes::list()`, `devices.md` §8) so Lucia can
     target another connected node explicitly when a task spans devices, or hand folder-bound work to a background
     specialist at `(device, folder)`.

---

### 3.2 Complete the Pulse-Driven Cognitive Loop & Separation of Orchestration from Execution (`R1`, `R3`, `A6`, `B2`, `B6`)

- **Structural tension**:
  1. **Silent `Chat` on background completion**: When a background worker run or delegated specialist settles, the front
     end prints the raw worker output (`D19`), and `Chat` only learns about it passively via a `[task result]` note on
     the user's *next* message (`chat.rs:320-321`). Lucia never proactively speaks up in her conversational voice when
     background work finishes or blocks.
  2. **Single-lane `Worker` bottleneck**: The same `Worker` loop acts as both Chief-of-Staff orchestrator (`goals`,
     `schedule`, `delegate`, reviewing specialist reports) and hands-on executor (`bash`, `edit`). Long hands-on work in
     a thread blocks orchestration, scheduled prompts, and specialist report reviews in that thread.
  3. **Ad-hoc waiting via `schedule`**: `TaskState::Waiting` currently tracks schedule IDs and `"d-"` delegation IDs as
     strings, without general multi-task join semantics.
- **Macro design**:
  1. **Proactive `Chat` turn on `RunSettled` / `Reported` (preserving `D19`)**: Keep the worker's verbatim report and
     verified `[tN]` citations visible as the artifact of record, while letting `Chat` react to `RunSettled` when
     appropriate (e.g., concise framing, surfacing next decisions, or answering queued context).
  2. **First-class `WaitingOn` dependencies in `TaskLedger`**: Generalize `Waiting` to structured predicates
     (`WaitingOn { tasks: Vec<TaskId>, delegations: Vec<String>, schedules: Vec<String> }`) so a parent task can fan out
     to multiple background specialists/nodes and resume deterministically once all children settle.

---

### 3.3 One Hub-Level `BoardLink` & Devices on the System Map (`A1`, `A2`, `D56`, `D65`, `D66`)

- **Current seam**:
  Every `Runtime` spawns its own `BoardLink` client (`crates/nypi-core/src/runtime/mod.rs:736-748`). When the daemon has
  4 threads open, the Board displays 4 separate `"daemon"` processes sharing the same PID, and `chat_loop.rs:58-74`
  temporarily mutates `runtime.set_channel(origin)` during sourced runs (causing `BoardLink` to re-emit `Hello` changing
  the process's `front_end` label mid-flight). Meanwhile, `Nodes` (`devices.json`) is not yet represented on the Board's
  System map (`crates/nypi-cli/src/board/topology.rs:1008-1011`).
- **Macro design**:
  - Follow `devices.md` §§11 & 13: give the hub **one** `BoardLink` that reports its open threads, background jobs, and
    the live `Nodes` register (`devices`), while standalone processes keep their per-process `BoardLink`.
  - Add the `Devices` region (or nodes under `Resources` / `Always on`) to `topology.rs` and `nypi-board-ui`, removing
    the temporary `place.rs` / `device.rs` exemption comments in `topology.rs:1008-1011`.

---

### 3.4 Durable Node Journal, Call Leases, and Checkpointed Stores (`A4`, `A6`, `B6`, `D66`, `D67`)

1. **Node-side durable call journal & reattach (`devices.md` §7)**:
   - Currently `Node::seen` (`crates/nypi-node/src/lib.rs:103`) is an unbounded in-memory `HashSet<String>` lost on
     restart, and `link::serve` (`crates/nypi-node/src/link.rs:54`) cancels all running calls the instant a socket drops.
   - Persist a bounded per-node call journal (`~/.nypi/node/journal.jsonl`) recording `received`, `finished`/`refused`
     results (retained until `Ack` from the hub), and renew per-call leases on heartbeats so a brief Wi-Fi/SSH or phone
     sleep flap never kills a running build or loses its result. Cap JSONL frame sizes on `nodes.sock` (as `bridge.sock`
     already caps lines at 64 KiB).

2. **Snapshots & derived indexes alongside append-only JSONL (`jsonl.rs`, `memory.rs`, `audit_log.rs`)**:
   - Keep append-only JSONL files as the canonical, human-readable source of truth, and add:
     - Periodic offset-stamped snapshot files for `Folded<S>` (`ledger`, `memory`, `outbox`, `schedules`) plus bounded
       compaction/archival of ancient closed ledger/outbox entries so cold start stays `O(recent)` instead of
       `O(years)`.
     - Folded or indexed `Portrait` and `PassLog`, and a lightweight rebuildable index over `audit/*.jsonl` so multi-day
       `nypi trace` and multi-week `nypi scorecard` do not linearly re-parse tens of thousands of raw JSON lines.
     - Serialization/batching around `LocalEmbedder` (`crates/nypi-ai/src/embeddings/local.rs:239-243`) via a semaphore
       and true batch forward pass so startup `backfill` (200 memories) does not contend with live `MemoryValve` queries
       on CPU/Metal.

---

### 3.5 Defense-in-Depth Hardening Across Providers, Subprocesses, and Linux (`R5`, `B6`)

1. **Collapse two-layer HTTP retry multiplication & blind 429 quota sleeps**:
   - `post_json` (`crates/nypi-ai/src/provider.rs:117-164`) retries HTTP `408/409/429/5xx` up to 3 times *before*
     returning the body, and `RetryState` (`crates/nypi-ai/src/retry.rs:76-141`) retries up to 3 times again—yielding up
     to `(1+3)*(1+3) = 16` attempts on 5xx, and sleeping through 3 retries in `post_json` on HTTP `429` billing/quota
     errors (`insufficient_quota`) before `NON_RETRYABLE` ever inspects the body. Inspect the error body in `post_json`
     (or delegate status retries to the single `RetryPolicy` layer with `Retry-After` propagated on the error).
2. **Clean-room subprocess environment & external specialist / MCP sandboxing**:
   - Scrub provider API keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `TELEGRAM_BOT_TOKEN`, etc.) from
     inherited subprocess environments after capturing them into NyPi's private auth lookup (`auth.rs`), unless
     explicitly allowlisted for an external specialist CLI.
   - Redact `api_key` in custom `Debug` impls on `StreamOptions` (`crates/nypi-ai/src/provider.rs:16`) and
     `RemoteEmbedder` (`crates/nypi-ai/src/embeddings/remote.rs:8`).
   - Pin a SHA-256 hash and commit rev for `tokenizer.json` in `crates/nypi-ai/src/embeddings/local.rs:115` (matching
     the `.gguf` verification), and guard empty `read` rules in `Sandbox::profile` (`sandbox.rs:66`).
   - Add Linux Landlock / bubblewrap parity alongside macOS `sandbox-exec`.
3. **In-process peer ancestry & `bridge.sock` peer check**:
   - Replace the up-to-64 `/bin/ps` subprocess spawns per socket connect in `daemon.rs:533-549` with `proc_pidinfo` on
     macOS (`/proc/<pid>/stat` on Linux), apply `started_by_this_process` to `bridge.sock` (`crates/nypi-hub/src/bridge.rs:85`)
     as well, and note that OS sandboxing (blocking `run/*.sock` at the kernel level) remains essential because
     double-forked processes reparent to PID 1.

---

## Recommended Implementation Order

Execute in small, self-verifying commits (`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test --workspace` green after each, updating `docs/architecture.md` and `docs/roadmap.md` per `AGENTS.md`):

1. **Step 1 — `Protected` & tool path hardening (Part 1.1)**: Component-wise symlink-resolving `real_path` (including
   dangling symlinks), `resolve_read_path` in `Guard`, in-tool `Protected` checks, skill-dir protection, and boundary
   tests.
2. **Step 2 — Unified approval IDs & Board resolution events (Part 1.2)**: Single `request.id` across `Inspector`,
   `BoardLink`, `rpc`, `chat_loop`, `daemon`, `tui`, and `print`.
3. **Step 3 — Finish & harden D68 remote places (Part 1.3)**: Typed `(device, folder)` keys in `Threads`, `Ledger`, and
   `Schedule`; `prepare` forwarding; remote `home_dir`/`os` in `Report` for `Policy` and `prompt`; fast stale-link
   replacement on reconnect.
4. **Step 4 — Typed internal hub protocol, `chat_loop` parity, & `daemon.rs` split (Part 2.1)**: Shared control/session
   commands, reflection events and `/followup` in chat mode, typed hub records/commands, and modular `daemon/`.
5. **Step 5 — Stuck-run progress ceilings, parallel `prepare()` unblocking, & standalone/trust parity (Parts 2.2–2.4, 3.5)**.
6. **Step 6 — Hub-level `BoardLink`, `Devices` on the System map, and durable node journal (Parts 3.3–3.4)**.
7. **Step 7 — Persistent `Chat` session, conversation/place decoupling, and structured `WaitingOn` task joins (Parts 3.1–3.2)**.
