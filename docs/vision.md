# NyPi Design Vision and Architectural Philosophy

NyPi is a compiled, always-on personal agent runtime written in Rust. The agent running on it (default name: **Lucia**)
serves one user over a multi-year horizon as a technical and operational chief of staff.

Where [principles.md](principles.md) specifies normative runtime invariants (`A1`–`A8`) and work-conduct rules
(`B1`–`B6`), and [architecture.md](architecture.md) and [devices.md](devices.md) specify module and protocol designs,
this document states the product and engineering philosophy that governs how NyPi evolves as a whole.

---

## 1. One Lucia, One Conversation, Three Planes

NyPi is one continuous agent across terminals, devices, and time, not a collection of isolated request-response
sessions.

- **One conversation.** The hub holds a single primary stream (`PrimaryStream`). Wherever the user speaks from (a
  terminal in some folder, Telegram, the board, a terminal on another machine), it is the same conversation; each
  message carries its focus (the device and folder the user is at), and work is done where that focus says.
- **Three planes, kept apart.** Talking, deciding, and acting have conflicting needs and are separate by design:
  - **Chat** answers the user at once. It reads everything (the stores, the vault, the tasks, time-boxed reads on a
    device) and changes nothing; when something needs doing it signals the Principal.
  - **The Principal** decides. It plans, keeps the stores (goals, memory, the vault, schedules, assets), starts tasks
    in the pool with a brief that says what done means, and reviews each settled task by its dossier before the user
    hears of it. Its only way to the user is through chat; it has no shell, device, or network tool.
  - **The task pool's workers** act: files, the shell, the network, each task under its brief, keeping its workspace
    notes in the vault, with an observer that steers a run going wrong.
- **Activations, not polling.** The user's messages, fired schedules, goal reviews, idle time, and webhook material
  reach the Principal as activations. Each starts from four rings of context (a cache-stable system prompt, a
  situational radar, a briefing pack made from words and the vault's index before the first model call, and its own
  transcript) and ends disposed of: a task, a store change, a word to the user through chat, or a recorded conclusion.
- **Pulses for causality.** Modules coordinate through typed, causation-linked pulses, so any action traces back to
  the message, schedule, or review that caused it (`nypi trace`).

---

## 2. Delegation, Campaigns, and Time Leverage

Lucia is the user's deputy: she manages complexity, owns progress over short-, medium-, and long-term goals, and
delegates execution rather than attempting every domain task with general-purpose tools alone.

- **Goals with campaigns.** A goal has a campaign in the vault (`10-projects/<slug>/plan.md`); what is started or
  scheduled for a goal carries it, so progress can be followed from the plan to each task.
- **Orchestration separated from execution.** The Principal plans and reviews; workers and `specialists` (configured
  NyPi profiles, or external agent CLIs such as Codex and Claude Code) do the work. Specialists are long-lived: each
  keeps domain instructions, skills, and an experience record that improves with every task.
- **Parallel time leverage.** Human attention is bounded and background compute is not, so independent workstreams run
  side by side in the task pool and come back as reviewed results.

---

## 3. Problem-Solving Resilience and Unbroken Execution

Crash recovery, retries, and durable queues are necessary but not enough; an effective agent must also keep going in
the face of ambiguous, degraded, or adversarial conditions.

- **Exploration before escalation.** An HTTP error, stale documentation, malformed data, or a failing command is the
  start of an investigation. Before reporting `Blocked` or `Failed`, a worker checks its assumptions against primary
  sources, runs small probes to isolate the fault, does not repeat what was shown not to work, and tries another way
  when one proves unviable.
- **Clear authority boundaries.** Persistence stops at what only the user decides: spending money, providing
  credentials, proving identity, and irreversible communication. Stopping there, Lucia says what was tried, what was
  verified, what options remain, and what she recommends.
- **Never silent.** Every run is a task in a durable ledger before it starts, watched for liveness and progress, and
  ends in an explicit state (`Done`, `Blocked`, `Failed`, or `Continuing` with a schedule or delegation to come back).
  What nobody was there to see goes through an outbox until it reaches the user.
- **Reviewed by evidence.** A worker's report is a claim. The Principal reviews it against an evidence manifest the
  harness draws from the task's own session (which the worker cannot write): every cited `[tN]`, and whether a file
  changed after its last check.

---

## 4. Memory, the Vault, and Assets

A long-horizon agent needs structured storage for projects, procedures, and the things it looks after, not a flat log
of conversation.

- **Episodic memory and a provenance-checked portrait.** Facts, commitments, and preferences live in an append-only
  store, ranked with relevance as a gate over importance, emotion, and recent use. Before a message reaches the model,
  the memory valve ranks memories against it without a model call and attaches the few that fit to the message itself
  (not the system prompt), holding back a sensitive one in a light moment; while the user types, the same recall runs on
  the draft and shows what it found. Traits in the user's portrait must quote the user's own words, checked by the
  harness.
- **The vault.** Durable knowledge, incoming material (`00-inbox`), project campaigns and task workspaces, entities,
  and playbooks live in a human-readable Markdown vault with links and an index, written only through the vault's own
  tools.
- **Assets apart from secrets.** Lucia tracks accounts, servers, domains, subscriptions, and renewal dates in an asset
  registry. Secret values never enter a prompt, a session, a log, or a note: they stay in the secret store, named by
  handle (`cred://scope/name`) and used by the harness.

---

## 5. One Hub Across the User's Machines

- **Hub, nodes, interfaces.** One always-on **hub** holds the conversation, the model keys, and all state. A **node**
  on each of the user's machines carries out calls there under that machine's own sandbox and protected paths.
  Terminals, Telegram, and the board are **interfaces**: they talk, watch, and answer approvals.
- **Joining by the user's word.** A machine joins once, with a six-digit code made from both machines' keys and the
  encrypted session, which the user confirms on a machine they already trust. It is admitted as `personal` (it can talk
  to Lucia and keep a copy of the hub's state) or `exec-only` (it only carries out calls).
- **Direct first, relayed when needed.** Machines link directly on the local network (found by mDNS) or at the hub's
  direct addresses, and otherwise through an optional relay behind Cloudflare or a reverse proxy. Every link is TLS 1.3
  with pinned keys; through the relay the same TLS runs inside a WebSocket, so the relay passes bytes it cannot read.
- **A hub that can move.** Personal machines keep a standby copy of the hub's state. `nypi hub here` moves the hub to
  one of them; the hub key moves with it, and an epoch guarantees there is only ever one hub.
- **Updates from signed releases.** Every machine updates from the public repository's signed releases, refuses
  anything not signed and checked, and puts the previous version back when a new one does not stay up.

---

## 6. Collaborative Conduct and Engineering Discipline

- **Intent first, candid communication.** Lucia answers the objective or risk behind a question rather than hiding
  behind current implementation details. She unpacks trade-offs and unverified assumptions while a design is open,
  and reports concisely and verifiably while work is under way, in natural prose in the user's language, without
  flattery or telegraphic stiffness.
- **Plain, compiled systems engineering.** NyPi favors static compilation, states and results as exhaustive Rust
  enums, provider errors in band, few runtime dependencies, plain names (`hub`, `node`, `interface`), and append-only
  files a person can read, folded into memory or local indexes that can be rebuilt.

---

## 7. Independent Governance and Empirical Verification

High autonomy is sustainable only when bounded by mechanisms the agent cannot alter or bypass.

- **A boundary outside the agent's reach.** The `Inspector`, `Protected`, project trust, and the OS sandbox live in
  the harness, outside every tool's write path, so Lucia cannot weaken her own constraints, forge the user's
  approvals, plant a service that runs outside the sandbox, replace her own program, or rewrite the audit log and the
  ledger.
- **Evidence-backed claims.** Answers cite harness-assigned tool references (`[tN]`), checked against the session
  before they are shown.
- **Measured, not self-reported.** A `Done` is a claim. Capability and regressions are measured by two independent
  signals: an adversarial benchmark of real recovery and exploration (`nypi eval`, through the three planes too), and
  the user's own verdicts over time, aggregated in the scorecard.
