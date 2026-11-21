# NyPi Roadmap

Priority rule: a fully working agent first, then the cognitive architecture, then research items. Each milestone ends in
a usable binary. Items cite the principle (P-xx, see [principles.md](principles.md)) or requirement (R-n, numbered as in
the conceptual design list) they serve. Design details are in [architecture.md](architecture.md).

Status: `done` · `in progress` · `next` · `planned` · `deferred` · `research`

## M1: Kernel parity with Pi (working coding agent)

| Item | Serves | Status |
|---|---|---|
| Message model, streaming protocol, never-throw provider contract | foundation | done |
| Anthropic Messages provider (SSE, thinking, prompt caching) | foundation | done |
| OpenAI-compatible Chat Completions provider (any `base_url`) | A7 | done |
| Faux provider and an HTTP-level provider test harness | testing | done |
| Agent loop: turns, parallel/sequential tools, before/after hooks, steering, follow-up, cancellation | A5, A6 | done |
| Tools: read, write, edit, bash, grep, find, ls, with Pi's truncation limits | foundation | done |
| Pi's path resolution in tools (Unicode spaces, `@` mentions, macOS name forms) and string-encoded edits (D51) | foundation | done |
| JSONL tree sessions (Pi v3 subset), resume and continue | A4 | done |
| Compaction (Pi algorithm and checkpoint prompt) | foundation | done |
| Context recovery as in Pi: overflow detector (27 provider wordings, silent overflow), one compact-and-retry for overflowing or cut-off replies, `context_edit` session entries (D44) | B6 | done |
| Compaction between turns of a long run, as in Pi (`TurnHooks`, D45) | B6 | done |
| Thinking levels clamped to what the model accepts, at the session and at the request (D46) | foundation | done |
| Prompt sections rebuilt before every turn, so mid-run edits apply at once (D47) | R4 | done |
| Layered system prompt: core (read-only), self, project context, skills | R4, B1-B6 | done |
| Agent Skills loading (including `~/.pi/agent/skills`) | A7 | done |
| Inspector: append-only audit log of all events + destructive-command guard | A1, A4, R5 | done |
| CLI: interactive (streaming, steering while working, Ctrl-C abort), `-p` print, `--json` events | A1, A5 | done |
| Run-end report rule in the core prompt (Done / Blocked / Failed / Continuing + next step) | B6 | done (enforced in code since M2) |
| `self_prompt` tool: versioned edits to the read-write prompt layer; direct writes blocked | R4 | done |
| Telegram bridge: pairing-gated access, steering and abort from chat, batched tool log, alert push | A1, A3, A5 | done; replaced by the daemon's Telegram channel (D66, D91) |
| macOS always-on service for the bridge (`scripts/nypi-telegram-service.sh`) | B6 | superseded: Telegram is a channel of the daemon, which the supervisor keeps (D64, D66); the script is removed |
| Env-file secrets kept private (not exported to tool subprocesses) | safety | done |
| Provider retries (429/5xx with backoff) and a 300 s stall timeout so a run can never hang silently | B6 | done |
| Auto-retry of transient provider errors (dropped connections, overloaded, rate limits, 5xx) with Pi's budget, abortable backoff, and `auto_retry_*` events (D42); also chat, compaction summaries, and one-shot calls (D43) | B6 | done |

## M2: Extensibility and the first split modules

| Item | Serves | Status |
|---|---|---|
| MCP client, stdio transport: `mcpServers` in settings, tools as `mcp__<server>__<tool>`, failures alert but never block startup | A7 | done |
| MCP Streamable HTTP transport, resources, live tool-list changes, `${NAME}` secrets in env and headers | A7 | done |
| MCP prompts as `/mcp:<server>:<prompt>` commands, with live prompt-list changes | A7 | done |
| MCP: the legacy HTTP+SSE transport (with fallback), session re-initialization, the standalone GET stream and stream resumption | A7 | done |
| Status-line enforcement: parse the status, nudge once when missing, derive it from the stop reason otherwise | B6 | done |
| Proactive Telegram reports: long-run results, stall notices, unanswered approvals; from terminal, print mode, and the bridge | A3, B6 | done |
| Approval mode in the Inspector: risky calls wait for a yes from the terminal or Telegram; print mode denies | A3, R5 | done |
| Approve print-mode and terminal requests from Telegram (bridge control socket, numbered approvals) | A3, B6 | done |
| Command tools: external programs declared in `commandTools`, argv substitution without a shell | A7, B2 | done |
| RPC mode (JSONL over stdio, Pi's command records) so other programs can drive NyPi | A7 | done |
| RPC parity: retry, compaction, queue-mode, thinking-cycle, session-stats, and session-switch commands (D48) | A7 | done |
| RPC session tree: entries, tree, fork, clone, fork candidates, session names (D49) | A4, A7 | done |
| RPC user bash (`bash`, `abort_bash`) and Pi's non-provider session messages read into context (D50) | A4, A7 | done |
| Model cycling through `enabledModels`, RPC `cycle_model` (D52) | A7 | done |
| Session export as a static HTML page rendered in Rust (`export_html`, `/export`, D53) | A4, A7 | done |
| `!` and `!!` shell commands in the terminal and TUI, abort for worker jobs, Pi's `--models` (D54) | A5, A7 | done |
| Board phase 1 (`nypi board`, D56, [board.md](board.md)): a board link in every runtime; Now (inbox with approve, deny, and clear; live work with the reply streaming, stop, and message; the last 24 hours; up next), a task's story, and Activity; a Rust/WebAssembly page on the LAN behind a token. Supersedes the read-only observer (D55) | A1, A2, A3, A4, A5, B6 | done |
| Board phase 2: Agents (per-agent work and outcomes) and Health and cost (by day, model, and agent; retries, failing tools, MCP, watchdog) | A3, B2 | planned |
| Board phase 3: Knowledge (memory, portrait, self-prompt history, lessons) and Plans (goals, schedules, DMN) | A8, B4 | planned |
| Board phase 4: resume and cancel from the inbox, reply to a blocked task, edit goals and schedules, review skill proposals; notifier links open the board's card | A3, A5 | planned |
| Pulse bus with priority and causation links; Inspector records every pulse; `nypi trace` | R3, A4 | done |
| Repository holds only NyPi; Pi's sources moved to the sibling checkout `../Pi` (D18) | maintenance | done |
| Split Chat module (fast model) from a Worker agent (strong model); chat relays worker progress | R1, A5 | done (every interface; chat history kept per worker session since D75) |
| ratatui TUI with a live task panel (`--tui` or `"tui": true`; solo and chat mode) | A1, A2 | done |
| Transcript-carried system prompt and tool deltas (Pi's cache-stable changes): leading system message, section and tool updates, in place for flagged models | efficiency | done |
| More providers: OpenAI Responses, Google Gemini, Amazon Bedrock (SigV4, event stream); 1360-model catalog | A7 | done (Responses live-tested; Gemini and Bedrock against mock servers) |

## M3: Secretary, the task ledger, and never-silent execution

| Item | Serves | Status |
|---|---|---|
| Durable task ledger (intent before effect, crash resume, `nypi tasks`) | B6, A6 | done |
| Watchdog: detect `Stalled` tasks, alert, resume in the background or escalate; `nypi watchdog` | B6, A3 | done |
| Specialist profiles: native (kernel loop) and external (agent CLI as subprocess; Codex and Claude Code templates); `nypi specialists` | B2 | done |
| Delegation tool: Lucia dispatches tasks to specialists and reviews results (`delegate`, child tasks) | B2, B4 | done |
| Put the installed agent CLIs to work as specialists: detect them (`nypi specialists detect`), working Codex and Claude Code profiles, Lucia delegating real work | B2 | done for Codex (Lucia delegates coding to it); Claude Code is ready once signed in |
| HR workflow: research-backed creation of master-level specialists with skills and an evaluation checklist | B2 | deferred (after the existing agents are in regular use) |
| Goals file (short/medium/long term) that the DMN and planner keep advancing | B4, B6 | done: goals file, versioned `goals` tool, prompt section, `nypi goals review`; idle time weighs the goals by two clocks (D85) |
| Reports with citations to tool calls and sources (checked `[t3]` references, `Sources:` footer, specialist citations carried through) | A4, B5 | done |
| Post-task reflection that writes lessons to the self-prompt and proposes skill edits (`nypi reflections`) | A8 | done |
| Out-of-band notifications: desktop, webhook (Slack, Discord, ntfy), command (e-mail through sendmail); `nypi notify test` | A3 | done |
| Notice outbox (D57): recorded before sending, retried with backoff, shown as "While you were away" at the next start; desktop channel by default; `nypi notices` | A3, B6 | done |
| Unattended runs always report (D58): `--unattended` results and failures to start go to the outbox; repeating stall notices; the Telegram bridge delivers alerts during a run | A3, B6 | done |
| `Continuing` needs a way back (D59): a schedule set in the run, or the agent is told to go on (three times), then Blocked; `Waiting` tasks judged by their schedules; resumes that cannot start are given up | B6 | done |
| Supervisor (D60): `nypi supervisor` runs the watchdog, scheduler, DMN trigger, and outbox retries with no front end open; `install`, `uninstall`, `status` (macOS LaunchAgent) | B6, B4 | done (not installed on this machine; Linux unit not written) |
| Daemon (D64): `nypi daemon` is Lucia as an always-on process on a local socket (RPC protocol, many clients, results to the outbox when nobody is attached, resumes its own interrupted tasks); the supervisor starts it, restarts it at once when it exits or hangs, and reports a crash loop; `nypi send`, `nypi attach`; tools cannot reach the socket | A5, B6, B4 | done on macOS (not installed on this machine) |
| Board, System screen (D65): the whole program as a map (34 modules in five regions, 52 connections of five kinds), live states, moving lines where work flows, a detail panel per module, a list for phones; `/api/topology` | A1, A2 | done |
| Daemon, remaining: a stuck run inside a live daemon. Chat mode in the daemon is with step 2's remainder in M6 | A5, B6 | done: a run with no progress for `stuckMinutes` is stopped and ends Failed (D73); chat mode is in every thread (D67, part 1) |
| Project trust gate (D61), as in Pi: a folder's own settings, specialists, and skills load only once the user trusts it; `nypi trust` | R5 | done |
| The Inspector's boundary holds (D62): all of `~/.nypi` and project `.nypi` folders unwritable and key files unreadable, exact path resolution in the guard, and an OS sandbox around every tool subprocess | R5 | done on macOS (other systems: guard checks only) |
| Model-supplied numbers cannot crash a tool or the scheduler (bash timeout, read limit, schedule interval) | R5 | done |
| Verdicts and scorecard (D63): `nypi rate` (one key per task) and `/rate` record the user's judgement in the ledger; `nypi scorecard` judges each week against bars from the ledger, outbox, and audit log | B1, B6, A1 | done |
| Measuring, remaining: failure drills against a real model (kill, close, cut the network); a baseline for the benchmark (built in D78 and D90) with a real model; cost per task, wrongly surfaced memories, and time to first response on the scorecard; rating from Telegram or the desktop | B1, B6 | planned |
| Boundary, remaining: MCP servers and external specialists run outside the sandbox (with a clean environment since D76); the Linux sandbox (bubblewrap, D76) is not yet run on a Linux kernel; the audit log is append-only by convention, and a failed audit write only alerts | R5 | planned |
| Never-silent, remaining: pid reuse after a reboot can make a dead task look alive; the status falls back to Done when a reply has no status line twice | B6 | planned |

## M4: Memory and proactivity

| Item | Serves | Status |
|---|---|---|
| Long-term memory store: event log, `memory` tool, BM25 with tags (CJK-aware, English stemming), `nypi memory` | R6 | done |
| Semantic recall: embedding sources in order (endpoints, then local Qwen3-Embedding-0.6B Q4 via candle), centered and calibrated; `nypi memory model download`, `reembed` | R6 | done |
| Weighted retrieval: relevance gates importance, emotion, and ACT-R activation; `--explain` | R6 | done |
| Memory valve: attach relevant memories by situation (task, casual, light) and sensitivity; `memoryValve` pulses | R6 | done |
| Recall while the user types: `typing` pulses (TUI, RPC), hints under the input, cached query vectors; drafts never logged | R2 | done |
| User portrait with provenance: traits learned after each turn, every one quoting the user (checked); `nypi portrait` | R7 | done |
| DMN: idle-time and interval passes over goals, tasks, schedules, and memory, with a daily budget; `nypi dmn` | R3 | done |
| Timers and condition triggers as pulse sources: `schedule` tool, `nypi schedule`, notices and prompts fired as tasks | R3 | done |
| Thinking module: a reasoning model's deeper look at each message beside chat, followed up only when it adds something | R1, R2 | done; retired by D84 (the Principal gives that look; the module observes runs) |
| Tuning from the first side-by-side evaluation (D41): cheap schedule triggers, a delegation size threshold, keyword-form recall in the valve, both results for open conventions | R3, R6, B1, B5 | done |

## M5: Research

| Item | Serves | Status |
|---|---|---|
| Local small chat model (llama.cpp or candle) for sub-100 ms first token | R1 | research |
| Nightly LoRA fine-tuning from memory and style | R8 | research |
| Motivation fitting: predict next questions and pre-generate answers | R9 | research |
| Situation-anchored emotional embeddings (affect vector x situation embedding) | R10 | research |
| Prompt weight differentiation beyond ordering and labels | R6 | research |
| Hash-chained, externally anchored audit log | R5 | research |

## M6: One hub across devices (decision D66, design in docs/devices.md)

| Item | Serves | Status |
|---|---|---|
| The hub and its supervisor on one machine: `nypi daemon`, `nypi send`, `nypi attach` (D64) | A5, B6 | done |
| Step 1: Telegram as a channel inside the daemon (relays the runs a chat started, approvals from any run, guarded pairing); the supervisor starts and keeps the board (`board` settings, `nypi board --link`); one install | A5, B6 | done (the channel is tested against a mock Bot API, not a real bot) |
| Step 2: threads in the daemon (one per folder, opened on demand, closed when idle, working at the same time); plain `nypi` and `nypi -p` attach to their folder's thread as interfaces, drawn as before (`--standalone` for a session of its own); an `elsewhere` section tells each thread what the others do; fired schedules and goal reviews are messages to the daemon that wait their turn; resumes are left to the thread that holds the conversation | A5, A6 | done; replaced by one conversation with a focus per message (D79) |
| Step 2, remaining: an attached terminal does not attach again by itself when the hub restarts; a folder's settings change applies when the conversation opens again (its skills and specialists at the next run, D73). Idle passes run inside the hub (D67, D85) and the full-screen UI is an interface (D67) | A5, A6 | planned |
| Step 3: nodes on computers: the node library and its wire types; tools declared by the node; the hub's register of nodes; `nypi node`, pairing and `nypi devices`, the network listener, a journal for calls, reattach, a lease per call | A6, R5 | done: the node library and wire types, device tools called through the node, `nypi node` on a link, the register with identity, tools, and presence, `nypi devices`, a focus on another device, tasks and schedules that record their device, the device's home in the approval policy, fast replacement of a silent link (D68, D71), the node's journal (D74), pairing over the user's network with a listener of NyPi's own (D80), a device folder's context files, skills, and native specialists (D80, D83) and its safe settings (D91). Not done: resources and environment in the register; a watched-file schedule condition at a device |
| Step 4: a node on Android: the app as node and interface; accessibility service, screen tree and screenshots, taps and text, notifications; the stricter Inspector | A7, R5 | planned |
| Step 5: a node on iPhone: the app as interface, then the node within what Apple allows (Shortcuts, calendar, reminders, files, location); pushes to wake it | A7 | planned |
| Step 6: specialists as jobs on a node; notices shown on a device; devices on the board's map | B2, A3, A1 | partly done: devices on the board's map (D74) |
| Step 7: a hub that can move: continuous copies of its state to a standby, and a documented switch | B6 | planned |
| Not decided (the user's): where the hub lives in the long run; what of a device's content may be kept on the hub; a detached mode without the hub; nodes on machines the user does not fully control; an Apple developer account and pushes for the iPhone app | | open |

## Reshaping the code to the brief (decision D67)

| Part | Serves | Status |
|---|---|---|
| 1. A daemon thread is the whole Lucia: with `chat.model` set, chat answers at once and hands work to the worker, thinking and recall run beside them, on every way of reaching a thread | R1, R2, A5 | done (recall while typing still needs an interface that reports drafts) |
| 2. One front-end path: the terminal, `nypi -p`, and the full-screen UI are interfaces on a link to one thread; without a daemon the same hub runs in the terminal's own process; approvals, stall notices, and long-run reports are done once by the thread; the per-front-end run loops (terminal, chat mode, full-screen UI, old Telegram bridge, print mode) are deleted | A2, A5 | done (Ctrl-C does not stop a `!` shell command typed at the terminal) |
| 3. One pulse bus in the hub; scheduler, watchdog, and idle passes as modules that publish pulses; no helper processes | R3, B6 | done: one bus per hub with every pulse scoped to its thread, a run named as caused by the timer pulse that started it, the scheduler and the watchdog as modules of the hub, and fired prompts, resumed tasks, and idle passes as jobs inside it with no second `nypi` process. Kept on purpose: the two lock files, since a terminal's own hub can run beside the daemon. A delegated specialist's runtime still has a bus of its own (part 4) |
| 4. Delegation in the background: `delegate` returns at once and the specialist's completion comes back as a pulse, so several run at once | B2, B4, A6 | done: in a conversation the specialist works in the background and its report comes back as a `reported` pulse that the conversation reviews as a task of its own; a promise to report stands on a working specialist; the watchdog closes or raises a task that waits on one. `wait: true`, and any run that is not a conversation, still waits. Not yet: cancelling one background specialist by name |
| 5. Crates that match D66: hub crate, node crate and wire types, tools behind the place they run at, the runtime split by responsibility | A7 | done: `nypi-hub` holds the hub and `nypi-cli` the program and its interfaces; `nypi-node-proto` and `nypi-node` are the node, with no model code; device tools are registered into this computer's node and called through the thread's place; the runtime is six files; a node on another device and the register followed in step 3 |
| 6. The hub holds the folded state of the stores it alone writes, instead of re-reading whole files per query | A6 | done: the ledger, schedules, outbox, and memories keep their fold in memory and read only what was appended, shared per file in a process; it follows the file, so other processes' writes are seen. A repeated read of a 16,400-event ledger went from 23 ms to 9 microseconds, and of 1,500 memories with vectors from 223 ms to 9 microseconds (debug build). Not folded: the audit log, the portrait, the pass log |

## Seams found by the review of the whole code ([plans/optimizations.md](plans/optimizations.md))

| Step | Serves | Status |
|---|---|---|
| 1. Paths followed as the kernel follows them; the file tools check what they open; the user's skill folders protected (D69) | R5 | done |
| 2. One approval id everywhere; an answer on the board resolves the question for every interface; the board's listener keeps a question from being denied for want of an answerer (D70) | A1, A3, A5 | done |
| 3. Remote places finished: typed place keys, tasks and schedules that record their device, argument fix-ups for remote calls, the device's home in the approval policy, its system in the prompt, fast replacement of a dead link (D71) | A6, B6, R5 | done |
| 4. One command language: the hub's commands typed once (`HubCommand`), both loops on it, every command available with chat in front (runtime commands, `bash`, steering, follow-ups, reflections), the daemon split by responsibility (D72) | A2, A5, A8 | done |
| 5. Stuck runs stopped after `stuckMinutes` of no progress and ending Failed; a call waiting for approval holds up no other; trust, skills, and specialists re-read before each run; standalone like the daemon; one visible retry layer; keys out of `Debug`; no empty sandbox read rule; peer ancestry by system call, also on the bridge socket (D73) | B6, A5, A6, R5 | done; the Linux sandbox followed in D76 and the tokenizer's hash pin in D91 |
| 6. A journal on the node: calls outlive a dropped link for a lease, results handed over on the next link and acknowledged, ids taken at most once across restarts; a sourced run no longer renames its thread on the board; devices on the System map (D74) | A4, A6, B6 | done; one board link per process followed in D80 |
| 7. Chat's history kept across restarts and idle closes; a task waiting on several specialists waits for all (D75) | R1, B2, B6 | done; the conversation is no longer bound to one place since D79 |

## V2: the chief-of-staff runtime ([plans/v2-plan.md](plans/v2-plan.md))

| Phase | Serves | Status |
|---|---|---|
| 0. Clean subprocess environment (`sandbox.allowEnv`, a specialist's `passEnv`), the tool sandbox on Linux through bubblewrap, `<partnership>` conduct in the core and chat prompts (D76) | R5, B1, B5 | done (the Linux sandbox's arguments are tested everywhere; it has not been run on a Linux kernel from here) |
| 1. `nypi-vault`: episodic memory and the JSONL folds moved below the runtime; Lucia's own Markdown vault (layout, safe paths, six tools), its heading-chunk and link index with one-hop graph retrieval; the asset registry (`asset_manage`); the sealed secret store with `cred://` handles, `nypi secrets`, `http_request` with brokered auth, per-host trust, and output scrubbing (D77) | R6, R5, B4 | done (the keychain path for the master key is not exercised by the tests, which use the key file) |
| 2. Principal, observer, worker: briefs with `done_when` and `bounds`, a task pool of up to `tasks.maxConcurrent` tasks, vault workspaces pinned into every turn's prompt, `turnCompleted` pulses and the Thinking module's rare steer of a run, the principal's review of a report before the user hears of it (`continue_task`, `tasks.maxReviewRounds`, `chat.review`); `nypi-eval` and `nypi eval`: twelve tasks with a hurdle each and five measures (D78) | A6, B1, B2, B6 | done; the first task of a conversation runs on its own worker, by design (D78); the benchmark has no real-model baseline yet (see Open items) |
| 3. One conversation: `PrimaryStream` with its session in `sessions/primary`, the ambient focus of each interface and message, `nypi-hub-proto`; a routed place (`DeviceLookup`, a `device` on file and shell calls, `on_device`, the Inspector judging a call for where it goes); `<active_tasks>` and `<devices>` in place of `elsewhere`; typed wait predicates and a task's workspace in the ledger (D79) | A5, A6, B5, B6 | done; open: a focus folder on this computer brings its context files but not its `.nypi` settings into the conversation; command tools work at the hub's own folder only; the terminal has no command to change its focus |
| 4. Pairing over the user's network: `hub.listen`, `nypi pair`, `nypi node --connect`, Ed25519 identities and TLS 1.3 with the hub's key pinned, a role fixed per key and acted on in one place; a device's folder context through its node, schedule command conditions on the device they were set at; one board link per process, and the task tree, devices, and vault notes on the board's home screen (D80) | R5, A1, A6, B5 | done; not run between two machines or over a real tailnet (the tests pair and link over loopback TLS); a watched-file condition at a device is not taken |
| 5. Channels behind one trait, with Telegram as the first and an authenticated inbound webhook (`channels.webhook`: notes, links, and files into `00-inbox/`, then a task to file them); the idle pass as the vault's consolidator (`vault.archiveAfterDays`, inbox triage, memory into entity pages, playbook drafts from recurring lessons) (D81) | R6, A7, A8, B4 | done; no idle pass has filed a real inbox with a real model, so the quality of filing is not measured |
| Refinement: topic intake with the conversation verbatim and `Done when` set by the worker, chat's read-only lookups, a user message joining or preempting chat's turns and messages in a row coalesced, MCP connected in a task's own runtime, a fresh worker session per project (D82) | A5, R1, B5 | done; not measured with a real fast model |
| Refinement: the scheduler's flaky test traced to an inherited `flock` and fixed with a record lock; unpairing closes open links; a trusted device folder's skills and native specialists applied to runs there, delegation at the run's focus; racing delegations (`AnyOf`); notices through the running channels (D83) | R5, A6, B5, B6 | done; pairing and races still not run between two machines |

## V3: three planes, campaigns, and a glass-box board ([plans/v3-plan.md](plans/v3-plan.md))

| Phase | Serves | Status |
|---|---|---|
| 1. Three planes: chat the only voice, reading everything (stores, `tasks_inspect`, device `read`/`grep`/`find`/`ls` time-boxed to 1.5 s and never asking for approval) and changing nothing, `signal_principal` after its reply; a singleton Principal on the strongest model with activations by priority, suspension and resumption with the decision journal's delta, the task pool and stores, `stimulate_chat`; `chat/turns.jsonl`, activation stories, `stores/decisions.jsonl`, all scrubbed of secrets; the Thinking module's second look retired (D84) | R1, A5, B6 | done (schedules, webhook drops, and idle passes wake it since phase 2); not measured with real models |
| 2. Campaigns: `vault: 10-projects/<slug>` on a goal, checked against the vault; `plan.md` kept by the Principal; `goal` and `spawned_by_activation` on briefs, the ledger, and schedules; `<project_plan>` above `<task_workspace>`; idle passes and goal reviews as the Principal's `P2` activations within the DMN budget, recorded as passes, with a two-clock goal digest; fired schedules and webhook material as `P1` (D85) | B4, B6, A6 | done; no live run with a real model on a real campaign |
| 3. The glass-box board: Now with the three planes and the tasks under their goals, Chat with every turn step by step, Principal and each activation step by step with suspension and resumption, a task's goal, activation, plan, workspace, steers, and verdicts, Vault with goals and notes, System with the planes; `/api/chat`, `/api/principal`, `/api/activation/:id`, `/api/vault`, `/api/system`, live `chat` and `activation` events, the `say` action; every answer scrubbed of secrets (D86) | A1, A2, R5 | done; the screens were checked in the built-in browser against `nypi board` on a sample home from the board's test |

## V3.1: executive precision, evidence dossiers, live board ([plans/v3.1-plan.md](plans/v3.1-plan.md))

| Part | Serves | Status |
|---|---|---|
| 1. Three fixes (a resumed task's workspace and campaign, a scrubber that follows the secret store, a cache-stable Principal system prompt with the portrait); the worker's `report.md`, the harness's evidence manifest in every review, `inspect_task_evidence`; `conclude_activation` and one nudge for an activation that disposed of nothing; `batch_unless_failed` and same-goal settlements reviewed as one (D87) | A4, B6, R5 | done; not measured with real models (the nudges and the hold apply to them; the tests opt in) |
| 2. Four rings for every activation (the cache-stable system prompt; `<situational_radar>` with goals on their clocks, the execution board, schedules of the next 48 hours, and recent decisions; a `<briefing_pack>` looked up by what woke it, by words, without waiting; the transcript with `<resumption_delta>`); chat's and the Principal's steps and text streamed to the board while in flight; the task page's report, manifest, and cited references, the activation page's briefing pack, disposition, and nudge (D88) | R1, A1, A4 | done; the briefing uses words only (no vectors); the screens were checked in the built-in browser on a sample home from the board's test, but live plane progress only by the tests, not with a running daemon |

## V3.2: working method and the three planes measured ([plans/v3.2-plan.md](plans/v3.2-plan.md))

| Part | Serves | Status |
|---|---|---|
| 1. A working method in `principal.md` and `chat.md` (check before asking, inspect before changing, parallel work, context passed into briefs, a clean stop at the user's four boundaries), "the user" in every prompt and model-facing string with `{{user_ref}}` in the Principal's prompt; inbound material disposed of, dossiers scrubbed whole, the Chat screen opening on a turn in flight (D89) | B1, B3, B5, A5 | done; whether the method changes what real models do is for part 2's benchmark to measure |
| 2. `nypi eval --three-planes`: each scenario through chat, the Principal, and the task pool, ended when every plane is quiet and judged by the same checks, with chat turns, signals, activations, tasks, and send-backs counted; `--chat-model`, `--principal-model`, `--suite worker\|planes\|all`; four scenarios for the working method (D90) | A4, B3, B6 | done, tested with scripted models only: no live run with real models is recorded, so neither suite has a baseline yet |

## V3.3: clean-up and current documents ([plans/v3.3-plan.md](plans/v3.3-plan.md))

| Part | Serves | Status |
|---|---|---|
| 1. `thinking.enabled` as the observer's one switch; a trusted device folder's safe settings applied to runs there and taken back after; Telegram under `channels/telegram/` with its bot token in the secret store; "the user" in every model-facing and board string; `tokenizer.json` pinned by SHA-256; `principal/` split into mod, journal, runner, and tools (D91) | R5, A2 | done |
| 2. The finished stage plans moved to [plans/](plans/); architecture.md's body rewritten to the system as it is, with the decision log kept whole; the other documents brought up to date; [overview.md](overview.md) (a summary in Chinese) checked against the code (D92) | A2 | done |

## Known tensions (tracked, not blockers)

1. **Chat speed vs. answer quality (R1).** Resolved by design: chat relays, and other modules supply the depth. Real
   sub-second chat needs either a local model or the fastest API tier; M2 uses the API tier.
2. **Self-modification (R4) vs. read-only Inspector (R5).** Resolved by layering: Lucia may edit only the self layer;
   core prompt, Inspector, and policies are outside every tool's write path.
3. **24/7 autonomy (B4, B6) vs. cost and safety.** The DMN and watchdog run under a token/cost budget with alerting;
   anything outward-facing (sending, purchasing, publishing) requires user approval through the Inspector.
4. **External specialists (B2) vs. traceability (A4).** External agents are black boxes; NyPi captures their event
   streams and final artifacts in the ledger. Their internals are not traced, and this limit is documented.

## V4: two instances, one entry, direct links, and moving the hub ([plans/v4-plan.md](plans/v4-plan.md))

| Part | Serves | Status |
|---|---|---|
| 1. `instance.json` (hub or node, epoch, the trusted hub key, trust) and one supervisor that runs either (a LaunchAgent, or a systemd user unit); one key and one admission per machine with `personal` or `exec-only` trust; links that name their purpose, decided in `daemon/remote.rs`; joining with a six-digit code from both keys and the TLS exporter, admitted by the user wherever approvals go, within limits; invite codes; a built-in entry; `nypi hub up\|run\|down`, `nypi node up\|run\|down\|leave`, `nypi status`, `nypi devices`; D80 homes moved once (D93) | A5, R5, B6 | done; tested over loopback with several NyPi homes, not between real machines |
| 2. Direct links on `hub.listen` with the hub's direct addresses (interfaces, a forwarded port, NAT-PMP or UPnP-IGD) and mDNS; nodes probe the direct ways first, take only the pinned key and the highest epoch, fall back to the entry, and move a relayed link to a direct one with calls kept; `nypi relay up\|run\|down`, a plain HTTP and WebSocket relay behind Cloudflare or a reverse proxy, with the hub's signed control connection, links as binary frames around the same pinned TLS, browsers passed to the board, and its limits; the board at the entry with browser sessions admitted by code (D94) | A5, R5, B6 | done; tested over loopback with a relay, two simulated networks, and several NyPi homes; port mapping tested against a simulated router only |
| 3. The hub's state and standby copies on personal machines (checked differences, paced over the relay, never on an exec-only machine); `nypi hub here` (a planned move: the hub stops its work, sends the last difference, steps down, then the target starts) and `--force`; the epoch kept by the relay and checked by nodes and mDNS; an old hub steps down with its state under `diverged/` (D95) | A5, B6, R5 | done; tested over loopback with several NyPi homes and a relay (planned, failed, and forced moves, an old hub coming back by the relay's word and by a node's) |
| 4. The hub's commands from any personal machine: `HubCommand::Admin` on a talk link with an allowlist in `protocol.rs`, run by the hub with the same code and its text sent back (`outln!`); `secrets set` reads the value on the node and the hub stores it; exec-only nodes refused, there and by the hub (D96) | A5, R5 | done; tested over loopback with a hub and two nodes |

## Public release and updates ([plans/public-and-update-plan.md](plans/public-and-update-plan.md))

| Part | Serves | Status |
|---|---|---|
| 1. A public repository: no personal domain, gateway, address, or credential in a tracked file; the entry optional (none by default, LAN first), saved once with `--entry` and followed by a running hub; RFC 2606 examples; version 0.4.0 and the repository in `Cargo.toml`; README, SECURITY.md, ignore rules for keys and backups (D97) | A4, A5 | done |
| 2. Verified self-update: signed manifests and checked archives, a pre-flight `--version`, an atomic swap with `nypi.prev`, the supervisor's confirmation or rollback, background checks (`notify`, `auto` on an idle machine, a relay only told), `nypi update --node` and `--all`, versions in `nypi --version`, `nypi devices`, and the board; CI, a release workflow for three targets, and `scripts/install.sh` (D98) | A3, A4, A5, B6 | done; tested against a release server on loopback; the workflows and the installer have not run on GitHub yet, and no release key is built in until the first release |
| 3. Refinements after D98: every service restarted onto the new binary and a pre-loop rollback guard, `--allow-unsigned` for `--check`, failed automatic updates told, SemVer pre-releases, a pasteable release key; the release workflow's tag check, aarch64 cross-build, and unsigned dry run; the installer's no-release message; both systems' service folders protected and a guard that sees `nypi` by path and after flags; standby batches that read only what changed; relay addresses believed only from loopback; the board's files revalidated by `ETag` (D99) | A3, A4, A5, B6 | done; tested over loopback; the workflows have not run on GitHub yet |
| 4. One name for the always-on process: `hub.enabled`, `hub.cwd`, and `channels.telegram.enabled` (an old `daemon` section still read), `run/hub.sock`, `run/hub.json`, `logs/hub.log` (an earlier hub's `run/daemon.json` still stopped), the System map's Hub, `hub_refused`, and the webhook on 7683 apart from the relay (D100) | B6 | done; tested with the faux provider and over loopback |

## Open items

1. **A real-model baseline for the benchmark.** `nypi eval` and `nypi eval --three-planes` have only run with
   scripted models; no run with real models is recorded, so neither suite has numbers to compare against yet.
2. **Physical devices.** Pairing, remote folders, and races are tested over loopback; nothing has run between two
   real machines or over a tailnet, and the Android and iPhone nodes (M6 steps 4 and 5) are not built.
3. **A watched-file schedule condition on another device.** A `fileChanged` condition set at a folder on another
   device is not taken (command conditions are).
4. **UDP hole punching.** Two machines that are both behind routers with no forwarded or mapped port reach each other
   only through the relay (decision D94).
5. **A node-to-node path for large files.** Nodes talk only to the hub, so a large file between two devices goes
   through the hub (and the relay, when either is away).
