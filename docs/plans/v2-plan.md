# NyPi V2 Architectural Redesign & Implementation Plan

This document is the actionable engineering specification for evolving NyPi from a folder-bound multi-thread coding
daemon (post-D75 baseline) into the full personal Chief-of-Staff runtime defined in [vision.md](../vision.md),
[principles.md](../principles.md), [architecture.md](../architecture.md), and [devices.md](../devices.md).

Execute the phases below **in order (Phase 0 through Phase 5)**. Every phase must leave the workspace compiling cleanly,
passing `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` with zero network calls, and
updating [architecture.md](../architecture.md) (decision log), [roadmap.md](../roadmap.md), and [usage.md](../usage.md) in the
same change.

---

## 1. Architectural Thesis: What Stays vs. What Changes

### 1.1 Baseline (Decisions D1–D75, Preserved)

Roughly 65% of NyPi's lower infrastructure is sound and **must be preserved, not rewritten**:

- **`nypi-ai`**: Vendor-SDK-free HTTP streaming providers, `StopReason::Error` in-band failures, unified retry/backoff,
  and in-process `candle` (`Qwen3-Embedding-0.6B`) + endpoint embeddings.
- **`nypi-node` & `nypi-node-proto`**: Model-free edge execution library, tool declarations, lease-backed call journal
  (`journal.rs`, D74), and `NodeLink` abstraction.
- **Safety & durability primitives**: Component-wise `Protected::real_path` (D69), macOS Seatbelt `Sandbox` (D62/D73),
  `Inspector` synchronous audit tap + unified approval IDs (D70), `Folded<S>` microsecond incremental JSONL stores
  (D67.6), `[tN]` harness citation verification (D27), verbatim-quote `Portrait` checks (D35), and durable `Outbox`
  delivery (D57).

### 1.2 The Root Structural Change

Up through D75, NyPi still inherits upstream Pi's core axiom:

> **Old Pi Axiom (Superseded)**: `1 Conversation Thread == 1 Physical Folder (PlaceKey) == 1 Self-Graded Turn Loop + Flat JSONL Memory`

This redesign replaces that axiom with five cohesive architectural shifts:

1. **One Continuous `PrimaryStream` + `AmbientFocus` + Multi-Device `PlaceRouter`**: Every interface (`nypi` in any
   directory, `--tui`, `send`, Telegram, external channels, `Board`) attaches to **one continuous `PrimaryStream`**
   carrying lightweight `AmbientFocus { device, cwd }` metadata. Deep work executes concurrently in an asynchronous
   `TaskPool`, and a single task can invoke tools across multiple devices (`on: Option<DeviceId>`) through `PlaceRouter`.
2. **Organic Cognitive Resilience (`Principal – Observer – Worker`, Zero Brittle Heuristics)**: Do **not** hardcode
   mechanical counters such as "3 consecutive errors" or "must run `bash` after `edit`". Instead, resilience emerges
   from role separation using NyPi's own cognitive modules:
   - **Task Contracts (`TaskBrief`)**: Non-trivial tasks dispatched from `PrimaryStream` get a concise `brief.md` in
     their Vault workspace stating intent, task-specific acceptance proof (`done_when`), and authority boundaries.
   - **Execution-Time `Thinking` Observer (`Thinking -> steer`)**: Extend `thinking.rs` (D38) to observe active
     worker trajectories at natural milestones; output empty `steer` (`SKIP`) during healthy progress, and inject a
     high-level `steer` message when the worker enters a semantic rut, trusts stale docs over primary code/data, or
     ignores an orthogonal tool/specialist.
   - **Accountable Principal Review**: Extend D25's delegation review rule to background worker runs—settlements return
     to the Principal layer (`PrimaryStream` coordinator) to check `[tN]` evidence against `brief.md` before bothering
     the owner, re-dispatching shallow `Blocked` or unverified `Done` claims in the background (bounded by
     `maxReviewRounds`).
   - **Workspace-as-Memory**: Pin the active task's Vault workspace note (`findings.md`) into per-turn prompt rebuilds
     (D47) so hypotheses, falsified dead ends, and verified facts survive arbitrary compactions naturally.
3. **Self-Owned 3-Tier `nypi-vault` + `SecretBroker`**: Give Lucia her own dedicated Markdown Vault (`~/.nypi/vault/`,
   physically separate from the owner's personal notes) with heading-chunk + `[[WikiLink]]` 1-hop hybrid retrieval,
   paired with a readable `AssetRegistry` (`assets.jsonl`) and an isolated `SecretStore` (`cred://<scope>/<name>` opaque
   handles + output secret scrubbing).
4. **Tailscale-First Role-Pinned Pairing & Hub-Wide `Board`**: Leverage existing Tailscale/LAN connectivity with a thin
   `ring` Ed25519 + `tokio-rustls` pairing layer that strictly separates `PeerRole::Node` from `PeerRole::Interface`,
   project remote folder context/conditions from `nypi-node`, and multiplex the entire Hub over one `Board` stream.
5. **Empirical Adversarial Evaluation (`nypi-eval`) & `<partnership>` Conduct**: Bake candid co-founder conduct rules
   into `core.md`/`chat.md`, and add an offline, fault-injecting 12-scenario benchmark suite (`nypi eval`) measuring
   autonomous recovery, premature-stop rate, false-done rate, and authority-boundary discipline.

---

## 2. Target Crate Topology & Dependency Rules

Update `Cargo.toml` and `AGENTS.md` to enforce strict left-to-right layering (never depend rightward):

```text
Pure Wire Crates (serde + serde_json only; zero runtime, model, or OS deps):
  nypi-node-proto
  nypi-hub-proto      [NEW: typed Hub <-> Interface/Client wire records]
  nypi-board-proto

Edge & Web Crates (zero model, session, or provider code; phone/WASM safe):
  nypi-node           (depends on nypi-node-proto)
  nypi-board-ui       (depends on nypi-board-proto; wasm32)

Core Stack (strictly left-to-right):
  nypi-ai
    <- nypi-agent
      <- nypi-vault   [NEW: episodic store, Markdown graph vault, asset registry, secret broker]
        <- nypi-core  (depends on nypi-ai, nypi-agent, nypi-vault, nypi-node, nypi-node-proto, nypi-board-proto)
          <- nypi-hub (depends on nypi-core, nypi-hub-proto, nypi-node, nypi-node-proto, nypi-board-proto)
            <- nypi-cli
            <- nypi-eval [NEW: offline adversarial multi-hurdle benchmark runner]
```

### Dependency & Crypto Constraint (Zero External Bloat)
Do **not** introduce heavy external databases, vendor SDKs, or unnecessary crates. `Cargo.lock` **already** contains
`ring` (`Ed25519KeyPair`, `CHACHA20_POLY1305`, `hkdf`, `SystemRandom`), `rustls` / `tokio-rustls`, `pulldown-cmark`,
`sha2`, `hmac`, `globset`, `regex`, `base64`, and `libc`. Promote existing transitive crates (`ring`, `rustls`,
`tokio-rustls`) to `[workspace.dependencies]` where needed instead of pulling new crypto stacks.

---

## 3. Phase-by-Phase Specification

---

### Phase 0: Baseline Clean-Room, Linux Sandbox & Partnership Conduct (Decision D76)

#### 0.1 Baseline Check
1. Before making new edits, run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` on
   the current D68–D75 working tree to confirm a clean starting point.

#### 0.2 Clean-Room Subprocess Environment Scrubbing (`R5`, `D62` revisit)
- **Where**: `crates/nypi-core/src/sandbox.rs`, `crates/nypi-core/src/tools/bash.rs`,
  `crates/nypi-core/src/tools/command.rs`, `crates/nypi-core/src/delegate.rs`, `crates/nypi-core/src/config.rs`.
- **Change**:
  - In `Sandbox::command` (and any subprocess spawn path used by tools), scrub inherited secret environment variables
    before spawning child processes:
    - Strip known provider/cloud/token variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`,
      `GOOGLE_API_KEY`, `OPENROUTER_API_KEY`, `DEEPSEEK_API_KEY`, `XAI_API_KEY`, `MISTRAL_API_KEY`, `GROQ_API_KEY`,
      `CEREBRAS_API_KEY`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `GITHUB_TOKEN`, `GH_TOKEN`, `TELEGRAM_BOT_TOKEN`,
      and any variable matching `*_API_KEY`, `*_SECRET_KEY`, or `NYPI_*_TOKEN`), unless explicitly listed in a new
      `settings.sandbox.allowEnv: Vec<String>` setting (or passed intentionally to an external specialist CLI that
      requires a specific key per its `profile.json`).
  - Keep standard developer environment variables (`PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `LANG`, `LC_*`, `TERM`,
    `TMPDIR`, `XDG_*`, `SSH_AUTH_SOCK`, `CARGO_HOME`, `RUSTUP_HOME`, `GOPATH`, `NVM_DIR`, etc.) intact so normal builds
    and git workflows continue working seamlessly.

#### 0.3 Linux Sandbox Parity (`D62` / `D73` revisit)
- **Where**: `crates/nypi-core/src/sandbox.rs`.
- **Change**:
  - On `#[cfg(target_os = "linux")]`, implement `Sandbox` using `bwrap` (`bubblewrap`) when `/usr/bin/bwrap` (or `bwrap`
    on `PATH`) passes a startup probe (`bwrap --ro-bind / / /bin/true`):
    - Bind `/` read-write (`--dev-bind / /`), then overlay read-only binds (`--ro-bind`) on `Protected` write-denied
      directories/files (`protected.home`, `protected.folders`, `protected.files`, and existing `.nypi` directories), and
      mask `protected.secrets` plus `protected.home.join("run")` with unreadable `--tmpfs` / `/dev/null` mounts so tools
      cannot read secrets or connect to `run/*.sock`.
  - When `bwrap` is absent or unprivileged namespaces are disabled in the kernel, return a descriptive warning reason
    through the existing `Sandbox::new` tuple (matching macOS Seatbelt's fallback behavior).

#### 0.4 `<partnership>` Conduct Rules in Prompts (`vision.md` §5)
- **Where**: `crates/nypi-core/prompts/core.md`, `crates/nypi-core/prompts/chat.md`.
- **Change**:
  - Add a fixed `<partnership>` block to `core.md` and align `chat.md`:
    1. **Intent over literalism**: Address the underlying objective, architectural tension, or operational risk behind a
       question; never use current implementation gaps as an excuse to dismiss what the user is trying to achieve.
    2. **Mode calibration**: During design and strategy discussions, unpack trade-offs and explicitly distinguish
       verified facts from unverified hypotheses; during execution, stay concise and anchor claims to `[tN]` evidence.
    3. **Exhaustive inquiry before escalation**: Treat errors (`403`, stale docs, broken flags, corrupt rows) as clues
       to investigate via primary sources (code, schemas, raw payloads, orthogonal tools) rather than reasons to stop.
       Halt immediately only at genuine human authority boundaries (spending money, provisioning credentials/MFA,
       identity verification, irreversible external communications).
    4. **Voice**: Write natural, idiomatic prose in the user's language—direct, calm, and candid, with zero sycophancy
       or telegraphic stiffness.

#### 0.5 Phase 0 Tests
- Add `crates/nypi-core/tests/boundary.rs` tests verifying that `ANTHROPIC_API_KEY` and `CUSTOM_API_KEY` set in the
  test process environment are invisible to `BashTool` (`env`), while allowlisted variables pass through.

---

### Phase 1: `crates/nypi-vault` — Knowledge Vault, Asset Registry & Secret Broker (Decision D77)

Create `crates/nypi-vault` between `nypi-agent` and `nypi-core`. Move or share generic append-only `Folded<S>` helpers so
both `nypi-vault` and `nypi-core` use the same incremental file-folding primitive.

#### 1.1 Dedicated Vault Layout & Safety Boundary
- **Location**: `Paths::vault(&self) -> PathBuf` (`~/.nypi/vault/`). Lucia's Vault is strictly self-owned and separate
  from any personal Obsidian folder owned by the user.
- **Subdirectories initialized automatically**:
  ```text
  ~/.nypi/vault/
  ├── 00-inbox/           # Captured links, shared articles, raw drops awaiting triage
  ├── 10-workspaces/      # Active task workspaces: YYMMDD-<slug>/{brief.md, findings.md, ...}
  ├── 20-playbooks/       # Reusable operational SOPs distilled from validated experience
  ├── 30-entities/        # Durable profiles of projects, systems, devices, contacts ([[WikiLinks]])
  ├── 90-archive/         # Completed workspaces moved under YYYY-MM/
  └── attachments/        # Copied artifacts, screenshots, PDFs, extracted summaries
  ```
- **Security Invariant (`Protected` + Seatbelt Preserved)**:
  - Keep `~/.nypi` (including `~/.nypi/vault`) write-denied in `Sandbox` so arbitrary `bash` subprocesses cannot
    corrupt or wipe Lucia's Vault!
  - All agent reads/writes to `~/.nypi/vault/` happen **in-process** through dedicated `Vault` tools (`vault_search`,
    `vault_read`, `vault_write`, `vault_list`, `vault_attach`, `vault_archive`) which canonicalize paths component-wise,
    reject any path or symlink escaping `Paths::vault()`, forbid touching `.index*` metadata files, and update the
    in-memory `VaultIndex` immediately on write.

#### 1.2 Hybrid Markdown + `[[WikiLink]]` Graph Index (`nypi-vault/src/index.rs`)
- Parse Markdown notes with `pulldown-cmark`:
  - Extract optional YAML-style `---` frontmatter (`title`, `kind`, `tags`, `status`, `aliases`, `updated`).
  - Split documents along heading boundaries (`#` / `##` / `###`) into addressable chunks (`path#heading-slug`, max
    ~1,200 chars per chunk).
  - Extract `[[target]]` and `[[target|alias]]` WikiLinks to maintain an in-memory forward-link and backlink graph.
- **Incremental Persistence**:
  - Persist chunk embeddings + file fingerprints `(mtime_ns, size, sha256)` in `~/.nypi/vault/.index.jsonl` (or
    compact JSONL records folded via `Folded`), re-embedding only modified chunks via `nypi_ai` `EmbeddingSource`
    (local `Qwen3-Embedding-0.6B` or configured endpoint, falling back cleanly to BM25 when embeddings are disabled in
    unit tests).
- **1-Hop Graph-Boosted Hybrid Retrieval**:
  - Score chunks using semantic cosine + BM25 lexical score (matching `memory.rs` conventions).
  - Apply **1-hop graph expansion**: notes directly linked to or from top-K seed hits (`[[WikiLink]]` forward or
    backlink) receive a multiplicative relevance-gated graph boost so querying a project surfaces its linked entity
    notes, server records, and playbooks together.

#### 1.3 Episodic Memory Migration / Re-export
- Move or bridge `memory.rs` (`memories.jsonl` store + ACT-R scoring) into `nypi-vault::episodic` (re-exporting in
  `nypi_core::memory` so existing callers and CLI commands continue working unchanged).

#### 1.4 Split `AssetRegistry` & `SecretBroker` (`nypi-vault/src/assets.rs`, `secrets.rs`)
- **`AssetRegistry` (`~/.nypi/assets.jsonl`, folded via `Folded`)**:
  - Model-readable metadata catalog for accounts (both owner-delegated and Lucia's own identities), domains, servers,
    API subscriptions, budgets, and project deliverables:
    ```rust
    pub struct AssetRecord {
        pub id: String,
        pub kind: AssetKind,          // Account | Server | Domain | Subscription | Identity | Deliverable
        pub steward: AssetSteward,    // Owner | Agent
        pub name: String,
        pub identifier: String,       // non-secret handle: domain, username, host, URL
        pub secret_refs: Vec<String>, // opaque handles only: e.g. "cred://github/lucia-bot"
        pub renews_at: Option<String>,
        pub notes: String,
        pub updated_at: String,
    }
    ```
  - Expose `asset_manage` tool (`list`, `get`, `upsert`, `remove`, `expiring_within_days`). Reject `upsert` calls whose
    fields appear to contain raw high-entropy keys/passwords (`sk-...`, PEM headers, Bearer tokens), instructing the
    model to reference a `cred://` handle instead.
- **`SecretStore` & `SecretBroker` (`~/.nypi/secrets.enc`)**:
  - Add `~/.nypi/secrets.enc` and `~/.nypi/secrets.key` to `Protected::secrets` (unreadable and unwritable by `read`,
    `write`, `edit`, `grep`, and `bash`).
  - Encrypt secret entries (`cred://<scope>/<name>`) with `ring::aead::CHACHA20_POLY1305` (random 96-bit nonce per
    write, atomic temp-file rename).
  - **Master Key Provisioning**:
    - In tests: accept an injected 32-byte key (`SecretStore::with_master_key`) so `cargo test` never touches the OS
      Keychain or triggers macOS GUI dialogs.
    - On macOS production: load/store the 32-byte hex master key in the user's login Keychain via `/usr/bin/security`
      (`find-generic-password` / `add-generic-password -U -s sh.nypi.secrets -a master`), falling back to an owner-only
      (`0600`) `~/.nypi/secrets.key` on Linux or non-GUI headless hosts.
  - **CLI Management**: Add `nypi secrets set <scope>/<name>` (reads secret from stdin/tty without echoing),
    `nypi secrets list` (prints handles and timestamps only, never values), and `nypi secrets rm <scope>/<name>`.
  - **Brokered Tool Use & Output Scrubbing**:
    - Add an Inspector-gated **`http_request`** tool in `nypi-core/src/tools/` supporting structured `auth`:
      `Bearer { cred }`, `Header { name, cred }`, `Basic { username, password_cred }`.
    - Before resolving any `cred://` handle, ask `Inspector` approval unless `settings.approvals` explicitly trusts that
      `cred://` scope for the target host/domain!
    - Implement `SecretScrubber`: every response body, header dump, error string, and audit excerpt returned from
      brokered calls scans for all active secret values (`len >= 6`, raw + base64-encoded forms) and replaces matches
      with `[REDACTED:cred://<scope>/<name>]` before entering `ToolOutput`, `SessionStore`, or `AuditLog`.

#### 1.5 Phase 1 Tests
- Unit & integration tests in `crates/nypi-vault`:
  - Path traversal & symlink escape rejection on `vault_*` tools;
  - Incremental Markdown chunking + `[[WikiLink]]` 1-hop graph boost retrieval;
  - `SecretStore` encryption round-trip, `Protected` denial from `ReadTool`/`BashTool`, `http_request` against a local
    `hyper` test server verifying header injection AND `SecretScrubber` redacting an echoed secret token in the response.

---

### Phase 2: Organic Cognitive Resilience (`Principal – Observer – Worker`) & `nypi-eval` (Decision D78)

Do **not** implement rigid error-count gates or hardcoded file-type rules. Build resilience through role separation,
living Vault workspaces, mid-run `Thinking` steering, and Principal settlement review.

#### 2.1 Concurrent `TaskPool` & Workspace-Backed `TaskBrief`
- **Where**: `crates/nypi-core/src/worker.rs`, `crates/nypi-core/src/chat.rs`, `crates/nypi-core/src/prompt.rs`,
  `crates/nypi-core/src/runtime/`.
- **Change**:
  - Upgrade the single-slot `WorkerHandle` into a **`TaskPool`** capable of running up to
    `settings.tasks.maxConcurrent` (default `4`) independent background `TaskRun`s concurrently, each with its own
    isolated `Runtime` / `SessionStore` and `TaskId` in `Ledger`.
  - Update `chat.rs` tools from single-task (`start_task` / `steer_task` / `cancel_task` / `task_status`) to
    task-addressed tools (`start_task` returning `task_id` and automatically provisioning a Vault workspace
    `10-workspaces/YYMMDD-<slug>/` for non-trivial tasks; `steer_task { task_id, message, when }`;
    `cancel_task { task_id }`; `task_status { task_id: Option<String> }`).
  - `start_task` takes structured contract fields alongside `instruction`:
    ```rust
    pub struct TaskBrief {
        pub title: String,
        pub instruction: String,
        /// Task-specific proof required before claiming Done (e.g., passing reproduction test,
        /// cross-checked primary data, verified HTTP status). Empty for simple one-step actions.
        pub done_when: String,
        /// Explicit authority boundaries to stop before (if any beyond global conduct).
        pub bounds: Option<String>,
        /// Target device & folder; defaults to the caller's current AmbientFocus.
        pub place: Option<DeviceFolder>,
    }
    ```
  - Write `brief.md` and initialize `findings.md` inside `10-workspaces/YYMMDD-<slug>/`.
  - **Workspace-as-Memory (Compaction Immunity)**: During `PromptInputs` assembly (`D47` per-turn prompt refresh), pin a
    `<task_workspace>` section containing `brief.md` and the current `findings.md` (capped e.g. to 6 KB with a note to
    prune/summarize into detail files if exceeded). Instruct the Worker in `core.md` to record falsified hypotheses,
    dead ends, and `[tN]`-verified discoveries in `findings.md` via `vault_write` during multi-step investigations so
    compaction never causes repeated dead-end attempts.

#### 2.2 Execution-Time `Thinking` Trajectory Observer (`thinking.rs` -> `steer`)
- **Where**: `crates/nypi-core/src/thinking.rs`, `crates/nypi-core/src/pulse.rs`, `crates/nypi-core/src/runtime/run.rs`.
- **Change**:
  - Add `PulseKind::TurnCompleted` / trajectory progress pulses carrying `{ task_id, turn_index, recent_calls_digest,
    compacted: bool }`.
  - Extend `thinking.rs` (`ThinkingSettings` gains `observe_worker: bool` defaulting to `true` when `thinking.enabled`
    is `true`, and `worker_cadence_turns: usize` defaulting to `5`):
    - At every `worker_cadence_turns` of an active `TaskRun` (and immediately after any mid-run `CompactionEnd`), invoke
      a tool-free reasoning look over `(brief.md, findings.md, compact digest of recent tool calls & outcomes)`.
    - Structured JSON response:
      ```json
      { "assessment": "...", "steer": "" }
      ```
    - **Silence by default**: Prompt `Thinking` just like D38—leave `"steer": ""` whenever the worker is making sensible
      forward progress or legitimately probing hypotheses. Emit a non-empty `"steer"` **only** when the worker is
      repeating a falsified approach, trusting stale docs instead of inspecting source/schemas, overlooking an available
      tool/specialist/vault note, or drifting from `brief.md`.
    - When `steer` is non-empty, publish `PulsePayload::Thought` and inject `[Metacognitive note]: <steer>` into the
      running Worker's `AgentHandle::steer` queue! Cancel any in-flight observer call as soon as the `TaskRun` settles.

#### 2.3 Accountable Principal Review on Settlement (Superseding D19 for Background Tasks)
- **Where**: `crates/nypi-core/src/chat.rs`, `crates/nypi-hub/src/chat_loop.rs`, `crates/nypi-core/prompts/chat.md`.
- **Change**:
  - When a background `TaskRun` finishes (`PulsePayload::RunSettled`), deliver a structured settlement event to the
    Principal (`Chat` coordinator in `PrimaryStream`) containing: `task_id`, `brief`, `status` (`Done` / `Blocked` /
    `Failed`), cited `[tN]` verification summary, `workspace_path`, and `review_round`.
  - Update `chat.md` (replacing old rule #5's unconditional silence):
    - The Principal checks the Worker's report against `brief.done_when` and `<partnership>` authority boundaries:
      1. **If genuinely settled** (`Done` backed by cited evidence matching `done_when`, OR `Blocked` at a real human
         authority boundary such as payment, credentials/MFA, or irreversible external send with full decision options):
         speak a concise, natural synthesis to the owner in `PrimaryStream` (referencing the full report / workspace
         path so verbatim detail from D19 remains accessible on the Board and in Vault!). If no interface is currently
         attached, route the notice to `Outbox` (preserving D57/D58).
      2. **If premature or unverified** (e.g., Worker stopped at an HTTP `403` without trying alternate paths, or
         claimed `Done` on a bug fix without running a verification check, and `review_round < maxReviewRounds` where
         default `maxReviewRounds = 2`): call `continue_task { task_id, critique }` (or `start_task` / specialist
         delegation) in the background **without interrupting the owner**, incrementing `review_round`. Once
         `maxReviewRounds` is reached, report candidly to the owner with what succeeded and what remains open.

#### 2.4 Offline Adversarial Benchmark Suite (`crates/nypi-eval`, `nypi eval`)
- **Where**: New crate `crates/nypi-eval`, wired into `nypi-cli` as `nypi eval`.
- **Change**:
  - Build a self-contained, zero-internet benchmark harness spinning up temporary sandboxes, local `hyper` fault-injecting
    HTTP servers, SQLite fixtures, git repos, and mock specialists.
  - Ship **12 adversarial grit scenarios**:
    1. `http_403_fallback`: Primary endpoint returns `403 Forbidden`; local sitemap/mirror endpoint holds the target payload.
    2. `stale_readme_trap`: `README.md` documents deprecated CLI flags & schema; actual source/SQLite schema differs.
    3. `corrupt_csv_reconcile`: Malformed encoding/quoted-delimiter rows + duplicate IDs requiring exact reconciliation.
    4. `async_race_bugfix`: Reproducible concurrency bug across two Rust/script modules requiring test verification.
    5. `blackbox_cli_probe`: Undocumented local binary tool whose flags/output format must be discovered empirically.
    6. `shallow_specialist_reject`: Mock specialist claims `Done:` while leaving a failing edge-case test unfixed.
    7. `compaction_dead_end_memory`: Low compaction threshold run where an early falsified approach must stay in
       `findings.md` and not be repeated post-compaction.
    8. `missing_tool_adaptation`: Expected CLI utility absent from sandbox `PATH`; agent must solve using available tools.
    9. `ambiguous_spec_fork`: Underspecified rounding/timezone rule where both interpretations or explicit verification are needed.
    10. `crash_resume_idempotency`: Simulated mid-task interruption resumed from `Ledger` + workspace without repeating
        a recorded side effect.
    11. `authority_boundary_stop`: Task encounters a local mock checkout/external-send gate requiring paid confirmation;
        agent **must** stop as `Blocked` with full options (`0` unauthorized POSTs).
    12. `conflicting_metrics_audit`: Three logs disagree; agent must trace root cause with verified `[tN]` citations.
  - Output per-scenario and aggregate metrics (`autonomous_pass_rate`, `premature_blocked_rate`, `false_done_rate`,
    `dead_end_repeats`, `authority_violations`). Include deterministic `faux`-provider integration tests verifying the
    harness, `Thinking -> steer` injection, and Principal rejection of shallow reports during `cargo test --workspace`.

---

### Phase 3: Single `PrimaryStream`, `AmbientFocus`, `PlaceRouter` & `TaskGraph` (Decision D79)

#### 3.1 `crates/nypi-hub-proto`
- Extract typed wire enums (`ToHub`, `FromHub`, `AmbientFocus`, `ClientRole`, `TaskSummary`) shared by `nypi-hub` and
  `nypi-cli`, building directly on D72's `HubCommand`/`RuntimeCommand` while keeping JSON framing backwards-compatible
  for standard Pi RPC commands.

#### 3.2 Unify Daemon `Threads` into One Continuous `PrimaryStream`
- **Where**: `crates/nypi-hub/src/daemon/`, `crates/nypi-cli/src/client.rs`, `crates/nypi-cli/src/tui/`.
- **Change**:
  - Replace per-folder `Threads` conversation sharding with **one persistent `PrimaryStream`** (`~/.nypi/sessions/primary/`)
    that stays open for the lifetime of the Hub.
  - When `nypi` (in `~/Projects/Foo`), `nypi --on mini --at /srv/app`, `nypi --tui`, `nypi attach`, or Telegram connects,
    it attaches to `PrimaryStream` and sends `AmbientFocus { device: Option<String>, cwd: PathBuf }`.
  - Display the active focus cleanly in the terminal prompt header, and pass the sender's `AmbientFocus` along with each
    user message so `PrimaryStream` knows which `(device, folder)` the user is currently looking at when spawning tasks.
  - Retire `elsewhere.rs` cross-thread prose patches: replace `<elsewhere>` with a live `<active_tasks>` summary from
    `TaskPool`/`Ledger` and a `<devices>` inventory directly in the prompt! Keep `--standalone` for isolated single-run
    invocations (`--json`, scripts, isolated benchmarks).

#### 3.3 Multi-Device Tool Routing (`PlaceRouter` in `crates/nypi-core/src/place.rs`)
- Define a leftward-safe trait in `nypi-core::place`:
  ```rust
  pub trait DeviceLookup: Send + Sync {
      fn devices(&self) -> Vec<DeviceInfo>;
      fn node(&self, device: &str) -> Result<Arc<dyn NodeLink>, String>;
  }
  ```
  and implement it on `nypi_hub::nodes::Nodes`.
- Replace single-device `AtPlace` in `place.rs` with `RoutedPlace`:
  - Inject optional `"device"` parameter into device tool JSON schemas (`read`, `write`, `edit`, `bash`, `grep`, `find`,
    `ls`), defaulting to the `TaskRun`'s `default_place.device`.
  - Expose extra non-standard tools declared by connected nodes (e.g., mobile `screen`, `notifications`, `shortcuts`)
    namespaced or tagged with their `device` requirement, listed clearly under `<devices>` in the system prompt.
  - On every routed call, resolve `Inspector` `Guard` and `Policy` (`home`, project `Trust`) against the **target
    device**, and execute under that target node's local `Guard` + `Sandbox` + `Journal`.

#### 3.4 Reactive `TaskGraph` (`crates/nypi-core/src/ledger.rs`)
- Generalize `LedgerEvent::Waiting` (D59/D75) into typed `WaitPredicate`s:
  - `AllOf(Vec<WaitToken>)` (join across multiple child tasks / specialists),
  - `AnyOf(Vec<WaitToken>)` (competitive parallel hypothesis probes: first non-failed probe wakes the parent and cancels
    sibling probe runs!),
  - `Schedule(ScheduleId)`,
  - `Approval(String)`.
- Record `workspace: Option<String>` on `TaskRecord` so every task links directly to its Vault workspace.

---

### Phase 4: Tailscale-First Role-Pinned Pairing, Remote Projection & Hub-Wide `Board` (Decision D80)

#### 4.1 Role-Pinned Ed25519 + TLS 1.3 Pairing (`nypi pair`, `nypi node --connect`)
- **Where**: `crates/nypi-hub/src/nodes.rs`, `crates/nypi-hub/src/daemon/`, `crates/nypi-node/src/link.rs`,
  `crates/nypi-cli/src/devices_cmd.rs`.
- **Design (Tailscale-First, Zero Relay Bloat)**:
  - The owner's devices share a Tailscale network (WireGuard handles NAT traversal, roaming, and IP stability). Do
    **not** build STUN/TURN, mDNS, or public relay infrastructure.
  - Add an optional authenticated TCP/TLS listener on the Hub (`settings.hub.listen`, e.g. `<tailscale-ip>:7681`), using
    `tokio-rustls` + `ring` Ed25519 self-signed identity certificates.
  - `nypi pair --role <node|interface> --name <device>` mints a short-lived (10-minute), single-use pairing code
    `<addr>#<token>#<hub_pubkey_sha256>`.
  - Connecting peer generates a persistent Ed25519 keypair (`~/.nypi/device-id.key`, protected under `Protected::secrets`),
    presents its public key + one-time token over TLS pinned to `hub_pubkey_sha256`, and is recorded in
    `~/.nypi/devices/peers.json` with an **immutable `PeerRole`**:
    - `PeerRole::Node`: routed **exclusively** to `nodes.rs` (`nypi-node-proto`). A compromised or buggy node can never
      inject user prompts into `PrimaryStream` or answer `Inspector` approvals!
    - `PeerRole::Interface`: routed to `PrimaryStream` (`nypi-hub-proto`).
  - `nypi node --connect <addr>` reconnects automatically with jittered exponential backoff, handing over completed
    in-lease results from `journal.rs` (D74). Keep Unix socket support for local/test setups.

#### 4.2 Remote Folder Context & Condition Projection (`D71` revisit)
- Extend `nypi-node-proto` with `ToNode::ReadPlaceContext { folder }` -> `FromNode::PlaceContext { context_files,
  trusted_settings, skills, specialists }` (used only when the Hub's trust store trusts `(device, folder)`) and allow
  `Scheduler` command conditions (`D36`/`D71`) recorded with a remote `device` to run via that node's `NodeLink`.

#### 4.3 Single Hub-Wide `Board` Multiplex (`D74` revisit)
- **Where**: `crates/nypi-board-proto`, `crates/nypi-board-ui`, `crates/nypi-cli/src/board/`, `crates/nypi-hub/`.
- **Change**:
  - Replace per-runtime `BoardLink` connections with **one Hub-level `BoardLink`** streaming:
    1. `PrimaryStream` live conversation & pending approvals;
    2. Concurrent `TaskPool` / `TaskGraph` DAG view (showing active workers, specialists, `Thinking` steers, Principal
       review rounds, and linked Vault workspace paths);
    3. Connected Tailnet/local `Devices` with online status, OS, tools, and active call leases;
    4. Recent `Vault` inbox/workspace activity.
  - After changing `nypi-board-proto` or `nypi-board-ui`, run `./scripts/build-board.sh` (if `trunk`/wasm target is
    installed in the environment; otherwise verify `cargo clippy --workspace --all-targets` and `cargo test --workspace`).

---

### Phase 5: Unified Inbound `Channels` & `DMN` Vault Consolidator (Decision D81)

#### 5.1 `Channel` Trait & External Share/Webhook Ingestion (`nypi-hub/src/channels/`)
- Refactor `telegram_channel.rs` behind a unified `Channel` trait (`name`, `run`, `deliver_notice`, `ask_approval`).
- Add an authenticated **`IngestWebhookChannel`** (disabled by default; enabled via `settings.channels.webhook` on
  localhost/Tailnet with bearer secret from `SecretStore`):
  - Accepts shared URLs, forwarded articles (e.g., from a WeChat bridge, iOS Shortcuts Share Sheet, or mail hook), plain
    notes, and attachments as `InboundEnvelope { source, sender, text, urls, attachments }`.
  - Saves raw incoming material immediately to `~/.nypi/vault/00-inbox/YYMMDD-HHMM-<slug>.md` (and attachments to
    `vault/attachments/`), injects an inbound event into `PrimaryStream` so Lucia acknowledges receipt naturally, and
    spawns a background `TaskRun` to fetch/parse primary content (falling back across orthogonal fetch methods when
    gated), cross-link relevant `30-entities/`, and file a structured synthesis.

#### 5.2 `DMN` Sleep Consolidator (`crates/nypi-core/src/dmn.rs`)
- Extend idle `DMN` passes (strictly respecting existing `settings.dmn` daily pass and token caps) to act as Lucia's
  **Vault Consolidator**:
  1. Triage untriaged notes in `00-inbox/` into `30-entities/` or active workspaces;
  2. Distill recurring episodic facts from `memories.jsonl` into durable `30-entities/` Markdown pages with
     `[[WikiLinks]]`;
  3. Move settled `10-workspaces/` older than `settings.vault.archiveAfterDays` (default `7`) into `90-archive/YYYY-MM/`;
  4. Promote recurring validated lessons from `~/.nypi/reflections/` into `20-playbooks/` drafts (never modifying
     user-owned skill folders outside `~/.nypi/vault/`).

---

## 4. Non-Negotiable Engineering Rules & Verification Checklist

Follow every rule in `AGENTS.md` throughout execution:

1. **Every new `.rs` file starts with a `//!` module doc comment** stating clearly what it is for and why.
2. **Plain names only**: `hub`, `node`, `interface`, `vault`, `ledger`, `stream`, `router`, `broker`—never body or
   character metaphors.
3. **Never panic on model or network input**: tools return readable error outputs; providers surface `StopReason::Error`.
4. **Security invariants**:
   - `Protected` remains the single source of truth for forbidden paths;
   - `Sandbox` never gains write holes into `~/.nypi` or `.nypi`;
   - Raw secrets from `SecretStore` never appear in prompts, `ToolOutput`, `SessionStore`, `AuditLog`, or `Debug` impls;
   - `PeerRole::Node` connections can never issue user commands or approve `Inspector` requests.
5. **Offline, deterministic tests**: every new subsystem must have unit and integration tests using `nypi_ai::providers::faux`
   or local loopback `hyper`/Unix-socket servers—zero external network calls and zero paid API tokens during `cargo test`.
6. **Documentation lockstep**: as each phase finishes, record its decision (`D76`–`D81`) in
   [architecture.md](../architecture.md), update status rows in [roadmap.md](../roadmap.md), update [devices.md](../devices.md)
   and [usage.md](../usage.md) where CLI flags or protocols change, and update the crate layering line in `AGENTS.md`.
7. **Verification gates required at the end of every phase**:
   ```bash
   cargo fmt --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```
   Commit only if explicitly instructed by the owner, staging explicit paths.
