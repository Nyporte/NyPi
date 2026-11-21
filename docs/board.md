# NyPi Board: product design

Status: **accepted** as decision D56; phase 1 is built (it supersedes the read-only viewer of decision D55), and the
System screen of decision D65. Serves principles A1
(observability), A2 (comprehensibility), A3 (alertability), A4 (traceability), A5 (priority response), and B6 (the
action chain never breaks silently).

## 1. Who it is for, and what they need

One user: the user of a long-lived chief of staff. They hand Lucia work from the terminal, the TUI, Telegram, and
schedules; Lucia hands parts of it to specialists, reflects, remembers, and acts on her own when idle (DMN). The
user is often away from the machine, and checks in from a phone.

The board answers, in this order of urgency:

| # | The user's question | Why it matters | Principle |
|---|---|---|---|
| Q1 | Does anything need me right now? | An approval or a blocked task waits on them; nothing else moves until they act | A3, A5, B6 |
| Q2 | What is Lucia doing right now, and is it going well? | Trust: catching a wrong turn early is cheap, late is expensive | A1 |
| Q3 | What happened while I was away, and how did it end? | Every run ends Done, Blocked, Failed, or Continuing; none may end silently | B6 |
| Q4 | Why did she do that, and on what evidence? | Checking an answer means seeing its sources, steps, and the model's reasoning | A2, A4 |
| Q5 | Who did the work, and how well? | Lucia delegates to specialists and is accountable for them | B2 |
| Q6 | What does she know and believe, about the world and about me? | Memory, portrait, self-prompt, and lessons shape every future answer | A8 |
| Q7 | Where is she heading on her own? | Goals, schedules, and idle-time passes are how she uses time leverage | B4 |
| Q8 | What does it cost, and is the machinery healthy? | Tokens, retries, failing tools, MCP servers, the watchdog | A3 |

Design rules that follow from these:

1. **Act where you look.** A pending approval shows its context and the approve/deny buttons together. Seeing a
   problem without being able to stop it is the frustration this board exists to remove.
2. **Glance, story, evidence.** Every object has three zoom levels: a status line (glance), a narrative of steps
   (story), and the full payloads (evidence). Depth is chosen per object by expanding it, not by a global switch.
3. **Live by default.** What is running updates as it happens, including the reply as it is generated, without
   reloading.
4. **One timeline of cause and effect.** Everything links to what caused it and what it caused (pulse causation),
   across processes and agents.
5. **Never the bottleneck.** Watching must not slow a run. A process publishes to the board only while a board is
   connected, never blocks on it, and drops what the board cannot take.
6. **Phone first for the inbox and the live view; desk first for evidence.**

## 2. Screens

### 2.1 Home: "Now"

The landing page answers Q1 and Q2 on one screen.

- **Inbox** (top, only when non-empty). One card per item that waits on the user, newest first:
  - an approval request: tool, summary, why it was flagged, the task it belongs to, how long it has waited;
    *Approve*, *Deny* (with a reason), *Open task*;
  - a task Blocked on the user: the question Lucia asked; *Reply* (sends a message to that task), *Open task*;
  - a stalled or failed task: what went wrong, the watchdog's plan; *Resume*, *Cancel*, *Open task*;
  - an unread alert; *Acknowledge*.
- **Live work.** One card per running process (the hub, a terminal with a session of its own, a scheduled run,
  the DMN, each specialist), showing:
  - who (agent and process), the task's goal, elapsed time, the model;
  - the current step: *thinking*, *writing* (with the last lines of the reply streaming in), *running `bash: cargo
    test`* (elapsed), *waiting to retry (2/3, 4 s)*, *compacting*, *waiting for your approval*;
  - a context-window gauge and tokens/cost so far;
  - *Open* (live task view), *Stop*, *Message* (steer).
- **Today strip.** Runs by outcome (Done / Blocked / Failed / Continuing), tool calls and failures, retries, tokens
  and cost, compared with the 7-day average.
- **Up next.** The next three schedule firings and the next DMN window.

### 2.1a The three planes (decision D86)

Since D84 the hub thinks in three planes, and the board shows each step by step. Five screens lead the navigation:
Now, Chat, Principal, Vault, System (Activity stays reachable).

- **Now** adds a live strip of three cards. *Chat*: its model, the newest exchange, and today's turns, lookups,
  lookups that ran out of time, and signals. *Principal*: idle, or the active activation with its priority (`P0
  User`, `P1 Event`, `P2 Idle`), topic, and steps so far; the suspended stack, each with the steps it keeps; the
  queue by priority. *Tasks and campaigns*: the tree of tasks under the goal each serves (its id, due date, and a
  link to its `plan.md`), tasks for no goal last, each with its state, place, and a link to its story.
- **Chat** (`/chat`) is the conversation as chat's turns, live. A turn says where it came from (the user, with
  the device and folder they were at; or the Principal, with the stimulus: kind, headline, every fact to convey,
  the question, the note to name), the memories recalled for it, chat's reasoning, each lookup (tool, arguments,
  time in ms, a badge when it ran out of time, its output), each signal as a pill that opens the activation it
  became, and the reply. Stimuli chat skipped are hidden unless *Show skipped* is on. A message box at the bottom
  talks to Lucia in the hub's conversation (the `say` action; it needs the hub).
- **Principal** (`/principal`): the stack and the queue, then every activation, newest first, filterable by
  priority, with its decision, tokens, cost, and the tasks and goals it touched.
- **An activation** (`/activation/:id`), live while it runs: priority, kind, state, times, tokens, cost, the
  decision, what woke it (the hand-over with the conversation, the settled task, the material, the schedule, or the
  goal digest), then its steps: reasoning and text, each tool call with its arguments and output (`start_task` and
  the other task tools link the task), what it had chat say, and banners where it was suspended (by which
  activation, after how many steps), resumed (with what was decided meanwhile), or joined.
- **A task** gains links up to the goal it serves and the activation that started it, where it works, how many
  times it was sent back, drawers with the goal's plan and the workspace's brief and findings as they read now, a
  device chip on each call made on another device, the observer's steers as callouts, and the Principal's verdicts
  (accepted, or sent back) where they fell.
- **Vault** (`/vault`): the goals by term with due date, next step, tasks, and the plan; then the vault's folders
  and their notes, each read in place (`/vault?path=…` links straight to one).
- **Join requests (decision D93).** A machine asking to join the hub is an inbox card on Now, first in the list:
  who asks (name and system), from which address, and its six-digit code, with **Admit as my computer**, **Admit as
  exec-only**, and **Refuse** (actions `admit` and `refuse`). The card says to admit only when the code is the one on
  the new machine's screen. The first answer anywhere counts. A browser asking through the entry is a card of the
  same kind (decision D94), naming its user agent and address, admitted as the user's own or refused.
- **Live and dossiers (decision D88).** While chat answers or the Principal works, the Now cards, the Chat screen
  (an in-flight row at the bottom), the Principal's active card, and the activation being written show the step
  (`Thinking`, `Writing`, `Reading vault_search`, `Calling start_task`) and the text as it is written. A task shows
  its workspace's three notes (brief, findings, `report.md`) as drawers, the harness's evidence manifest, its cited
  references as badges (green when they resolve to a call, amber when they match nothing), and a warning when it
  changed a file and said Done without checking it. An activation shows its briefing pack (closed by default: each
  plan, note excerpt with its links, memory, and asset that was put before it, and how long the lookup took), its
  disposition as a badge (also on the Principal's rows), and the harness's nudge where it fell.
- **System** draws the planes: chat, the Principal, the workers, the observer, the inbound webhook, and the planes'
  logs, with the connections between them (chat signals the Principal, the Principal has chat speak, starts
  tasks, keeps the vault and goals; chat reads files on devices; the scheduler and idle time wake the Principal).

### 2.2 Task view

The story of one task, live while it runs (Q2, Q3, Q4, Q5).

- **Header**: goal, status (one of the four B6 states with its text), owner and origin, started, duration, attempts,
  cost, parent task and child tasks (delegations).
- **Steps**, as a vertical story rather than a log:
  - the prompt (with memories the valve attached, marked);
  - each turn: the model's reasoning (collapsed), what it said, each tool call as one line (`read src/main.rs ✓
    0.2 s`, `bash cargo test ✗ exit 101`) that expands to arguments and full output;
  - harness events inline where they happened: retry, compaction, context recovery, approval asked and answered,
    prompt updates, steering messages from the user;
  - a delegated child task as a nested, collapsible story with its own status.
- **Report**: the final answer, its citations resolved to the tool calls they point at (click jumps to the step), and
  the status line.
- **Evidence** tab: the raw audit records and session entries for the task.
- While running: the reply streams into the current step; *Stop*, *Message*, *Approve/Deny* sit in a sticky bar.

### 2.3 Activity

Everything that happened, across processes and agents, newest first (Q3). Filters as chips: agent, origin
(terminal, Telegram, schedule, DMN, specialist), outcome, time range, text. Rows are tasks and notable events
(alerts, approvals, schedule firings, reflections, DMN notes), each opening its task view or detail.

### 2.4 Agents

Who does the work (Q5): Lucia (worker), the chat module, Thinking, the DMN, and each specialist. Per agent: model,
state (idle / working on X), tasks this week with outcomes, success rate, average duration and cost, and for
specialists the experience log and profile.

### 2.5 Knowledge

What Lucia knows and believes (Q6), read-only in the first release:

- **Memory**: recent memories, search, and per message what the valve recalled and attached;
- **Portrait**: traits with the user's own words behind them;
- **Self-prompt**: the current text and its version history as diffs;
- **Lessons**: reflections and skill proposals waiting for review.

### 2.6 Plans

Where she is heading (Q7): goals by horizon with their progress log; schedules (timers and conditions) with last and
next firing; DMN passes with their notes and the daily budget used.

### 2.7 Health and cost

Q8: tokens and cost by day, model, and agent; retries and failures by provider; slowest and most failing tools; MCP
servers and their tools; notification channels; watchdog checks; Inspector guard blocks and approval decisions.

### 2.8 Audit

The audit log itself: search, filter, and the causal trace of any pulse (what `nypi trace` shows), for when the story
is not enough.

### 2.9 System

The whole program as a map (decision D65), for the question the other screens do not answer: what is NyPi made of,
how do the parts connect, and which of them is alive right now.

- **Regions**: *You* (the ways in: terminal, send/attach, Telegram, the board, other programs), *Always on*
  (supervisor, hub, watchdog, scheduler, idle passes, outbox), *Lucia* (one runtime: chat, the Principal, worker, thinking,
  recall, prompt, reflection, portrait, goals, compaction, pulse bus, Inspector), *Tools and models* (models, tools, sandbox,
  MCP servers, specialists, skills), and *Kept on disk* (sessions, task ledger, audit log, memory, settings and
  trust). Ways in are on top, stores at the bottom, Lucia in the middle between what keeps her running and what she
  works with.
- **Modules** are boxes with a name, a few words on what they are doing, and a state: *working* (blue), *ready*
  (green dot), *not open* (hollow dot: it exists only while used), *off* (dashed: turned off or not set up), *needs
  attention* (red: it should be there and is not, or it holds something that has not reached you).
- **Connections** are lines with an arrowhead, of five kinds: messages, work handed on, watching, records, and
  notices. A connection something is going over right now moves. Chips show one kind at a time.
- **Pointing** at a module lights its connections and dims the rest; **selecting** one opens what it is for, its
  numbers, what it acts on and what acts on it, where it lives in the code, and a link to the screen that shows
  more (the running task, the activity list).
- Below the map the same modules are a list by region, which is the map on a phone.

The map is read again every few seconds while the screen is open.

## 3. How the board gets its information

Two kinds of information, two paths.

**What NyPi stores** (sessions, the task ledger, the audit log, memory, goals, schedules, portrait, DMN passes,
reflections, specialists, the self-prompt history) is read from disk by the board, read-only. Since decision D84 that
includes the three planes' logs: every chat turn (`chat/turns.jsonl`), every step of each of the Principal's
activations (`principal/activations/<id>.jsonl`, indexed in `stores/activations.jsonl`), and the Principal's
decisions (`stores/decisions.jsonl`), each written scrubbed of secrets. The Principal also keeps
`principal/status.json` (its process id and its stack and queue) current, so the board, in a process of its own,
shows what it is doing now; when that process is gone, the board shows its activations as stopped. Every answer of
the API and every event of the live stream goes through the secret store's scrubber before it leaves the board:
what the board shows comes from models and tools, which may have written a secret. Since D87 that scrubber follows
the store (`LiveScrubber`): a secret set while the board runs is taken out of the next answer. Since D85 a task's record and a schedule
name the goal they serve and the activation that started them (`goalId`, `spawnedByActivation`), and a goal names
its project folder (`vault`), which is what ties goals to tasks on the board. The screens that show them are D86's.

**What the program is made of** (the System screen) is written out in the board service
(`crates/nypi-cli/src/board/topology.rs`): the modules, their regions, and their connections. That structure lives
in the code, so it is kept honest by a test, not by discovery: every module names a source file that must exist,
every file of the core crate must be a module or be listed as part of one, and every connection must join two
modules. The state of each module is read fresh from the linked processes, the supervisor's and the hub's
heartbeats, the settings, the stores, and the last day of the audit log.

**What only exists inside a running process** (the current step, the reply being generated, a pending approval, the
steering queue, context usage, a retry wait, MCP connection state) is published by that process:

- The board service listens on `~/.nypi/run/board.sock` (owner-only, Inspector-protected, like the bridge socket).
- Every NyPi process with a runtime (terminal, TUI, RPC, Telegram, print runs, native specialists) has a **board
  link**: when the socket exists it connects and announces itself (pid, front end, agent, session, cwd); when it does
  not, the link does nothing.
- The link sends a **snapshot** on connect and then **events**: the runtime's existing events (agent events,
  pulses, retry and compaction notices, alerts), plus state changes the runtime already tracks (task started and
  settled, approval waiting and answered, queue changes, context usage after each turn).
- Streamed reply deltas are sent only while a board client is viewing that process (the board tells the link).
- **Non-interference**: the link is a bounded channel drained by its own task. A full channel drops events and
  counts the drops; the agent never waits on the board. No board, no cost beyond one failed connect at start and a
  retry every 30 s.
- **Actions** travel back on the same socket: approve or deny a pending approval, stop the run, steer it with a
  message, acknowledge an alert. Each is an explicit user action recorded in the audit log, routed into the
  mechanisms that already exist (the approval desk, the abort handle, the steering queue). Tasks of a dead process
  are resumed or cancelled through the ledger, as `nypi tasks` does.

The board is a long-lived service (`nypi board`, with a launchd script like the Telegram bridge's), so it is there
when processes start. It keeps a bounded in-memory index of recent activity, rebuilt from disk at start.

**The API (decision D86).** `/api/now` (with `planes` and `campaigns`), `/api/chat`, `/api/principal`,
`/api/activation/:id`, `/api/task/:id` (with the goal, activation, place, plan, brief, findings, rounds, steers, and
verdicts), `/api/vault` and `/api/vault?path=…` (a note, read through the vault's own path checks), `/api/system`
(the map; `/api/topology` is the same), `/api/activity`. The live stream `/api/live` takes `chat=1` (a `chat` event
when a turn is added) and `activation=<id>` (an `activation` event, with the Principal's card, while it grows),
besides `task=` and `watch=`; the planes' logs are files another process writes, so the stream looks at them every
0.9 s. The `say` action sends the user's words to the hub's conversation as an interface does. Since D88 every
stream also carries `plane` events (what chat or the Principal is doing, from the hub's board link, `idle` when
it ends), `/api/now` and `/api/principal` put the current one on the planes' cards (`live`), `/api/task/:id` adds
`report`, `manifest`, `verifiedRefs`, `unknownRefs`, and `uncheckedChange`, and `/api/activation/:id` adds
`briefing`, `disposition` on its row, and `nudged` steps. Since D89 `/api/chat` and the `chat` event carry `live` too, so the Chat
screen opened in the middle of a turn shows it at once. A benchmark run (`nypi eval`, also with `--three-planes`, decision D90)
works in temporary homes of its own and never shows on the board.

## 4. Access

- Served on the LAN behind a token, as decided for D55 (HttpOnly cookie after a one-time link); `--local` for this
  Mac only.
- Through the entry (decision D94): a phone opens the user's own entry (`https://nypi.example.com` here; without one,
  decision D97, the board is on the LAN only); the relay passes the request to the hub,
  and the hub to the board's public socket (`run/board-public.sock`), where the local token is never taken. A browser
  without a session sees only a page asking to be admitted, with a six-digit code; the user is asked wherever join
  requests go and admits it (always as their own) or refuses it; admitted, the page goes on by itself and keeps a
  session cookie (`HttpOnly`, `Secure`, `SameSite=Strict`) whose hash is in the register of peers. The live stream works
  through the entry too, unbuffered. `nypi devices remove <name>` ends a session at once, the live stream included.
  Every answer there goes through `Board::scrub`, as on the LAN.
- Each device card says the NyPi version the machine runs (decision D98), so a fleet update can be followed there.
- The board runs where the hub runs (decision D95): a move of the hub stops the old machine's board, and the new hub's
  supervisor starts its own. Browser sessions are in the register of peers, which moves with the hub, so an admitted
  phone needs no new code after a move.
- Actions additionally require the request to come from the board's own page (same-origin, SameSite=Strict cookie,
  and a per-session CSRF token), since they change what the agent does.
- The notifier links to the board: a Telegram or desktop notice about an approval or a stall opens its card.

## 5. Delivery in phases

1. **Live core**: the board service, the board link in every runtime, Home (inbox with approve/deny/stop, live work
   with streaming), the task view, and Activity. This is what makes the board worth opening.
2. **Agents and Health**: per-agent and cost views, retries and tool failures, MCP and watchdog state.
3. **Knowledge and Plans**: memory, portrait, self-prompt history, lessons, goals, schedules, DMN.
4. **More actions**: resume/cancel from the inbox, edit goals and schedules, review skill proposals.

## 6. Decisions the user made

- **Front end**: Rust compiled to WebAssembly (Leptos, `crates/nypi-board-ui`), sharing its wire types with the
  service (`crates/nypi-board-proto`); built by `scripts/build-board.sh` and embedded in `nypi`.
- **Actions in phase 1**: approve, deny, stop, and message (steering a running task), plus clearing inbox items.
- **Reply streaming**: yes, while a viewer watches; the Now screen watches every process while open, a task view
  watches the process running it.

## 7. What phase 1 left out

- Resume and cancel from the inbox, and replying to a blocked task, are phase 4: the inbox links to the task and
  says where to answer.
- The Evidence tab (raw records of a task) is not built; the story keeps tool output up to 20,000 characters.
- The notifier does not link to the board yet.
