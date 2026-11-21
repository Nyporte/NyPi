# NyPi V3 Cognitive Kernel & Glass-Box Board Plan

This specification defines the next-generation refactoring of NyPi on top of the `D83` baseline (`6a9db5a81`). It:
1. Splits Lucia's hub cognition into **three strictly separated planes** (`Chat` Information Plane ⇄ `Principal`
   Preemptible Executive Plane ⇄ `TaskPool` Tactical Execution Plane);
2. Connects `goals.md`, `nypi-vault`, `TaskPool`, and `Scheduler` into a **four-tier campaign hierarchy** (`Goal ->
   10-projects/<slug>/plan.md -> TaskRun -> Step/Schedule`); and
3. Upgrades **`nypi board` (`nypi-board-proto`, `crates/nypi-cli/src/board/`, `nypi-board-ui`)** into a **full
   glass-box real-time observatory** where every single turn, reasoning block, read/write tool call, signal, preemption,
   stimulus, and workspace note across **`Chat`**, **`Principal`**, and **`Tasks`** can be inspected step by step.

Execute **Phases 1, 2, and 3** below in order, creating one clean, tested commit per phase (`D84`, `D85`, `D86`). Every
commit must pass `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and
`cargo test --workspace` with zero external network calls during tests, and update `AGENTS.md`,
[architecture.md](../architecture.md), [board.md](../board.md), [roadmap.md](../roadmap.md), and [usage.md](../usage.md) in
lockstep.

---

## 1. Core Architectural Shift

### 1.1 Why `Chat`-as-Principal (`D78`/`D82`) Is Superseded

In `D78`–`D82`, `crates/nypi-core/src/chat.rs` played two conflicting roles on one model (`settings.chat.model`):
fast conversational responder **and** executive principal starting tasks and reviewing worker settlements. That caused:
- **Capability inversion**: `chat.model` is selected for sub-second speed, whereas decomposing multi-week goals, writing
  rigorous `TaskBrief` contracts, and auditing frontier-model worker reports require NyPi's strongest reasoning model.
- **Voice drift vs. execution coupling**: Letting two modules speak directly to the user splits Lucia's voice; keeping
  execution in `Chat` couples conversation to task management.
- **Flat task horizon**: `goals.md` (single lines) and `TaskPool` (minute-scale runs) lacked a unifying `goal` foreign
  key and campaign plan (`10-projects/<slug>/plan.md`).
- **Observability blind spots**: Only `Task` runs had step-by-step `Story` views on the Board; `Chat`'s internal
  lookups and executive reasoning were invisible.

### 1.2 Target Three-Plane Architecture

When conversational mode (`chat.model`) is active (leaving standalone/worker-only runs without `chat.model` unchanged):

1. **Information & Interaction Plane — `Chat` (`crates/nypi-core/src/chat.rs`)**
   - **Sole speaker**: `Chat` is the **only** module that ever speaks to the owner (live in `PrimaryStream` and when
     phrasing proactive notices for `Outbox` / `Channels`).
   - **Read-all, act-none**: Full multi-step `Agent` loop on the fast model (`settings.chat.model`) with **read-only
     access to all system resources** (in-memory stores, `Vault`, `Ledger`, `Principal` status, **and** time-boxed
     `<= 1,500 ms` read-only device tools `read`, `grep`, `find`, `ls`), and **zero** write, mutation, shell, network,
     or `TaskPool` execution tools.
   - **Demand-driven signaling**: Casual chat and read-only lookups complete inside `Chat` without waking `Principal`.
     Only when a message carries an actionable intent, goal, plan change, decision, or deep analytical question does
     `Chat` invoke its sole upward signal tool: **`signal_principal`**.

2. **Executive Plane — Singleton `Principal` (`crates/nypi-core/src/principal.rs`, NEW)**
   - **Goal-oriented, never chat-facing**: Globally unique instance in `PrimaryStream` running on NyPi's strongest
     frontier reasoning model (`settings.principal.model`, defaulting to the worker's model, at
     `settings.principal.thinking`, defaulting to `"high"`). Its objective is never "reply to the user", but **advance
     and complete the owner's goals**.
   - **First-class `Activation` Scheduler (`Active` / `Suspended` / `Queued`)**:
     - At any instant, `Principal` is either `Idle` or running **at most one exclusive `Activation`**.
     - Every wake source—`P0` (`Chat` user signal), `P1` (`Worker` settlement, inbound webhook material, fired schedule),
       or `P2` (budgeted idle pass unifying `DMN` and `Goals Review`)—instantiates a durable `Activation` (`a-...`)
       persisted step-by-step in `~/.nypi/principal/activations/<id>.jsonl` so the Board can inspect it live and
       historically!
     - **Preemptive `Suspend` & Delta-Aware `Resume`**: When a `P0` signal arrives from `Chat` while a lower-priority
       (`P1`/`P2`) or independent `Activation` is `Active`, `Principal` aborts only the in-flight LLM stream,
       **preserves all completed steps of that activation intact as `Suspended`**, runs the `P0` activation to `Done`
       (appending a concise summary to `DecisionJournal`), and then pops and resumes the `Suspended` activation with a
       trailing delta note (`[Resumed after higher-priority activation: ...]`). Rapid follow-up `P0` signals belonging
       to the same active user burst join via `agent.steer`.
   - **Mind & Orchestration Tools Only**: Full Hub-local store read/write (`vault_*`, `goals`, `memory`, `asset_manage`,
     `schedule`) + full `TaskPool` control (`start_task`, `steer_task`, `continue_task`, `cancel_task`, `task_status`) +
     **`stimulate_chat`**. Zero slow external I/O tools (`bash`, device writes, `http_request`, `mcp__*`).

3. **Tactical Execution Plane — `TaskPool` & `Thinking` Observer (`task_pool.rs`, `worker.rs`, `thinking.rs`)**
   - Concurrent isolated `Worker`s and `Specialist`s executing `Principal`'s `TaskBrief`s across local and remote Tailnet
     `Node`s, watched mid-run by `Thinking`'s trajectory observer (`TurnCompleted -> steer`), returning `[tN]`-cited
     settlements to `Principal` (`P1` activation).

---

## 2. Phase-by-Phase Implementation Specification

---

### Phase 1 (Decision D84): Read-All `Chat`, Singleton `Activation`-Based `Principal`, and Durable Plane Logs

#### 1.1 Expand `introspect.rs` to Full Time-Boxed System Read-Only Access
- **Where**: `crates/nypi-core/src/introspect.rs`, `crates/nypi-core/src/place.rs`, `crates/nypi-core/src/inspector.rs`.
- **Change**:
  - Keep Hub-local read-only wrappers (`vault_search`, `vault_list`, `vault_read` capped at `2,000` chars, `memory`
    `recall`, `asset_manage` `list`/`get`/`expiring_within_days`, `schedule` `list`, `goals` `list`).
  - Add **`tasks_inspect`** (read-only view over `TaskPool` active/settled runs, recent `Ledger` tasks, `Principal`
    `Active`/`Suspended`/`Queued` activations, and recent `DecisionJournal` entries).
  - Add **time-boxed read-only device tools (`read`, `grep`, `find`, `ls`)** for `Chat`:
    - Route via the conversation's `Place` (supporting optional `device` and resolving relative paths against the
      sender's current `Focus`).
    - Enforce three strict non-blocking guards:
      1. **Hard `Guard` & Zero-Approval Policy**: Check `Inspector::guard` (`Protected` secrets denied). Also check
         `Policy` synchronously: if reading the target path would require interactive approval, **refuse immediately**
         with a readable message (`"Reading this path requires approval; signal Principal to inspect it in a task"`) so
         `Chat` never blocks waiting on an approval prompt.
      2. **`1,500 ms` Hard Timeout**: Wrap execution in `tokio::time::timeout(Duration::from_millis(1500), ...)`. On
         timeout, return `"Read timed out (>1.5s); manage the user's expectation and signal Principal for background
         scanning"`.
      3. **Output Cap**: Truncate text blocks to `4,000` chars.
  - Include `<devices>` (paired/connected devices, OS, status, home) in **both** `Chat`'s and `Principal`'s system
    prompts alongside `## Where`.

#### 1.2 Implement `crates/nypi-core/src/principal.rs` (`Activation` Scheduler, Logs & `DecisionJournal`)
- **Settings (`config.rs`)**:
  - Add `PrincipalSettings` (`settings.principal`): `model: Option<String>` (defaults to worker's model),
    `thinking: String` (defaults to `"high"`), `max_activations_queued: usize` (default `32`).
- **Durable Step-Complete Persistence (Required for Board Observability!)**:
  - **`DecisionJournal` (`~/.nypi/stores/decisions.jsonl`, folded via `Folded`)**:
    `{ seq: u64, activation_id: String, at: String, priority: String, trigger_kind: String, topic: String, summary: String, tasks_touched: Vec<String>, goals_touched: Vec<String>, stimulated_chat: bool, tokens: u64, cost: f64 }`.
    Show the newest `15` entries in `Principal`'s `<recent_decisions>` prompt section.
  - **Per-Activation Story Log (`~/.nypi/principal/activations/<id>.jsonl` + index `~/.nypi/stores/activations.jsonl`)**:
    Record every event of each `Activation` (`Created`, `Started`, `AssistantTurn` [including thinking blocks and tool
    calls], `ToolResult` [tool, arguments, output, duration_ms, is_error], `Suspended { by_activation, step_count,
    at_decision_seq }`, `Resumed { delta_summary }`, `StimulatedChat { stimulus }`, `Finished { summary, tokens, cost }`)
    so the Board can render every single step of any live or past `Activation`!
  - **Structured `Chat` Turn Log (`~/.nypi/chat/turns.jsonl`)**:
    Alongside `Chat`'s rolling message window, append structured turn records (`turn_id`, `at`, `origin`: `UserSay |
    PrincipalStimulus`, `focus`, `user_text` or `stimulus` payload, `recalled_memories` summary, `thinking` text,
    `tool_calls` [name, arguments, duration_ms, output_excerpt, timed_out, is_error], `signals_fired` [topic, urgency,
    spawned `activation_id`], `reply_text`, `skipped`, `tokens`, `cost`) so the Board can inspect every step of `Chat`!
- **`Activation` Preemption (`Suspend -> P0 -> Resume with Delta`)**:
  - Priorities: `P0 User` (`signal_principal`) > `P1 Event` (`TaskSettled`, `InboundWebhook`, `SchedulePrompt`) > `P2
    Idle` (`IdlePass` / `GoalsReview`).
  - When `P0` arrives while a lower-priority (`P1`/`P2`) activation is `Active` (or an independent `P0` with `urgency ==
    "interrupt"`):
    1. Abort only the in-flight LLM stream of `Active`;
    2. Strip any incomplete/aborted trailing `AssistantMessage` while keeping **all completed assistant + tool-result
       pairs** in `active.transcript`;
    3. Push onto the `suspended` stack (`Suspended { at_decision_seq }`) and log `Suspended` to its activation log;
    4. Run the `P0` activation exclusively to `Done`, appending its outcome to `DecisionJournal`;
    5. When no higher-priority item waits in `queued`, pop the top `Suspended` activation back to `Active`, append
       `[Resumed after handling higher-priority activation(s):\n- ...]` (built from `DecisionJournal` entries >
       `at_decision_seq`), refresh prompt sections, log `Resumed`, and continue!
  - Rapid follow-up `P0` signals during an already-active `P0` burst (`urgency != "interrupt"`) join via `agent.steer`.

#### 1.3 Wire `Chat ⇄ Principal` Zero-Blindness Handover & `Stimulus`
- Remove `start_task`, `steer_task`, `continue_task`, and `cancel_task` from `Chat`. `Chat` holds `introspect` read-only
  tools + **`signal_principal({ topic, urgency })`**.
- **Post-Reply Handover**: When `Chat` calls `signal_principal` during a turn, dispatch the `UserSignal` to `Principal`
  **immediately as `Chat`'s reply finishes**, carrying: `topic`, `urgency`, verbatim recent `PrimaryStream` messages
  **including `Chat`'s exact reply just sent**, a digest of read-only tool results `Chat` gathered in that turn, and
  `Focus`—returning the minted `activation_id` for audit/Board cross-linking.
- **`stimulate_chat`**: Give `Principal` `stimulate_chat({ kind, headline, must_convey: Vec<String>, ask_user:
  Option<String>, vault_ref: Option<String> })`. `Chat` prioritizes live `UserSay` over `PrincipalStimulus`; when
  handling a `Stimulus`, `Chat` speaks in Lucia's single natural voice preserving all facts, options, `[tN]` citations,
  and `vault_ref` in `must_convey` (or `SKIP` if redundant). When `attached == 0`, `Chat` persists the reply and sends
  that natural prose through `Outbox` / `Channels`.
- Keep `thinking.rs`'s **Worker Trajectory Observer (`TurnCompleted -> steer`)** active on `TaskPool`, and retire its
  old `UserMessage -> Thought -> Chat follow_up` path.

---

### Phase 2 (Decision D85): Four-Tier Campaign Hierarchy (`Goal -> Project Plan -> TaskRun -> Schedule`)

#### 2.1 L1 (`goals.rs`) & L2 (`nypi-vault` `10-projects/<slug>/plan.md`)
- Add `"vault"` to `ATTRIBUTES` in `crates/nypi-core/src/goals.rs` (`vault: 10-projects/<slug>`), validated via
  `Vault::resolve`.
- Add `pub const PROJECTS: &str = "10-projects";` to `crates/nypi-vault/src/vault.rs`.
- Teach `Principal` (`prompts/principal.md`) to maintain `10-projects/<slug>/plan.md` (frontmatter + `## Objective`,
  `## Workstreams & Milestones` `[ ]`/`[~]`/`[!]`/`[x]`, `## Key Decisions`, `## Linked Notes`) for multi-step goals.

#### 2.2 L3 & L4 Attribution (`goal: Option<String>` & Pinned `<project_plan>`)
- Add `goal: Option<String>` (`g-...`) and `spawned_by_activation: Option<String>` (`a-...`) to `TaskBrief`,
  `LedgerEvent::Started` / `TaskRecord`, and `ScheduleEntry`.
- In `PromptInputs` (`prompt.rs`), whenever a `TaskRun` carries `goal: Some(goal_id)` whose goal points to a `vault`
  project folder with `plan.md`, automatically pin `<project_plan>` (capped at `6 KB`) right above `<task_workspace>` on
  every turn (D47). Ad-hoc tasks (`goal: None`) carry zero extra prompt weight.

#### 2.3 Unify `IdlePass` & `Goals Review` into `Principal` `P2` Dual-Clock Activations
- Route daemon `Order::IdlePass` and scheduled goal reviews into `Principal` as `P2` activations (enforcing
  `settings.dmn` daily pass/token caps and recording `PassLog`), and route fired schedule prompts and
  `IngestWebhookChannel` drops into `Principal` as `P1` activations.
- Supply `P2` activations with a computed **Dual-Clock Goal Digest**:
  1. **Deadline Urgency** (days to `due` + blocked items needing owner nudge via `stimulate_chat`);
  2. **Staleness / Anti-Starvation** (`days_since_last_task` per goal -> pick one unblocked sub-item from `plan.md` to
     advance via `start_task`, plus `00-inbox/` triage and workspace archival).

---

### Phase 3 (Decision D86): Glass-Box Web `Board` Observatory (`Chat` + `Principal` + `Tasks` + `Goals/Vault`)

Observability is a top owner priority. Upgrade `nypi-board-proto`, `crates/nypi-cli/src/board/`, and `nypi-board-ui`
(Leptos WASM + `static/board.css`) so the owner can visually and intuitively inspect **every step, reasoning block, tool
call, signal, suspension, stimulus, and note** across **`Chat`**, **`Principal`**, and **`Tasks`** in real time.

#### 3.1 Board Navigation & Visual Architecture (`nypi-board-ui` + `board.css`)
Keep the clean, high-contrast light/dark design language in `static/board.css`, extending it with clear semantic lane
accents, collapsible step drawers, live streaming cursors, and cross-link pills between `Chat Turn <-> Principal
Activation <-> Task <-> Vault Note`. Organize the top navigation bar into **five crisp screens**:

1. **`Now` (`/`) — Real-Time Three-Plane Cockpit**
   - **Needs You**: Approvals (1-click Approve/Deny), human-boundary `Blocked` questions, and unacknowledged alerts.
   - **Three-Plane Live Strip** (three prominent live cards updated via SSE):
     - **`Chat` Plane Card**: Current live step (`Idle`, `Reading <tool>`, `Writing` with live streaming text), model
       name, latest exchange preview, count of read-only calls & signals today, and 1-click link to `/chat`.
     - **`Principal` Plane Card**: Current state (`Idle` or `Active` with priority badge `P0 User` / `P1 Event` / `P2
       Idle`, topic, current reasoning/tool step, live streaming text), **`Suspended` Activation Stack** (each showing
       topic + preserved completed step count!), **`Queued` Inbox** count by priority, and 1-click links to `/principal`
       and `/activation/:id`.
     - **`TaskPool` & Campaigns Card**: Open & recent tasks grouped hierarchically under their **L1 `Goal` (`g-...`, due
       date, link to project `plan.md`)**, with indented child `TaskRun`s and `Specialist` runs, state pills, target
       `device` badge, live current tool step, and 1-click link to `/task/:id`.
   - **Devices**: Connected/paired Tailnet & local nodes, roles, OS, tools, and active call count.

2. **`Chat` (`/chat`, NEW Screen) — Full Conversation & Read-Only Step Inspector**
   - Displays the continuous `PrimaryStream` timeline (live-updating via SSE, plus the message input box to talk to
     Lucia directly from the Board).
   - **Every `Chat` turn is an inspectable card** showing:
     - Origin badge (`User` + `Focus` device/folder pill, or `Stimulus from Principal` with expandable `Stimulus` card
       showing `kind`, `headline`, `must_convey` items, and `vault_ref` link!);
     - Expandable **Recalled Memories** pill (`MemoryValve` hits attached to that turn);
     - Expandable **Thinking / Reasoning** drawer (when present);
     - **Step-by-step Read-Only Tool Calls**: each `introspect` / time-boxed device read call shown with tool name,
       formatted arguments, duration (`ms`), timeout/truncation badge, and expandable full output;
     - **`signal_principal` Callout Pill**: highlighted badge showing `urgency`, `topic`, and a direct clickable link to
       `/activation/<id>` so the owner can jump straight from a chat message to the exact `Principal` activation it
       triggered!
     - Lucia's final reply (or `SKIP` badge on suppressed background stimuli, toggled via a "Show skipped" switch).

3. **`Principal` (`/principal` & `/activation/:id`, NEW Screens) — Executive Stack & Step-by-Step Story**
   - **`/principal` Overview**:
     - Top: Visual **Activation Stack & Queue** (`Active` panel, `Suspended` stack cards showing preemption chain,
       `Queued` priority heap);
     - Bottom: Filterable chronological timeline of all `Activations` (`P0` / `P1` / `P2`) alongside `DecisionJournal`
       summaries, token/cost metrics, and clickable links to touched `Goal`s and `Task`s.
   - **`/activation/:id` Deep Inspector** (streams live via SSE while active, just like `/task/:id`!):
     - Header: priority pill, trigger badge & expandable trigger envelope (showing `Chat`'s handover context, settled
       `Task` link, webhook note, or Dual-Clock Goal Digest), duration, tokens, cost, and final decision summary;
     - **Full Chronological Step List**:
       1. Model **Thinking / Reasoning** blocks (collapsible, formatted cleanly);
       2. Every **Tool Call & Output** (`vault_*`, `goals`, `memory`, `asset_manage`, `schedule`, `start_task` linking
          to `/task/:id`, `continue_task` showing the critique sent to the worker, `stimulate_chat` showing the exact
          `Stimulus` sent to `/chat`);
       3. Distinct **Preemption Banners**: `⏸ Suspended by P0 activation a-... (after N steps)` and `▶ Resumed with
          delta: ...` rendered clearly inline at the exact point in the step list where context switching occurred!

4. **`Task` (`/task/:id`, Upgraded) — Deep Tactical Execution Inspector**
   - Header links upward to parent **`Goal` (`g-...`)** and spawning **`Principal` Activation (`/activation/:id`)**, and
     shows target `(device, folder)` + review round count.
   - **Pinned Context Drawers**: clean rendered Markdown cards at the top of the task page for **`<project_plan>`
     (`plan.md`)** and **`<task_workspace>` (`brief.md` + live `findings.md`)** so the owner sees the exact contract and
     running notes the Worker is operating under!
   - **Step Timeline**: every turn, thinking block, tool call (`[t1]`, `[t2]` verified citation handles, target `device`
     chip, duration, expandable args & output), **distinct visual callout cards for mid-run `Thinking` Observer `steer`
     injections**, and **Principal Review verdicts** (`Accepted` vs `Sent back — Round N critique`).

5. **`Vault` (`/vault`, NEW Screen) & `System` (`/system`, Updated)**
   - **`/vault`**: Visual **Goals & Vault Explorer** — top section renders active `Goals` (`Short` / `Medium` / `Long`)
     with due dates, linked tasks, and expandable `10-projects/<slug>/plan.md` Markdown; bottom/side section lets the
     owner browse and read any Markdown note in Lucia's Vault (`00-inbox/`, `10-projects/`, `10-workspaces/`,
     `20-playbooks/`, `30-entities/`, `90-archive/`).
   - **`/system`**: Update `topology.rs` and `system.rs` so the live System SVG map renders the new Three-Plane
     topology (`Interfaces/Channels`, `Chat`, `Principal [Activation Stack]`, `TaskPool`, `Thinking Observer`, `Vault &
     Secrets`, `Inspector`, `Devices`) with live animated edges, keeping the D65 source-tree topology test green!

#### 3.2 Building & Embedding the Board WASM Bundle
- **Important**: `./scripts/build-board.sh` does **not** use `trunk`—it compiles `nypi-board-ui` directly with
  `cargo --target wasm32-unknown-unknown --profile wasm-release` and `wasm-bindgen` into `crates/nypi-cli/assets/board/`.
- Run `./scripts/build-board.sh` followed by `cargo build` so the new Board UI is actually built and embedded into the
  `nypi` binary! (If the environment blocks downloading the `wasm32-unknown-unknown` rustup target, document that clearly
  while ensuring `cargo clippy --workspace --all-targets -- -D warnings` and comprehensive server-side API tests in
  `crates/nypi-cli/src/board/tests.rs` cover `/api/now`, `/api/chat`, `/api/principal`, `/api/activation/:id`,
  `/api/task/:id`, and `/api/vault`.)

---

## 3. Non-Negotiable Invariants & Test Checklist

1. **Update `AGENTS.md`** to state the D84 Three-Plane rule (`Chat` is sole speaker with time-boxed read-only tools +
   `signal_principal`; `Principal` is singleton activation-scheduled executive with store/pool tools + `stimulate_chat`;
   `Worker` holds device/network tools).
2. **Safety**: `Chat` read-only device tools never read `Protected::secrets` and never hang on interactive approvals;
   raw secrets never appear in `turns.jsonl`, `activations/*.jsonl`, `decisions.jsonl`, or Board API responses
   (run outputs through `SecretScrubber` where applicable).
3. **Deterministic offline tests** (`faux` provider + local fixtures) proving:
   - Read-all `Chat` autonomy, `1.5s` timeout, and approval-free refusal;
   - Zero-blindness post-reply handover (`Chat -> Principal`) and single-voice `Stimulus` (`Principal -> Chat ->
     User/Outbox`);
   - `Activation` preemption (`Suspend` mid-run preserving completed steps -> run `P0` -> `Resume` with
     `DecisionJournal` delta);
   - 4-tier `Goal -> 10-projects/plan.md -> TaskRun (<project_plan>)` inheritance & `P2` dual-clock digest;
   - Board API endpoints (`/api/now`, `/api/chat`, `/api/principal`, `/api/activation/:id`, `/api/task/:id`,
     `/api/vault`, `/api/system`) returning complete step-by-step records for all three planes.
