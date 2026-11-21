# NyPi Architecture

This document describes NyPi as it is built today: the crates, the kernel ported from Pi, and the hub with its three
planes. Every design traces to [principles.md](principles.md); what is planned is in [roadmap.md](roadmap.md), and
how each part came to be is in the decision log (§5). The multi-device design has its own document,
[devices.md](devices.md), and so does the board, [board.md](board.md). Upstream background is in
[pi-analysis.md](pi-analysis.md).

## 1. Language: Rust

NyPi is a Rust reimplementation of Pi, not a port of the Node runtime: Rust edition 2024 on Tokio, built as one
`nypi` binary with no runtime to install (decision D1). The deciding reasons: Pi's design is tagged unions (messages,
events, states, results), which `enum` and an exhaustive `match` check at compile time; an always-on process gains from
having no garbage collector and no data races; Tokio runs the pulses, the planes, concurrent tasks, and tool calls on
one runtime with structured cancellation; and local models run in process (`candle`, for the embedding model of
§4.4). The cost, slower development than Go, is accepted.

## 2. Repository layout

```
NyPi/
  Cargo.toml              the Rust workspace
  crates/
    nypi-ai/              message model, streaming events, provider adapters, retries, embeddings
    nypi-agent/           the turn loop, the tool trait, hooks, steering and follow-up queues, cancellation
    nypi-vault/           what NyPi keeps: episodic memory, the Markdown vault and its index, assets, secrets
    nypi-core/            tools, sessions, compaction, prompts, skills, settings, the Inspector, the runtime, the planes
    nypi-hub/             the hub: the one conversation, jobs, channels, the protocol, notices, the register of nodes
    nypi-cli/             the `nypi` binary: command line, terminal and full-screen interfaces, supervisor, board
    nypi-eval/            the offline benchmark: sixteen scenarios, on a worker alone or through the three planes
    nypi-hub-proto/       what an interface and the hub say to each other (serde only)
    nypi-node/            what carries out a hub's calls on one device; no model, session, or provider code
    nypi-node-proto/      what a hub and a node say to each other (serde only)
    nypi-board-proto/     the board's wire types (serde only)
    nypi-board-ui/        the board's page, Rust compiled to WebAssembly (Leptos)
  docs/                   this documentation (normative); docs/plans/ holds the completed stage plans
  scripts/                model catalog generator, board build, git hooks
../Pi/                    sibling checkout of upstream Pi (TypeScript): a read-only reference, not built (D18)
```

**Layering.** `nypi-ai` <- `nypi-agent` <- `nypi-vault` <- `nypi-core` <- `nypi-hub` <- `nypi-cli`. A crate never
depends on one to its right. `nypi-eval` sits beside `nypi-hub`: it depends on `nypi-core` and below, and only
`nypi-cli` depends on it. `nypi-hub-proto` is used by `nypi-hub` and `nypi-cli` and depends on nothing of NyPi.
`nypi-vault` knows nothing of the runtime. `nypi-node` and `nypi-node-proto` depend on no model, session, or provider
code, so they build for a phone; `nypi-board-ui` depends on nothing else of NyPi but `nypi-board-proto`.

Notable module folders:

| Folder | What is in it |
|---|---|
| `nypi-core/src/runtime/` | The `Runtime` every run goes through: `mod.rs` (the type, building one, prompt sources, place context), `run.rs` (a prompt becomes a run), `tasks.rs` (the ledger side, resuming), `sessions.rs`, `controls.rs` (model and settings), `mcp.rs` |
| `nypi-core/src/principal/` | The Principal: `mod.rs` (triggers, priorities, the handle), `runner.rs` (the queue, suspension, the four rings, dispositions), `tools.rs` (`stimulate_chat`, the pool's tools, `conclude_activation`, `inspect_task_evidence`, `tasks_inspect`), `journal.rs` (the decision journal, activation stories, the status file) |
| `nypi-core/src/tools/` | The device tools (`read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`), command tools, `http_request`, and Lucia's own tools (`memory`, `goals`, `schedule`, `self_prompt`) |
| `nypi-core/prompts/` | `core.md` (the worker's read-only core), `chat.md`, `principal.md` |
| `nypi-hub/src/daemon/` | `mod.rs` (the socket, the process, how machines reach the hub), `primary.rs` (the one conversation), `threads.rs`, `pump.rs`, `local.rs`, `remote.rs` (what a machine's link may do), `entry.rs` (the hub at the relay), `peer.rs`, `heartbeat.rs` |
| `nypi-hub/src/` (devices) | `instance.rs` (what this machine is), `pairing.rs` (the register, joining, links), `reach.rs` (how a node picks its way to the hub), `direct.rs` (the hub's direct addresses, port mapping), `mdns.rs`, `ws.rs` (WebSocket), `relay.rs` (`nypi relay run`) |
| `nypi-hub/src/channels/` | The `Channel` trait (`mod.rs`), Telegram (`telegram/mod.rs`, `telegram/api.rs`), and the inbound webhook (`webhook.rs`) |
| `nypi-cli/src/board/` | The board service: `hub.rs` (the live picture), `planes.rs`, `records.rs`, `topology.rs` (the System map), `web.rs`, `public.rs` (browsers through the entry) |
| `nypi-cli/src/tui/` | The full-screen interface: `app.rs` (state from records), `view.rs` (drawing), `input.rs` |

## 3. Kernel and coding layer

The kernel (`nypi-ai`, `nypi-agent`) is a faithful translation of Pi's `ai` and `agent` packages; the coding layer
(`nypi-core`) follows Pi's coding agent where it overlaps and adds NyPi's own parts.

### 3.1 Message model (`nypi-ai::types`)

```rust
enum Message { User(UserMessage), Assistant(AssistantMessage), ToolResult(ToolResultMessage), System(..) }
enum ContentBlock {
    Text{text, text_signature}, Thinking{thinking, signature, redacted},
    ToolCall{id, name, arguments, thought_signature}, Image{data, mime_type},
}
enum StopReason { Stop, Length, ToolUse, Error, Aborted }
```

Field names serialize in Pi's camelCase, so session files stay Pi-compatible. Signatures are opaque replay data under
Pi's names (`textSignature`, `thinkingSignature`, `thoughtSignature`): an Anthropic thinking signature, an OpenAI
Responses reasoning item or message id, a Gemini thought signature, or Bedrock's encrypted reasoning.

**The system prompt travels in the transcript** (decision D40, `nypi-ai/src/transcript.rs`, from Pi's
`utils/transcript.ts`). `Message::System` holds `content`, ordered named `sections` (`null` removes one), and
`toolsAdded`/`toolsRemoved`. A new session's first run records a leading system message with every prompt section
and every tool. Before later runs the runtime appends only the sections that changed, and before each request the
agent loop appends the difference between the tools the transcript declares and those that can run. Providers then
prepare the request from the transcript:

- With the model's `supportsMidConvoSystemMessages` compat flag, the head stays as it was (the cached prefix) and
  updates go in place: as instruction-role messages for Chat Completions and Responses, and for Anthropic as `system`
  messages held until after the next user message (one between a tool call and its result is rejected).
- Otherwise, and for every model not verified, the transcript is folded into one leading prompt with the same
  sections (a section new since the head comes last). Gemini and Bedrock always fold.
- Tools always travel as the current set in the top-level field; Pi's in-place tool changes are not ported.
- Compaction folds the summarized part's system messages into a new head.

### 3.2 Provider contract

```rust
trait Provider: Send + Sync {
    fn stream(&self, model: &Model, ctx: &Context, opts: &StreamOptions, cancel: CancellationToken)
        -> BoxStream<'static, StreamEvent>;
}
```

- **Never fails out of band.** Every failure is a final `StreamEvent::Done` whose message has
  `stop_reason = Error | Aborted` and an `error_message`.
- Events: `Start`, `TextDelta`, `ThinkingDelta`, `ToolCallStart`, `ToolCallDelta`, `ToolCallEnd`, `Done`, each delta
  with its content index. Unlike Pi, events do not carry the partial message (copying it per delta is costly);
  consumers that need it rebuild it with `MessageAccumulator`.
- Adapters, chosen by `model.api`:

| `api` | What it speaks |
|---|---|
| `anthropic-messages` | Messages API over SSE, thinking, prompt-cache breakpoints on the system prompt and last tool |
| `openai-completions` | Chat Completions over SSE; `base_url` makes it any OpenAI-compatible server |
| `openai-responses` | Stateless Responses API (`store: false`): reasoning items replayed verbatim, message ids and phases in `textSignature`, tool call ids `call_id\|item_id`, `prompt_cache_key` = session id |
| `google-generative-ai` | Gemini's REST stream, key in `x-goog-api-key`; thought signatures go back only to the same model; Gemini 3 gets `thinkingLevel`, 2.5 a token budget |
| `bedrock-converse-stream` | Converse stream REST endpoint, SigV4-signed (or a Bedrock API key as bearer), AWS's binary event stream decoded and CRC-checked here |
| `faux` | Scripted responses for tests (by order, or chosen from the request so several agents on one model are scripted each) |

- Every adapter shares `post_json`: cancellation, and a request that never reached a server (connection failed or
  timed out) is sent again up to three times. A status the server answered with (429, 5xx) is returned at once with
  its `retry-after` named, for the runtime's single retry layer (§3.6) to decide on (decision D73). Streams stall out
  after 300 s. Provider keys are redacted from `Debug` output.
- The model catalog is generated from Pi's (`scripts/nypi-gen-models.py` writes `crates/nypi-ai/data/models.json`);
  user-defined models are added in settings. Keys come from environment variables or `~/.nypi/auth.json`.
- Not ported from Pi: Vertex AI, Azure OpenAI, Mistral's API, Codex and Copilot sign-in, grammar tools, tool search,
  service tiers.

### 3.3 Agent loop (`nypi-agent`)

The same algorithm as Pi ([pi-analysis.md §3](pi-analysis.md#3-agent-the-turn-loop)):

- turns continue while there are tool calls or queued messages;
- steering is drained after each turn, follow-ups when the agent would otherwise stop (`steeringMode`,
  `followUpMode` set how many one drain takes);
- `before_tool_call` can block a call and `after_tool_call` can rewrite its result;
- tool calls run in parallel by default, in sequence when the config or a tool asks;
- a `length` stop fails every tool call in that message;
- `TurnHooks::before_next_turn` runs before each turn after the first (Pi's `prepareNextTurn`): it may replace the
  transcript and add messages ahead of queued ones. The runtime's hook compacts when needed and brings the prompt's
  sections up to date;
- arguments are checked against the tool's JSON Schema with lossless coercions (`"5"` -> 5);
- the loop is cancelled through a `CancellationToken`.

The loop emits `AgentEvent`s on one channel; the Inspector, the ledger, the board link, and every interface consume
the same stream (principle A1).

```rust
#[async_trait] trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;           // name, description, JSON schema
    fn execution_mode(&self) -> ExecutionMode { Parallel }
    async fn execute(&self, call_id: &str, args: Value, ctx: ToolContext) -> ToolOutput;
}
```

Tools return errors as results the model reads; nothing a model sends can panic the harness.

### 3.4 Coding layer (`nypi-core`)

- **Device tools**: `read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`, with Pi's limits (2,000 lines or 50 KB;
  `bash` keeps the tail and spills the full output to a temp file, kills the process group on abort). Paths resolve as
  in Pi (decision D51): Unicode space variants become spaces and a leading `@` is dropped; when nothing is at the
  path, `read` also tries the forms macOS stores names in (a narrow no-break space before AM/PM, decomposed accents,
  a curly apostrophe). `edit` accepts its edits sent as a JSON string. These tools belong to a device and are called
  through the runtime's `Place` (§3.7).
- **Sessions**: Pi's JSONL tree format (a header, then entries with `id`/`parentId`), under `~/.nypi/sessions/`: the
  hub's conversation in `sessions/primary/`, others as `sessions/<cwd-slug>/<timestamp>_<id>.jsonl`. The active branch
  is the path from root to leaf. Pi's `context_edit` entries omit or replace an earlier message in model context
  (newest edit wins; the message stays in the file); `session_info` names a session; entries NyPi cannot parse are
  kept as written; Pi's extra message kinds reach the model as Pi's `convertToLlm` turns them into user messages
  (decision D50). A fork or clone writes a new file holding one path of the tree with its parent named (decision D49).
  Trees are built without recursion, and the RPC writer runs on a 256 MB stack, since an unbranched session nests as
  deep as it is long. `export_html` writes a static page with no script (decision D53).
- **Compaction**: Pi's algorithm and checkpoint prompt. The threshold (`compaction.reserveTokens` 16,384,
  `keepRecentTokens` 20,000) is checked before a prompt, after it, and between turns (decision D45), so a long run of
  tool calls compacts before it overflows.
- **Context recovery** (decision D44): Pi's overflow detector (`nypi_ai::overflow`) recognizes an overflow from
  provider error wordings, from a completed reply whose input exceeds the window, and from an empty length stop with
  the window full; it also recognizes a reply cut below the output limit the request asked for. The runtime then takes
  the failed reply out of context with a `context_edit` entry, compacts, and continues once; a second failure raises
  an alert.
- **Thinking levels** (decision D46): as in Pi, a level the model's catalog entry does not map is clamped to the
  nearest accepted one (higher first); a model switch starts from `defaultThinking`, then clamps.
- **Skills**: the Agent Skills format (`SKILL.md` with `name` and `description`), loaded from `~/.nypi/skills`,
  `~/.agents/skills`, `~/.pi/agent/skills`, `skillPaths`, and a trusted folder's `.nypi/skills` and
  `.agents/skills`. Only the index goes into the prompt; the model reads a skill when it needs it.
- **Settings**: `~/.nypi/settings.json` (camelCase keys), with a trusted folder's `.nypi/settings.json` on top; the
  agent name (default `Lucia`), default model and thinking level (`medium`), and the settings of every part named
  below. `envFiles` (default `~/.env`) are read into a private lookup, never exported (decision D76). A file
  written before decision D100 may still hold a `daemon` section: its `enabled`, `cwd`, and `telegram` are read as
  `hub.enabled`, `hub.cwd`, and `channels.telegram.enabled` wherever the same file does not set those itself
  (`fold_daemon`, in `config.rs`).
- **Project trust** (decision D61, `trust.rs`, from Pi's `trust-manager.ts`). A folder's own configuration
  (`.nypi/settings.json`, `.nypi/specialists`, `.nypi/skills`, `.agents/skills`) can turn approvals off, add programs
  as tools, and start MCP servers, so it is read only from a folder the user trusts. `~/.nypi/trust.json` maps
  canonical paths to true or false; the nearest ancestor with a decision wins. A terminal asks once at startup; every
  other way of starting leaves the configuration out with a warning naming what was left out. `nypi trust` shows and
  changes it (`yes`, `parent`, `no`, `forget`). A folder's trust, skills, and specialists are read again before each
  run of an open conversation; its other settings when the conversation opens again.
- **Command tools**: `commandTools` declares external programs as tools (`tools/command.rs`): a description, a JSON
  Schema, and an argv template where `{name}` takes argument `name`, with no shell, so an argument cannot start a
  second command. They share `bash`'s process handling and its sandbox, and are offered at the hub's own folder only.
- **MCP** (`mcp.rs`): stdio servers, Streamable HTTP (`url`), and the legacy HTTP+SSE transport (`"transport": "sse"`,
  or as a fallback when the initialize is refused with 400, 404, or 405). Tools appear as `mcp__<server>__<tool>`;
  servers with resources get `list_resources` and `read_resource`; prompt templates become `/mcp:<server>:<prompt>`
  commands. A `tools/list_changed` notice re-lists the server's tools before the next prompt. Recovery follows Pi's
  transport: a 404 on a known session starts a new session and retries once, a read-only request (never a tool call)
  is retried once after a transient failure, and the server-to-client GET stream reconnects with backoff (1 s doubling
  to 30 s, five tries) and `Last-Event-ID`. `${NAME}` in `env` and `headers` resolves through the private lookup. A
  runtime beside the conversation connects its servers in its own task, so a slow server never holds a task's start.
  There is no OAuth for remote servers.

### 3.5 Inspector

The Inspector (`inspector.rs`) observes and enforces, and Lucia cannot change it: it lives in the harness, no tool
exposes it, and its files are among those no tool may write (principle A1).

**Audit log.** Every `AgentEvent` (user input, model output with thinking, each tool call and result, errors), every
pulse (§4.1), and every harness decision is appended to `~/.nypi/audit/<date>.jsonl` with session, turn, and time;
each record written during a run carries `cause`, the pulse that started it. No tool receives its path.
`audit_log.rs` reads it for `nypi trace`, the scorecard, and the board without touching it.

**What tools are kept away from** (decision D62, `protected.rs`). One definition, `Protected`, shared by the guard
and the sandbox:

- may not be written: NyPi's home (`~/.nypi`: audit log, ledger, outbox, prompts, settings, trust store, vault,
  specialists, the supervisor's binary, ...), any `.nypi` folder under the user's home (in any letter case), the
  user's own skill folders (`~/.agents/skills`, `~/.pi/agent/skills`), and the user's service folders on both
  systems (decision D99): `~/Library/LaunchAgents` and `~/.config/systemd/user`, with the supervisor's own plist and
  unit in them, so no tool plants a service that would run outside the sandbox at the next login (they may be read; on
  Linux the unit folder is made when missing, since bubblewrap keeps only what exists read-only);
- may not be read either: `auth.json`, the env files, `run/` (tokens and sockets), `secrets.enc`, `secrets.key`, the
  machine keys (`hub-id.key`, `device-id.key`, and a D80 `interface-id.key` until it is forgotten), unused invite
  codes, join requests, a standby copy of the hub's state (`standby/`), a hub's state set aside (`diverged/`), and env
  files carried from the hub's machine (`env/`);
- `self.md` and the goals file change only through their versioned tools; the vault only through the `vault_*`
  tools, in process (§4.4).

**Guard.** A `before_tool_call` hook that blocks what is never acceptable: destructive `bash` patterns (`rm -rf /`,
`mkfs`, `dd of=/dev/`, fork bombs, `chmod -R 777 /`, writes to `~/.ssh`, ...), writes to what `Protected` names,
and commands that are the user's alone (`nypi trust`, `nypi supervisor`, `nypi hub`, `nypi node`, `nypi relay`,
`nypi devices`, `nypi send`, `nypi attach`, `nypi rate`, `nypi secrets`, `nypi pair`, `nypi update`), whatever path
`nypi` is run by and whatever global flags come before the subcommand (`~/.nypi/bin/nypi --mode text update`,
decision D99). A block is an error result the model reads
and an `Alert` the user sees (principle A3).

- *Exact for tools that name a path* (decision D69). `write`, `edit`, `read`, `grep`, `find`, `ls`, and `delegate`'s
  `cwd` are resolved as the tool resolves them and then followed as the kernel does (`real_path`: component by
  component, symlinks replaced by their targets, `..` applied to the folder reached), so a dangling link, a leading
  space, or a curly apostrophe names the file it reaches. `grep` also passes over key files in a folder it searches.
- *The file tools check again where they open.* `read`, `write`, and `edit` run in the harness's process, so each
  holds `Protected` itself, resolves its path once, and opens the resolved path: a link made meanwhile by a parallel
  `bash` call does not change which file is reached. A node on another device runs the same tools.

**Tool sandbox** (`sandbox.rs`). A shell can spell a path endlessly, so every subprocess the agent can start (`bash`,
command tools, a schedule's command condition) goes through `Sandbox::command`, where the operating system judges the
file a call really reaches:

- *macOS*: `sandbox-exec` with a Seatbelt profile that allows everything except the writes and reads `Protected`
  forbids, and connecting to any socket in `run/`.
- *Linux* (decision D76): `bwrap` (bubblewrap), when installed and the kernel allows unprivileged namespaces (probed
  once): the system as it is, with NyPi's home, the skill folders, the protected files, and the `.nypi` folders at and
  above the working folder mounted read-only, and the secrets and run folder masked. Bubblewrap covers only what
  exists when a command starts, so on Linux the guard also reads a command's text (`Sandbox::judges_every_path` is
  true for Seatbelt only). Not yet run on a real Linux kernel here.
- *Without a sandbox* (another system, or NyPi already inside one) the guard refuses a `bash` command that names
  NyPi's home or a `.nypi` folder beside anything that writes, and a warning says so at startup. `"sandbox": "off"`
  turns it off.
- *A clean environment* (decision D76). `sandbox::scrub_env`, applied by `Sandbox::command` and before an external
  specialist or a stdio MCP server starts, removes the keys NyPi was started with: `AWS_SECRET_ACCESS_KEY`,
  `AWS_SESSION_TOKEN`, `GITHUB_TOKEN`, `GH_TOKEN`, `TELEGRAM_BOT_TOKEN`, `HF_TOKEN`, and every `*_API_KEY`,
  `*_SECRET_KEY`, and `NYPI_*_TOKEN`. Ordinary variables are untouched. What a subprocess should have is named:
  `sandbox.allowEnv`, a specialist's `passEnv` or `env`, an MCP server's or command tool's `env`.
- Not sandboxed: MCP servers and external specialists (programs the user configured; the latter bring their own
  sandbox). Both start with the clean environment, and a specialist cannot be sent into a protected folder.

**Approvals.** After the guard, a policy decides which calls need the user's yes. A match is a question, not a
refusal, so it covers routine but consequential actions: `git push`, history-discarding git commands, `sudo`,
`curl | sh`, publishing packages, acting on GitHub, killing processes, infrastructure changes, `DROP TABLE`,
installing background jobs, sending mail, copying to other machines, `rm -r` outside the working folder, and edits to
shell or SSH configuration. `approval.commands` (regexes) and `approval.tools` (globs such as `mcp__gmail__*`) add
rules; `approval.mode` is `risky` (default), `all`, or `off`.

- A request keeps the Inspector's id everywhere (decision D70): on the board, on the hub's stream, in every interface
  and channel. The Inspector takes the first answer from any of them (`Inspector::answer`) and, however the wait ends,
  closes it everywhere (`Approval::Closed`, `approval_resolved`).
- Calls of one reply are prepared side by side (decision D73), so a call waiting for an answer holds up none of the
  others. Abort cancels the wait. No one to ask, no answer within `approval.timeoutSeconds` (900), or a denial: the
  call becomes an error result telling the model not to retry it unchanged.
- Credentials: `http_request` with a `cred://` handle is asked about whatever the mode, unless
  `approval.trustedCredentials` names that secret's scope for the request's host (§4.4).
- A call carried out elsewhere is judged for where it goes (§3.7).
- Chat's device reads are judged without asking (`Inspector::allowed_unasked`, §4.2.2).

### 3.6 Runs: retries, status, and the never-silent rule

The `Runtime` (`runtime/`) owns the agent, the session, the Inspector, compaction, and the ledger side of a run, and
emits `RuntimeEvent`s (agent events, alerts, compaction, retries). Messages are persisted at each `message_end`, so a
crash loses at most the message in flight.

**Auto-retry** (decisions D42, D43; `nypi_ai::retry`, `retry.rs`). A reply that fails with a transient error
(overloaded, rate limits, 429/500/502/503/504/520/524, network and stream failures, plus the wording of NyPi's HTTP
stack) is retried as Pi's `AgentSession` does: wait `retry.baseDelayMs * 2^(n-1)` capped at `retry.maxAgentDelayMs`,
drop the failed reply from the transcript (it stays in the session file), and continue, up to `retry.maxRetries`.
Quota and billing errors are never retried; a context overflow is left to compaction. An abort cancels the wait; the
first reply that comes through resets the budget. Each retry is a `RuntimeEvent::Retry` (serialized as Pi's
`auto_retry_start`, `auto_retry_end`, ...), shown by every interface and recorded in the audit log. The same rules
retry chat's replies, compaction summaries (around the model call alone, as Pi's `completeSummarization`), and
one-shot calls (`oneshot.rs`: reflection, portrait, the observer), which show no notices.

**Run status** (principle B6, `status.rs`). Every `RunOutcome` carries a `RunStatus`: Done, Blocked, Failed, or
Continuing, with its text. `status::parse` reads it from the last lines of the reply (in English whatever the reply's
language; a table row is not a status line). A reply that ends normally without one gets one fixed nudge
(`STATUS_NUDGE`, when `enforceStatusLine` is on); when there is still none, or the run ended in an error, abort, or
the token limit, the harness derives the status from the stop reason and marks it `synthesized`. A synthesized Done
raises a warning.

**Continuing needs a way back** (decision D59). A run that ends `Continuing` is over unless something brings the agent
back, so the status stands only when the run set a schedule that is still active, or has a specialist working for it
(§4.5); its task then goes to `Waiting` in the ledger. Otherwise the runtime tells the agent its turn is over
(`status::continue_nudge`): finish now, set a schedule, or say Done, Blocked, or Failed. After `maxContinuations` (3)
such promises the run becomes a synthesized `Blocked` with an alert.

**Stuck runs** (decision D73). A run with no progress at all (no reply text, no tool call starting, ending, or writing
output; a tool repeating the same output does not count; waiting for an approval is not stuck) for `stuckMinutes` (30)
is stopped and ends `Failed`, saying why. Slow progress is the pump's stall notices (§4.8).

**Citations** (decision D27, `citations.rs`). A hook gives every executed tool result a reference, `[t7]`, numbered
across the session and put at the start of the result the model reads. The prompt asks the model to cite the results
its claims rest on, files as `path:line`, and pages by URL. After the run the harness resolves each reference to the
tool call behind it, a specialist's reference quoted in a delegation report (renamed `[analyst:t2]`), or nothing,
which is flagged as unverified; a run that used tools but cites none is flagged too. `RunOutcome.citations` goes to
the audit log, every interface's `Sources:` footer, and RPC's `agent_settled`.

### 3.7 Place: where a call is carried out

Reading a file, running a command, and editing are things a device does, so the agent does not hold those tools
itself: it holds what its place's node declares (`place.rs`, docs/devices.md). A runtime's device tools are
registered into this computer's node (`device::this_computer`, the same node `nypi node` runs), in this process, and
each call goes to the node, which carries it out under its own gate, sandbox, and journal. Lucia's own tools (memory,
schedules, goals, delegation, the self-prompt, the vault, MCP tools) stay with the runtime.

- **Routed places** (decision D79). A runtime given the hub's devices (`RuntimeOptions::devices`, a `DeviceLookup`
  the hub's register implements) works at the focus of each run (`Runtime::set_focus`): a folder on this computer,
  or a folder on a paired device (`Focus { device, folder }`). Its device tools also take an optional `device`, so one
  call can go to a second device (at the user's home there), and `on_device { device, tool, arguments, folder }` calls
  a tool only some device declares. The prompt's `<devices>` section lists each device with its kind, system, home,
  whether it is connected, and the tools only it has. Each tool works with a folder given per call.
- **Judged for where it goes.** The Inspector asks the place where a call would be carried out (`Place::judged`) and
  judges it with a guard and approval policy seated there, made once per target: the target folder for relative
  paths, and for a device its home and sensitive files, without following paths on this computer's disk. `on_device`
  is judged as the call it carries. The node then applies its own rules too, and an approval question names the
  device.
- **A device's folder speaks for itself.** For a run focused on a device, the hub asks its node
  (`ToNode::ReadPlaceContext`) and gets `PlaceContext { context_files, trusted_settings, skills, specialists }`:
  the context files on the way down to the folder always, and the rest only where the folder is trusted on that
  device (decision D61, `nypi trust` run there). The run's prompt has that folder's context files beside the user's
  global file from the hub. From a trusted device folder (decisions D83, D91; `apply_place_context`):
  - its skills join the skill index (read at the device);
  - its native specialists join the roster (their experience kept under `~/.nypi/places/<device>/specialists/`) and
    work at the run's focus, on that device; an external specialist is never run for a folder on another device;
  - its `trusted_settings` apply through an allowlist: the thinking level (clamped to the model), compaction, retries,
    and tools added to the approval list. Never read from a device: the approval mode, env files, MCP servers, the
    sandbox, channels, the hub, the board, and command tools. The hub's own values come back when the run works
    elsewhere, and what the last run added is taken out before the next.
- **A focus on another folder of this computer** follows only that folder's context files; the conversation keeps
  the settings, skills, and specialists of the hub's own folder (`--standalone` in that folder loads all of it).
  The user's own `!command` runs in the focus folder, and is refused with the reason when the focus is on a device.

### 3.8 Interfaces

Every way a person talks to Lucia is an **interface** (decisions D66, D67; `nypi-cli/src/client.rs`,
`nypi-hub/src/link.rs`): it holds a `Link`, sends what is typed as commands of the RPC protocol, and draws the
conversation's records. None holds a runtime or drives a run. The link goes to the hub's socket when the hub
runs (`Link::hub`); to a paired hub over the network when this device is paired as an interface
(`Link::paired`, §4.9); and otherwise to a hub this process runs for itself (`Link::local`: `daemon::Local`, one
thread reached over a pipe by the same `converse` code a socket connection gets). What every interface needs (asking
for approvals and passing them on, stall notices, long-run reports, resuming what was interrupted) is done once, by
the conversation's pump (§4.8).

- **Terminal**: plain `nypi`. Typed lines are messages, or steer a run under way; `/followup` queues one for after;
  Ctrl-C or `/abort` stops the run; a bare `y` or `n` answers a waiting question. Leaving the hub's conversation
  leaves her running; leaving a hub of one's own ends it after the last reflections. `nypi attach` is the same
  conversation on the hub from anywhere.
- **Full-screen UI**: `nypi --tui` (or `"tui": true`) draws the conversation, a task panel (the running task with its
  last tool calls, recent activity, the approval being asked), and an editor (ratatui, `tui/`). `app.rs` turns records
  into state without terminal I/O and `view.rs` draws it; both are tested with ratatui's test backend. Wrapping uses
  display width, so CJK text lays out correctly. It reports drafts with `typing` and shows recall as `memory_hint`.
- **Print**: `nypi -p` and `nypi send` give one message and print the result when the conversation settles: chat's
  answer when it answers alone, otherwise what the user is told after the work is reviewed. `--no-wait` returns once
  the message is taken.
- **JSON**: `nypi --json` runs one prompt in a runtime of this process and writes every `RuntimeEvent` as a line
  (`run_once.rs`, which the commands run by name also use: `nypi tasks resume`, `nypi dmn run`, `nypi specialists run`,
  `nypi goals review` without a hub).
- **RPC**: `nypi rpc` (or `--mode rpc`) reads JSONL commands on stdin and writes responses and events on stdout
  (`nypi-hub/src/rpc.rs`), following Pi's RPC mode command for command (prompting with `streamingBehavior`, steering,
  queues, models and thinking levels, compaction, retries, session stats and switching, the session tree, the user's
  `bash`, `export_html`; decisions D16, D48 to D53). Events are NyPi's `AgentEvent`s as JSON plus `alert`, compaction
  and retry events, `approval_request` (answered with `approval_response`), and `agent_settled` with the status,
  citations, and pulse id. While a run is active only queue, abort, state, and approval commands are taken. Toggles
  are saved to the global settings file. Not ported: extension UI requests, since NyPi loads no extensions (D4).
- **The board's page** watches and acts in a browser (§4.10); **Telegram** is a channel, not an interface (§4.8).

**What still runs a runtime of its own** (`needs_own_runtime` in `main.rs`): `--standalone`, `--json` and `--mode`,
a model, thinking level, tool set, or session given on the command line, and `--chat-model` or `--no-chat`. Everything
else goes through the hub. `--on <device> --at <folder>` works through the hub only.

**One command language** (decision D72, `nypi-hub/src/protocol.rs`). A command record becomes a typed `HubCommand`
once, where it arrives, and both conversation loops (`rpc.rs`, the worker alone, and `chat_loop.rs`, with chat in
front) match on it, so a command cannot go missing from one. The runtime's own commands (models, thinking levels,
compaction, the session tree, statistics, export, queue modes) are one `RuntimeCommand::run`. What an interface says
beyond Pi's records is in `nypi-hub-proto` (decision D79): the `attach` line with its `AmbientFocus`, a `focus` line
that changes it, the greeting `daemon_hello` (agent, model, focus, session, busy, clients, open tasks as
`TaskSummary`), and the records a module adds (`"module": "chat"` or `"worker"`, `chat_settled`, `task_started`,
`user_said`, `task_reviewed`, `activation_queued`).

## 4. Hub and cognitive architecture

The hub (`nypi-hub`) is the one place Lucia runs: it holds her conversation, carries out the work nobody asked for in
it, runs her channels, and tells the user what nobody was there to see. With a chat model set, the conversation is
three planes that do not share a role (decision D84): chat speaks, the Principal decides, the task pool's workers
act.

```
 interfaces (terminal, TUI, board, paired devices)       channels (Telegram, inbound webhook)
                    |  commands + ambient focus                      |  Port
                    v                                                v
 +-------------------------------- hub: the one conversation (PrimaryStream) ----------------------------+
 |  chat (fast model) --signal_principal--> Principal (strongest model) --start/steer/review--> task pool  |
 |    ^  reads stores, tasks, devices         |  stores: vault, goals, memory,        workers (runtimes)  |
 |    +----------- stimulate_chat ------------+  assets, schedules                     observer (Thinking) |
 |                                                                                                        |
 |  pulse bus  .  Inspector  .  ledger  .  scheduler  .  watchdog  .  jobs  .  pump  .  register of nodes  |
 +--------------------------------------------------------------------------------------------------------+
                    |  Call / FromNode                                   |  board link
                    v                                                    v
          nodes (this computer, paired devices)                  board service (nypi board)
```

### 4.1 Pulses and schedules

All data and control flow between Lucia's modules goes through **pulses**: typed, causation-linked messages on one
bus (`pulse.rs`).

```rust
enum PulseSource { UserInput{channel}, UserTyping{channel}, Timer{schedule}, Condition{condition},
                   TaskEvent{task}, Dmn, Module{module} }
struct Pulse { id, thread: Option<String>, source, priority, at, causation: Option<PulseId>, payload }
```

- **One bus per hub** (decision D67, part 3). Each thread works through a handle scoped to it (`PulseBus::scoped`):
  what it publishes carries the thread's name, and its subscribers hear that thread's pulses only. The hub uses the
  root handle: what it publishes (a timer firing) belongs to no thread, and it hears every pulse. One sequence numbers
  them all, so a run names the hub's pulse that caused it across threads. A runtime ends its scope when dropped
  (`ThreadEnd`), and modules listening there stop.
- **IDs and priority.** IDs are `<random bus prefix>-<seq>`, unique across processes sharing the audit log. Priority
  follows from the source and cannot be chosen: user input and typing are `user`, the DMN is `background`, the rest
  `normal`.
- **Taps** see each pulse synchronously, in publish order, before any subscriber; the Inspector is the tap that writes
  it to the audit log (decision D17). **Subscribers** get their own inbox filtered by `PulseKind`, delivered highest
  priority first, FIFO within one. A full inbox (1,024) drops the oldest pulse of the lowest priority and counts it;
  user pulses are never dropped.
- **Kinds**: `userMessage` (delivery `prompt`, `steer`, or `followUp`), `delegated`, `reply`, `toolStarted`,
  `toolFinished`, `runSettled` (with status and answer), `alert`, `memoryValve`, `typing`, `memoryHint`,
  `triggered`, `thought`, `turnCompleted`, `reported`. Every run pulse's `causation` is the pulse that started the run.
- **Typing**: the TUI publishes `typing` when the user pauses 250 ms with a changed draft, and RPC clients send
  `{"type": "typing", "text": ...}`. The audit log records only the draft's length.
- **Tracing**: `nypi trace` lists recent pulses; `nypi trace <id>` prints the causal chain from the root, the pulses it
  caused, and the audit records written while it was the cause (`trace.rs`, principle A4).

**Timers and conditions** (decision D36; `schedule.rs`, `nypi-hub/src/scheduler.rs`). Schedules live in
`~/.nypi/schedules/schedules.jsonl` (append-only, harness-written). A trigger is a time (`at`, `every`, `daily` with
optional weekdays, local time) or a condition (`fileChanged`: size and modification time; `command`: exit status and
a hash of the output, checked every few minutes, firing on success or on change). A condition records its first
observation without firing and fires once unless `repeat`; times missed while nothing ran fire once, late. The action
is `notify` (a notice) or `prompt` (work for the conversation, at the folder and device the schedule was set at; a
command condition set at a device is run on that device and left alone while it is away).

- The scheduler is a module of the hub: the hub or a terminal's own hub checks every 30 s behind a round lock
  (`run/scheduler.lock`), publishes `triggered` (source `timer` or `condition`), and records the firing before acting
  on it, so a crash can drop a firing but never repeat one. `nypi schedule tick` does one round for launchd or cron.
- The agent sets schedules with the `schedule` tool; a command condition passes the guard and approval policy as the
  `bash` call it will become. The tool steers toward the cheapest form (decision D41): text to wait for belongs in a
  command condition that tests for it, and a prompt on a trigger that can fire often gets a note that each firing is
  a model run. A schedule may carry the `goal` it serves (§4.6).
- A fired prompt is a sourced prompt, never the user's words: with chat in front it is the Principal's `P1`
  activation (§4.2.3), otherwise a task of its own under origin `timer`.

**Round locks** (decision D83, `round_lock.rs`). Two hubs can run at once (a terminal with a session of its own beside
the hub), so the scheduler's and the watchdog's rounds each take a `RoundLock`: a POSIX record lock (`fcntl`), which
belongs to the process and is never inherited by a forked child, with the process noting its own held locks. A round
that finds the lock taken skips. Never `flock`, which a child forked during a round keeps until it execs.

### 4.2 The hub and its three planes

#### 4.2.1 One conversation

The hub holds a single conversation, `PrimaryStream` (`daemon/primary.rs`, decision D79): one runtime opened when the
hub starts and held as long as it runs, its session in `~/.nypi/sessions/primary/`. Every interface attaches to it,
whichever folder or device it was opened at, and every record reaches every interface.

- **Ambient focus.** What an interface brings is where its user is, `AmbientFocus { device, cwd }`
  (`nypi-hub-proto`): the `attach` line names it (without one, the hub's own folder), the greeting repeats it, a
  `focus` line changes it, and the hub puts it on every command that asks for work (`prompt`, `steer`, `follow_up`,
  `bash`). A folder that does not exist, a device that is not connected, and a connection that says it is a node are
  refused. The run works at the focus of the message that started it (§3.7), and the task's ledger record says where,
  so a resume or a fired schedule goes back there.
- **Threads.** A `Thread` (`daemon/threads.rs`) is one runtime put to work with its record stream and command stream,
  in one of three roles: the conversation (`Role::Conversation`), the one thread of a terminal's own hub
  (`Role::Local`), or a job nobody is talking to (`Role::Job`, §4.7). A conversation or local thread runs the chat
  loop when a known chat model is set, and the worker-only loop otherwise; a job always runs the worker alone.
- **What else is going on.** `<active_tasks>` (`active_tasks.rs`) lists, from the ledger, what is worked on besides
  this run (a task beside it, a job a schedule started, a run in another program) with where each works, what it waits
  on, and its workspace, and what ended in the last six hours: eight lines at most.
- **Sourced prompts.** A prompt that is not the user's own words is a `prompt` command with a `source` (a pulse source)
  and an `origin`; one claiming to be the user is refused. It waits behind what runs and never steers the user's run.
- **Resuming.** The conversation resumes, when it opens, the tasks its session's last process left (through the
  worker when chat is in front).

#### 4.2.2 Chat: the only voice

Chat (`chat.rs`, prompt `prompts/chat.md`) is the only module that speaks to the user, live and, when nobody is
attached, through the outbox and channels. It runs on `chat.model` (thinking `chat.thinking`, default off), reads
everything, and changes nothing.

- **Its tools.** The read-only wrappers of the stores (`introspect.rs`, decision D82): `vault_search`, `vault_list`,
  `vault_read` (cut to 2,000 characters), `memory` (`recall` only), `asset_manage` (`list`, `get`,
  `expiring_within_days`), `schedule` (`list`), `goals` (`list`). Each wrapper's `action` enum lists only what reads,
  and the wrapper refuses anything else again. `tasks_inspect` shows the pool, the ledger's ten newest tasks, the
  Principal's active, suspended, and queued activations, and its five newest decisions. The device reads `read`,
  `grep`, `find`, and `ls` go through the conversation's `Place` (`Runtime::device_read_tools` makes them whether or
  not the worker has them). And one tool upward: `signal_principal { topic, urgency: normal | interrupt }`.
- **Device reads never wait.** A read is judged before it runs and never asked about (`Inspector::allowed_unasked`):
  the guard refuses a key file as for any agent, and a read the policy would put to the user is refused at once
  ("Reading this path requires approval; signal the Principal to inspect it in a task"). It is stopped after 1.5 s
  (its cancellation token tells a node to stop there too, and the model is told to manage the user's expectation and
  signal the Principal) and shows at most 4,000 characters. A relative path starts at the user's focus.
- **The user comes first** (decision D82). A message that arrives while chat answers is steered into that turn;
  messages that arrive together are one turn; a turn's end takes a message that came too late for it as the next turn.
  A message that arrives while chat phrases a stimulus (a turn the user does not see yet) stops that turn, is
  answered, and the stopped one runs again after.
- **The hand-over follows the reply.** What chat signals in a turn goes to the Principal once the reply is out, with
  the conversation as it then stands (the last twelve messages verbatim, each cut past 4,000 characters and all past
  12,000, oldest first; harness notes and `SKIP` replies left out), a digest of what chat looked up (each result cut to
  600 characters), and the focus. The tool returns the activation's id (`a-...`) at once. If the chat model fails, the
  user's message goes up as a signal of its own.
- **Chat phrases, in its own voice.** A stimulus from the Principal is a turn the user sees only when done:
  `[The Principal asks you to tell the user something (...)]` with the headline, every fact to keep, the question to
  ask, and the note to name. Chat tells it briefly, keeping every fact and `[tN]`, or replies `SKIP` when the
  conversation already said it all. A stimulus turn is marked on its `chat_settled` record and is told as a notice to
  a user who is not there.
- **History.** Chat's side of the conversation is kept in `~/.nypi/chat/<worker session>.json`, written whole after
  every turn (bounded by `chat.historyMessages`, 80) and read when the conversation opens again (decision D75).
- **Prompt** (decision D89). Chat is one person to the user and names no internal part; it answers what one to three
  read-only lookups settle and signals everything else at once, with the possibilities in `topic`, rather than ask
  what a background check can answer; `interrupt` is only for stop, cancel, or a change of direction. Its prompt
  carries `## Where` and `<devices>`; the memory valve and portrait apply to its turns as to the worker's.
- **Log.** `chat/turns.jsonl`: one record per turn (`origin` `userSay` or `principalStimulus`, focus, the user's text
  or the stimulus, memories recalled, thinking, every tool call with arguments, time, excerpt, and whether it timed
  out, the signals with their activation ids, the reply, whether it was skipped, tokens, cost), scrubbed of secrets.

#### 4.2.3 The Principal: the one who decides

The Principal (`principal/`, prompt `prompts/principal.md`) is one per conversation, on `principal.model` (the
worker's by default) at `principal.thinking` (`high`), with the Inspector's hooks. It never speaks to the user.

- **Its tools.** The stores' (`vault_*`, `goals`, `memory`, `asset_manage`, `schedule`); the pool's (`start_task`,
  `steer_task { task_id?, message, when: now | after }`, `cancel_task`, `task_status`,
  `continue_task { task_id, critique }`); `inspect_task_evidence`; `conclude_activation`; and
  `stimulate_chat { kind: result | question | update | alert, headline, must_convey, ask_user?, vault_ref? }`, its
  one way to the user. No shell, no device tool, no `http_request`, no MCP server.
- **Activations.** Every wake is an activation with an id, run one at a time by priority:

| Priority | What wakes it | Trigger |
|---|---|---|
| `P0` | The user, through chat's `signal_principal` | `userSignal` |
| `P1` | A settled task (or several) to review; webhook material; a fired schedule's prompt | `taskSettled`, `taskSettledBatch`, `inbound`, `schedulePrompt` |
| `P2` | Idle time: an idle pass or a timed goal review (§4.2.6) | `idle` |

- **Suspension and resumption.** A user's signal that arrives while a `P1` or `P2` runs (or any, with `urgency:
  interrupt`) stops only the model request in flight: the completed steps are kept, the aborted reply dropped, and the
  activation goes on a stack, suspended, with the decision journal's last number. The user's runs to its end. The
  suspended one comes back when nothing of higher priority waits, with `<resumption_delta>` listing what was decided
  meanwhile (`[Resumed after handling higher-priority activation(s): ...]`). More from the user while their
  activation runs joins it as steering. At most `principal.maxActivationsQueued` (32) wait; when full, idle time gives
  way first.
- **Four rings of context** (decisions D87, D88; `briefing.rs`), outermost first:
  1. *The system prompt*: `principal.md` (with `{{user_ref}}` filled), `<portrait>`, and `<self>`, the same for
     every activation and turn so the provider's prompt cache holds.
  2. *`<situational_radar>`*, at the head of the opening message: the date; the open goals on their two clocks (`due
     in Nd` or overdue, `idle Nd`, the tasks working for each, its `plan.md`, the plan's next item, what is blocked);
     the execution board (the pool's tasks, the Principal's own suspended and waiting activations, schedules firing in
     the next 48 hours, `## Where`, `<devices>`); and `<recent_decisions>` (the fifteen newest).
  3. *`<briefing_pack>`*, looked up before the first model call by what woke it, by words only and without waiting
     (the vault's index only when its lock is free, `VaultStore::search_now`; no embedding; memories ranked without
     recording a recall), with the time it took. For the user's request: the `plan.md` of up to two goals it matches
     (whole, 6 KB), the matching chunk (700 characters) of up to three `30-entities/` and `20-playbooks/` notes with
     the notes one `[[link]]` away named, up to four memories (none sensitive), and up to four assets with their
     `cred://` handles. For a review: the plans of the goals its tasks served, each task's `findings.md`, and the last
     three chat turns. For material or a schedule's prompt: the material fenced and the plans and notes it matches.
     For idle time: the two-clock digest, the inbox's notes, and what today's idle budget has left. Logged as the
     story's `briefed` event.
  4. *The activation's own transcript*, with `<resumption_delta>` after a suspension.
- **Dispositions** (decision D87). An activation records what it did about what woke it: a task started, steered,
  sent back, or cancelled; a store changed (any store call but the reading ones, `introspect::reads_only`); chat told;
  or `conclude_activation { disposition: already_handled | waiting_on_running_task | noted_only, reason }`. A user's
  signal, a review, material, or a schedule's prompt that ends with none of these is told once (`[NyPi harness] You
  ended this activation without ...`, logged as `nudged`); idle time is not (`NOTHING` is its answer).
  `principal.enforceDisposition` is `auto` (real models only), `always`, or `off`.
- **Reviews.** A settled task the Principal started is its `P1` activation with the review note (§4.2.4). It accepts by
  having chat tell the user (`task_reviewed` with `said: true`), lets the report stand (`said: false`, the report
  shown as written), or sends it back with `continue_task`; after `tasks.maxReviewRounds` (2) the report goes out as
  it is, with candour about what stays open. A report `Blocked` at a boundary only the user may cross (a payment, a
  credential, an irreversible send) is accepted as such. `chat.review` is `auto` (reviews unless the chat model is a
  scripted test model), `always`, or `off` (reports shown verbatim).
- **Reviews that go together.** `start_task`'s `review_policy` is `immediate` (default) or `batch_unless_failed`: a
  Done settlement of the latter waits while a task for the same goal, or started by the same activation, still works,
  until those settle or `tasks.batchHoldSeconds` (60) pass; a Blocked or Failed one is never held. Whenever a review
  is taken from the queue, the settlements waiting there that go with it come along as one `taskSettledBatch`
  activation (theirs end as `joined`), with one review and one word to the user.
- **Working method** (decision D89). Beside its nine rules, `principal.md` has a `<working_method>`: check before
  asking (two or three readings checked with quick read-only tasks in parallel, then one word with a recommendation);
  inspect an unfamiliar system before planning changes; run independent work in parallel; pass known notes,
  playbooks, entities, and `cred://` handles into briefs; move freely on reversible work and stop before what is the
  user's alone; work around ordinary snags; write lessons down.
- **Logs** (all through the secret store's `LiveScrubber`, §4.4). `principal/activations/<id>.jsonl`: the story
  (`created`, `started`, `briefed`, `assistantTurn` with thinking and tool calls, `toolResult`, `joined`,
  `suspended` with its step count and journal number, `resumed` with the delta, `nudged`, `stimulatedChat`,
  `finished` with summary, tokens, cost). `stores/activations.jsonl`: an index line per change of state.
  `stores/decisions.jsonl` (`DecisionJournal`): one line per finished activation (`seq`, activation, priority,
  trigger, topic, summary, disposition, tasks and goals touched, whether chat was asked to speak, tokens, cost).
  `principal/status.json`: its stack, queue, and step count with its process id, rewritten whole through a `.part`
  file and a rename whenever they change, for the board.

#### 4.2.4 The task pool and its workers

The task pool (`task_pool.rs`, decision D78) runs the Principal's tasks, up to `tasks.maxConcurrent` (4) at once. The
first runs on the conversation's own worker (`worker.rs`: the whole `Runtime` in a Tokio task of its own, with every
tool, the guard, approvals, the session, and compaction), so a follow-up still finds what the last task read. A task
started while that one is busy gets a runtime beside it (`Runtime::beside`: the same place, model, and settings, an
ephemeral session, its own ledger task), whose events reach interfaces as `WorkerEvent::Beside`. One over the limit is
refused with the list of what runs. The pool remembers where its last task on the own worker worked: a task for
another place begins a fresh session there, one for the same place keeps it.

- **The brief.** `TaskBrief { title, topic, instruction, done_when, bounds, place, conversation, goal,
  spawned_by_activation, review_policy }`. `start_task` takes a one-sentence `topic`; the harness attaches the
  conversation the Principal was handed (`<conversation_context>`) and where the user is. `place` is the user's focus
  unless `start_task` names a `folder` and `device`. `goal` is checked against the goals file.
- **The workspace.** A task whose brief says what counts as done, or that comes with the conversation attached (a
  topic brief), gets `10-workspaces/YYMMDD-<slug>/` in the vault with `brief.md` and an empty `findings.md`; a
  one-step errand gets none. Without `done_when`, the worker appends a `## Done when` section to `brief.md` on its
  first step, and the review reads the brief as it is then (`task_pool::done_when_in`). Every turn's prompt carries
  `<task_workspace>` (findings cut at 6 KB, with a note to move detail into other notes of the workspace), and a task
  for a goal carries the goal's `plan.md` as `<project_plan>` above it (§4.6), so what the run has ruled out survives
  compaction. `core.md`'s conduct rules tell the worker to write falsified approaches, dead ends, and verified
  findings there as it goes.
- **The dossier** (decision D87, `evidence.rs`). A task with a workspace writes `report.md` there before it ends Done
  or Blocked: `## Contract Check` (each item of `Done when` with its `[tN]`), `## Key Findings & Deliverables`, and
  `## Falsified Paths & Residual Risks`. A run that ends Done or Blocked without one, or with one that cites no tool
  result although the run ran tools, is sent back once to write it (`tasks.requireReport`: `auto` for real models
  only, `always`, `off`).
- **The review note** the Principal gets holds the brief, the status, `report.md` (the final reply without one), and
  the **Harness Evidence Manifest**, drawn from the task's session file (compacted messages too), which the worker
  cannot write: each `[tN]` the report and reply cite with its tool, arguments, ok or error, time, and 400 characters
  of what came back; references that match nothing; the latest run's file changes (`write`, `edit`) and commands
  (`bash`); and whether a `read`, `grep`, `find`, `ls`, or `bash` ran after the last change, with a warning when a Done
  has none. Brief, findings, report, and manifest are scrubbed before the note is built. The Principal reads any step
  in full with `inspect_task_evidence { task_id, refs }`, from the session file the ledger names, scrubbed, at most
  8,000 characters a call.
- **Records.** A settlement the Principal will review carries `review: "pending"` and its key; then `task_reviewed`
  (`accepted`, `said`, `round`) and, when accepted without words, `task_report`.
- `Runtime::resume_task` puts a resumed task back on its workspace and campaign from its ledger record, so a task taken
  up after a crash sees `<project_plan>` and `<task_workspace>` again.

No rule of the kind "after three errors, stop" or "for a `.csv`, do this" exists in the runtime (decision D78): what
keeps a task on course is its brief, its workspace notes, the observer's rare steer, and the Principal's review.

#### 4.2.5 The observer

The Thinking module (`thinking.rs`, decisions D78, D84, D91) watches a working run's course and steers it when it
goes wrong. `thinking.enabled` (off by default) is its one switch; it runs on `thinking.model` (the worker's by
default) at `thinking.level` (`high`). Each turn's end is a `turnCompleted` pulse (`task`, `turn_index`,
`recent_calls_digest`, `compacted`). Every `thinking.workerCadenceTurns` (5) turns, and at once after a compaction,
one tool-free call reads the task's goal, brief, findings, and the digests, and answers `{assessment, steer}`. `steer`
is empty unless the run repeats what it already falsified, trusts a document over the source, overlooks a tool, a
specialist, or a note, or drifts from the brief; then it is published as a `thought` and queued for the run as
`[Metacognitive note]: ...`. A look still under way when the run settles is dropped. The observer does not read the
user's messages.

#### 4.2.6 Idle passes

The DMN (`dmn.rs`, decision D37) is idle-time reflection, opt-in (`dmn.enabled`). The scheduler's round orders a pass
when one is due: nothing in the ledger is running, and either nothing has happened for `idleMinutes` (30) and the last
pass is `minGapHours` (2) old, or the last pass is `everyHours` (12) old; never more than `maxPassesPerDay` (6) passes
or `dailyTokens` (300,000) tokens a day. Idle is read from the ledger, so it holds across every interface.

- **The vault's upkeep comes first** (decision D81, `dmn::consolidate`): a workspace under `10-workspaces/` moves to
  `90-archive/YYYY-MM/` once every ledger task that names it has been settled for `vault.archiveAfterDays` (7); one
  whose task is open or waits on the user, and one no task names, stay.
- **With chat in front** of the hub's conversation (decision D85), the pass is the Principal's `P2` activation,
  given the goals' two-clock digest (§4.6), the state digest (time, goals, recent tasks, schedules, commitments, the
  last note, the inbox to file, memories to distil into `30-entities/` pages, recurring lessons to draft as
  `20-playbooks/` notes with `status: draft`), and the budget. The chat loop refuses a second idle activation while
  one waits or runs, and refuses one when the day's budget is spent; a finished one is recorded in
  `~/.nypi/dmn/passes.jsonl` from its decision. A timed goal review (`goals.reviewEveryHours`, origin `goals`) is a
  `P2` activation the same way. Nothing is posted from the pass: what the user should hear, the Principal has chat
  say.
- **Otherwise** a pass is a quiet job of the hub (§4.7) with source `dmn` (background priority): it may act as it
  would for the user within what they would expect unasked (nothing sent to others, nothing irreversible), and answers
  `NOTHING` or a two-to-four-line note sent through the notification channels. `nypi dmn` shows whether a pass is due,
  today's budget, and recent passes; `nypi dmn run` runs one now.

#### 4.2.7 Without a chat model

Without `chat.model` (or with `--no-chat`), one agent does everything: the conversation's thread runs the worker-only
protocol loop of `rpc.rs` on its runtime, messages go straight to it, schedules and goal reviews run as tasks, and
delegation, approvals, notices, and the ledger work as described here. A job is always this way.

<a id="self-modifying-prompt"></a>

### 4.3 Prompt layers and the self-modifying prompt

The worker's system prompt (`prompt.rs`) is built from named sections, each tagged `<name>...</name>`, in three
layers:

1. **Core (read-only)**: `prompts/core.md`, compiled into the binary (a user-maintained `~/.nypi/prompt/core.md`
   replaces it): identity, the conduct principles (principles.md Part B), the partnership rules (decision D76), safety
   and tool rules, then the `tools` and `rules` sections built from the tools offered. No tool can write the prompt
   folder (§3.5).
2. **Self (read-write)**: `~/.nypi/agents/<name>/self.md`, which Lucia edits only through `self_prompt` (view, append,
   replace, rewrite, history; 16 KB cap). The tool saves the previous text and the reason under
   `agents/<name>/history/` before every change; the guard blocks direct `write` and `edit`. Reflection appends
   lessons here (§4.11).
3. **Situational**: `goals`, `portrait`, `specialists`, `addendum` (`appendSystemPrompt`), `project_context` (the
   context files), `project_plan`, `task_workspace`, `active_tasks`, `devices`, `skills`, and `environment` (folder,
   device, system, date).

Recalled memories are situational too, but the valve attaches them to the user message they bear on rather than to
the system prompt (§4.4), so the cached prefix stays stable. The transcript carries the sections (§3.1), so a changed
section is appended, not rewritten. Chat's and the Principal's prompts are built the same way from `chat.md` and
`principal.md`; the Principal's is cache-stable by design (§4.2.3). Priority between sections is expressed by order
and explicit labels; real attention weighting is not controllable through provider APIs.

### 4.4 What NyPi keeps (`nypi-vault`)

Four stores, apart on purpose (decision D77): single facts in episodic memory, structured notes in the vault, things
looked after in the asset registry, and values never shown in the secret store. Beside them, the user portrait.

**Append-only logs** (`jsonl.rs`). The ledger, schedules, memories, portrait, and outbox are JSONL event logs, one
event per line, synced before returning, with a fresh line after a torn one. A store's meaning is the fold of its
events; `Folded` keeps each fold in memory and at each read takes in only the complete lines appended since (decision
D67, part 6). It follows the file, not the process's own writes, so another process may append too; it starts over
when the file is not the one it was reading. Folds are shared per file within a process. Measured: a 16,400-event
ledger reads in 23 ms the first time and 9 microseconds after.

**Episodic memory** (`episodic.rs`, used as `nypi_core::memory`; decisions D30, D31, D33).
`~/.nypi/memory/memories.jsonl` holds `added`, `updated`, `recalled`, and `forgotten` events; a memory is `{id, text,
tags, kind (fact, preference, decision, event, person, commitment), importance, provenance, created, last_recalled,
recall_count, emotion? {valence, arousal}, embedding?}`. The agent uses the `memory` tool (`recall`, `remember`,
`update`, `forget`); a memory can be marked sensitive (health, money, loss, conflict), and one remembered as painful
and intense (valence <= -0.3, arousal >= 0.4) counts as sensitive. The prompt says to recall before answering about
the user, people, past decisions, or earlier work, and never to store secrets. `nypi memory` lists, searches
(`--explain` shows the factors), adds, forgets, and re-embeds.

- **Embeddings** (`nypi-ai/src/embeddings/`). `memory.embeddings` is an ordered list of sources: OpenAI-compatible
  endpoints (`baseUrl`, `model`, `apiKeyEnv`) and local models (`local`); the first that works is used, and one that
  fails is skipped for 10 minutes. The default is the local model alone: Qwen3-Embedding-0.6B in Q4_K_M GGUF (397 MB,
  `smarttasks/Qwen3-Embedding-0.6B-GGUF`, tokenizer from `Qwen/Qwen3-Embedding-0.6B`), fetched by `nypi memory model
  download` into `~/.nypi/models/embeddings/`. Both files are checksummed: the GGUF against the repo's published
  `SHA256SUMS`, and `tokenizer.json` against a SHA-256 pinned in the code, since its repo publishes none (decision
  D91). It runs in process with candle (feature `local-embeddings`, on by default): a hand-written Qwen3 forward pass
  over the quantized weights, last-token pooling, queries prefixed with a task instruction, vectors centered on the
  mean of 24 generic memory sentences before normalizing. A message embeds in 40 to 90 ms on the CPU (Metal on macOS 15
  and later only). Vectors are stored with their model, so different models are never compared.
- **Retrieval score** (decision D31) is gated: `score = R x (w_r + w_i*I + w_e*E + w_a*A)`, weights normalized
  (defaults 0.55, 0.20, 0.10, 0.15, `memory.weights`). R, relevance, is embedding similarity rescaled
  (`(c - 0.17)/(0.65 - 0.17)`, clamped) with BM25 keyword evidence adding to it (`R = s + (1 - s)*0.5*k`; keywords
  alone give `s/(s+2)`); below 0.15 a memory is dropped, so a vivid or important memory never surfaces for an unrelated
  question. Keywords are characters, bigrams, and English words stemmed conservatively (Porter step 1 only), without
  lone Chinese particles. I is importance; E the recorded arousal; A the ACT-R base level over creation and recalls
  (τ a year for preferences, people, and decisions, 90 days for facts and commitments, 14 days for events), squashed
  as `A/(A+1)`. Recall shows the factors; a test fixture pins the ranking's contract.
- **The memory valve** (decision D32, `valve.rs`). Before every prompt (the worker's and chat's) the valve ranks
  memories against the message with no model call (embedding with a 2 s limit, waiting up to 15 s for a local model
  still loading). A message of eight words or more is also ranked by its keyword form. It reads the situation from the
  wording (`task`: a request, a question, code or a path; `light`: laughter, jokes, playful emoji, in English and
  Chinese) and attaches a memory when its score reaches 0.25 for a task or 0.45 for casual talk, up to 3 per message,
  in a `<recalled-memories>` block on the user message. A sensitive memory in a light moment, or one already attached
  in the visible conversation, is held back. Each decision with matches is a `memoryValve` pulse (surfaced, held and
  why, factors). Settings: `memory.valve` (`enabled`, `taskThreshold`, `casualThreshold`, `limit`).
- **Recall while typing** (decision D34). A task subscribed to `typing` pulses runs the same recall on the newest
  draft and publishes `memoryHint` (at most 2) when the memories that would clear the valve change; the TUI shows them
  in the input box's border, RPC clients get `memory_hint`. The draft's query vector is cached (the last 8) for the
  message when it is sent. A preview is not a recall. Measured: the hint appears about 0.55 s after the last key.

**The user portrait** (decision D35, `portrait.rs`). `~/.nypi/portrait/portrait.jsonl` holds traits `{id, category
(identity, value, goal, style, preference, dislike), text, evidence [{quote, pulse, session, at}], shown}`. After each
turn, when the user's new messages hold at least `portrait.minWords` (6; CJK characters count one each), one
background, tool-free call (`portrait.model`, default the agent's) proposes `add`, `revise`, or `remove`, each quoting
the user verbatim; the harness keeps a change only when a quote is found in those messages, which gives the trait its
provenance down to the pulse. A contradiction removes the old trait and adds the new one. Limits: 40 traits, 8 quotes
each. It keeps out one-off requests, specific facts, anything sensitive, and secrets. The portrait is a `<portrait>`
section (most-shown first, at most 2,500 characters) for the worker, chat, and the Principal; delegated specialists
neither read nor learn it, and sourced prompts never teach it. `nypi portrait` lists, `show`s, and `forget`s traits;
`portrait.mode` is `auto` or `off`.

**The vault** (`vault.rs`, `index.rs`, `tools.rs`; `~/.nypi/vault/`). Lucia's own Markdown notes, apart from any the
user keeps, in a fixed layout made when it opens:

| Folder | What it holds |
|---|---|
| `00-inbox/` | What came in, to be sorted (the webhook's notes) |
| `10-workspaces/` | One folder per substantial task: `brief.md`, `findings.md`, `report.md`, other notes |
| `10-projects/` | One folder per goal's campaign, with `plan.md` (§4.6) |
| `20-playbooks/` | How something is done |
| `30-entities/` | Projects, systems, devices, people |
| `90-archive/` | Finished workspaces by month |
| `attachments/` | Files copied in |

- It is inside NyPi's home, closed to every subprocess, so no shell command can damage it. The agent reaches it only
  through six in-process tools: `vault_search`, `vault_read`, `vault_write`, `vault_list`, `vault_attach`,
  `vault_archive`. `Vault::resolve` checks every path: relative to the vault, one component at a time, no `..`, no
  symlink on the way, never the index's own files. `vault_attach` copies in only what `Protected` lets be read;
  `vault_write` refuses a note that looks like it holds a raw secret. Notes are capped at 256 KB, attachments at
  32 MB.
- **The index** splits each note along its `#`, `##`, and `###` headings into chunks of at most 1,200 characters,
  addressable as `path#heading-slug`; frontmatter gives `title`, `kind`, `tags`, `status`, `aliases`, `updated`; and
  `[[target]]` and `[[target|alias]]` links resolve by path, file name, title, or alias into a forward and a back graph.
  A search scores chunks as memory does, keeps each note's best chunk, then follows links one hop from the five
  strongest hits (a linked note with relevance of its own gains `1 + 0.6x` the hit's score; one with none comes up at
  0.35 of it, marked as linked), so a project brings its servers and playbooks. The notes are the truth;
  `.index.jsonl` keeps only vectors (with model and text hash) and file fingerprints, so a chunk is embedded again only
  when its text changed. A write through the tool is indexed before the call returns.

**The asset registry** (`assets.rs`, `~/.nypi/assets.jsonl`, tool `asset_manage`): accounts, servers, domains,
subscriptions, identities, and deliverables Lucia looks after, each with a steward (user or agent), a non-secret
identifier, renewal date, notes, and `secretRefs`. The model reads it, so it refuses any field that looks like a raw
secret and any secret reference that is not a handle.

**The secret store** (`secrets.rs`, `~/.nypi/secrets.enc`). Values named by opaque handles, `cred://<scope>/<name>`,
each sealed with ChaCha20-Poly1305 (`ring`) under a random 96-bit nonce, the handle bound into the seal, the file
written whole with mode 0600. The master key is kept apart: in the login keychain on a Mac with the default home
(`sh.nypi.secrets`, written through `security -i` so it is never an argument), otherwise in the user-only
`secrets.key`. Neither file is readable by any tool. The user manages it: `nypi secrets set <scope>/<name>` (typed
unseen or piped), `list` (handles and times), `rm`.

- **Brokered use** (`tools/http_request.rs`). `http_request` takes `auth` as `{type: bearer, cred}`, `{type: header,
  name, cred}`, or `{type: basic, username, passwordCred}`. The Inspector asks before a credential is used unless
  `approval.trustedCredentials` trusts that scope for the host. The harness opens the secret, sets the header, and
  passes everything that comes back through `SecretScrubber`, which replaces every stored value, raw or base64, and
  the `user:password` form, with `[REDACTED:cred://scope/name]` before it becomes a tool result, session entry, or
  audit record. Redirects are not followed; a secret typed into a header by hand is refused.
- **`LiveScrubber`** (decision D87). The planes' logs and the board scrub through a scrubber that notes `secrets.enc`'s
  modification time and size and reloads the store only when the file appears or changes (asking for no key while
  there is none), so a secret added to a running hub is scrubbed from the next line written.
- Other credentials: the Telegram bot token can be a handle (§4.8); the webhook's secret is one.

<a id="task-ledger-and-watchdog"></a>

### 4.5 Tasks: ledger, watchdog, specialists, delegation

Lucia is a chief of staff, not only a doer (principle B2), and no task may stop silently (principle B6).

**The task ledger** (decision D22, `ledger.rs`). `~/.nypi/tasks/ledger.jsonl` is an append-only event log; each line
is synced before the step it announces, and a task's state is the fold of its events. Only the harness writes it.

- States: `Planned`, `Running`, `BlockedOnUser`, `Done`, `Failed`, `Cancelled`, `Stalled`, `Waiting`.
- Every run of a runtime is a task: `created` (goal, full prompt, owner, origin, session, working folder and device,
  pid, the pulse that started it) and `running` are written before the model is called; the run's status sets the end
  state. Other events: `campaign` (`goal_id`, `spawned_by_activation`, §4.6), `workspace`, `toolStarted` and
  `toolFinished` (written around each call, so a crash leaves exactly which calls were cut off), heartbeats (at most
  every 30 s while streaming, and every 30 s whatever the run does), `resumed`, `dispatched`, `waiting`, and `rated`.
- **Interrupted tasks.** A task is interrupted when it is not finished and its pid is gone. When its session opens
  again it resumes: `resumed` (attempt n+1), every unanswered call of the last reply gets an error result
  ("Interrupted: ... not run again", or "Not run"), an errored half-reply is dropped, and the run continues from the
  transcript. After `RESTART_LIMIT` (5) attempts the task fails instead. `nypi tasks resume <id>` opens a task's
  session and runs it to the end, or leaves it to the hub when the conversation holds it.
- **What a waiting task waits on.** `Waiting` carries a `WaitPredicate` over `WaitToken`s (a schedule, a delegation,
  another task, an approval): `AllOf` (a join), `AnyOf` (a race: the first that comes through without failing),
  `Schedule`, or `Approval`. The runtime writes the predicate its wake ids stand for; the watchdog judges it
  (`watchdog::check_waiting`), and on a race won closes the tasks still running for the others.
- `nypi tasks` lists open tasks (`--all` for finished), `show <id>` one with its trace, `cancel <id>` closes one.

**The watchdog** (decision D23; rules in `nypi-core/src/watchdog.rs`, acting in `nypi-hub/src/watchdog.rs`). A
module of the hub: the hub or a terminal's own hub checks the ledger every `watchdog.intervalSeconds` (60), and the
supervisor does while no hub runs, one process per round behind `run/watchdog.lock`; a round over two minutes is
abandoned. Per open task:

- alive, running, and silent for `stallSeconds` (300): `Stalled` with one alert; a later heartbeat puts it back;
- `Waiting`: judged by its predicate. On schedules: nothing while one is pending; Done ("continued by schedule ...")
  when one fired, since the work went on as that schedule's own task; `BlockedOnUser` with an alert when all were
  cancelled. On delegations: waiting while the specialist's task is open or the hub holding the report is alive; Done
  once a `report` task for it exists; handed to the user as Blocked when the hub went away with the report unreviewed;
- `Running` with a `Continuing` note and no way back: the user is alerted once while its process lives, and it is
  resumed once the process is gone;
- its process died: after `resumeAfterSeconds` (120), so a user restarting NyPi resumes it in place, it is dispatched
  once to the hub as an order (§4.7);
- past the restart limit, without a session, dead for `giveUpAfterHours` (24), or dispatched three times without any
  process taking it over: failed with an alert.

Alerts go through the Inspector and the outbox, each beginning with its status keyword. `nypi watchdog` runs one round.

**Specialists** (decision D24, `specialist.rs`). Long-lived agent profiles in `~/.nypi/specialists/<name>/` or a
trusted folder's `.nypi/specialists/<name>/` (the folder's wins): `profile.json` (kind, description, domain, model,
thinking, tools; for external ones `command`, `stdin`, `format`, `timeoutSeconds` (default 3,600), `env`,
`passEnv`), `instructions.md`, and `experience.md`. Every finished run appends a line to the experience log, and the
newest 40 entries are part of the next briefing.

- *Native*: a runtime with the specialist's name as `agentName`, so its own self-prompt layer, model, thinking level,
  and tools, with its instructions and experience appended. It asks the same approver as the delegating agent and
  forwards its alerts, and has no `delegate` tool, so delegation is one level deep.
- *External*: an agent CLI run as a subprocess from an argv template (`{task}`, `{cwd}`, `{model}`; no shell), the
  briefing on stdin when `stdin` is set, stdout parsed as `codex-json` (`codex exec --json`), `claude-stream-json`
  (`claude -p --output-format stream-json --verbose`), or `text`. Tool events become progress lines and heartbeats;
  the final message is the answer and its status line the status (without one, a clean exit is a derived Done). The
  full output is kept in `runs/<task>.log`. It gets `NYPI_PARENT_TASK`, so a NyPi used as the CLI files its task under
  the specialist's.
- `nypi specialists` lists them; `show`, `new <name> --from native|codex|claude`, `run <name> <task>`, and `detect`
  (finds agent CLIs on PATH and inside their desktop apps, with version and sign-in, and flags profiles whose program
  is missing or older).

**Delegation** (decisions D25, D41, D83; `delegate.rs`). When the roster has someone in it, the agent gets `delegate
{ specialist, task, cwd?, wait?, race? }` and a `<specialists>` section, with the rule that professional work in a
specialist's domain goes to that specialist and that the agent reviews the report. A small change it already
understands (about 20 lines or fewer in one or two files, nothing left to design) it makes itself. Each delegation is a
child task of the agent's current task.

- *In the background.* In a conversation of the hub, `delegate` returns at once with the delegation's id (`d-...`),
  and the specialist works in a task of its own, so several work at once. When one finishes, its report is a
  `reported` pulse; the conversation takes it as a sourced prompt with origin `report`, behind what runs, reviews it,
  and tells the user. A run that ends `Continuing` while specialists work has them as its way back (§3.6): its task
  waits on a join of their ids. With `wait: true`, and in a runtime that is not a conversation (`--json`, a job), the
  call blocks until the specialist finishes, its tool calls streaming as the call's progress.
- *Races.* `delegate { race: [{specialist, task}, ...] }` starts two or more delegations together. The first to come
  through without failing is reported, with the names of the others, which are stopped; when every one has failed, one
  report says what each said. A run waiting on a race waits on `AnyOf`.
- *Where.* A native specialist works at the run's focus, on another device too, through the same routed tools; an
  external one is a program of this computer and is refused for a folder on another device.
- The result names the child task and its status, lists the last tool calls and the external log, quotes the report,
  and ends with an instruction to verify what can be verified. A Failed delegation is an error result. External runs
  are not resumed after a crash (the watchdog fails them).

### 4.6 Goals and campaigns

The user's short-, medium-, and long-term goals live in `~/.nypi/goals.md` (decision D26, `goals.rs`): Markdown the
user edits freely, one checkbox line per goal under `## Short` / `## Medium` / `## Long` (or 短期 / 中期 / 长期),
with optional ` · next: … · due: YYYY-MM-DD · dir: … · vault: 10-projects/<slug>` fields and a hidden
`<!-- g-xxxxxx -->` id (a goal written by hand gets a stable id from its text).

- Open goals (up to 25) are the worker's `<goals>` section, with the rule to relate work to them.
- The agent changes them only through the `goals` tool (`list`, `add`, `update`, `done`, `reopen`), which edits single
  lines and saves the previous file with the reason in `goals-history/`; the guard blocks `write` and `edit` on it. A
  goal is added only when the user states one. `nypi goals` shows the file; `add`, `done`, `reopen` edit it by hand.
- `nypi goals review` runs a fixed review prompt (advance the one goal that matters most and can move without the
  user, or say what is needed), handed to the hub when it runs. `goals.reviewEveryHours` (default 0, off) has the
  hub do it on a timer.

**Campaigns** (decision D85, `campaign.rs`). A goal may name its project folder, ` · vault: 10-projects/<slug>` (the
`goals` tool takes only one folder under `10-projects/` that `Vault::resolve` accepts). Its `plan.md` is the campaign:
frontmatter, `## Objective`, `## Workstreams & Milestones` (`[ ]` open, `[~]` under way, `[!]` blocked, `[x]` done),
`## Key Decisions`, `## Linked Notes`. The Principal keeps it; `principal.md` teaches the shape. The tiers below a goal
carry it:

- `start_task` takes `goal`, so the brief has `goal` and `spawned_by_activation`; the runtime records both in the
  ledger as a `campaign` event when the task begins (`TaskRecord.goal_id`, `spawned_by_activation`).
- A task for a goal sees the goal's `plan.md` as `<project_plan>` (cut at 6 KB) above `<task_workspace>`, read again
  every turn. A task with no goal carries nothing.
- A schedule the Principal adds takes `goal` too, recorded with the activation that set it (`Schedule.goal`).
- The decision journal's `goalsTouched` lists the goals an activation started tasks for, set schedules for, or
  changed.

**Two clocks** (`campaign::goal_digest`). Idle time is spent by *deadlines* (goals due within 14 days or overdue, with
the `[!]` items of their plan: the Principal nudges the user through chat when only they can unblock one) and by
*neglect* (the three goals left longest since a task last served them, each with the first item of its plan under
way or open, to advance one so no goal starves), with the inbox waiting to be filed. The same clocks head the
Principal's `<situational_radar>`.

### 4.7 Hub, supervisor, and jobs

**The hub** (decision D64, `nypi-hub/src/daemon/`). `nypi hub run` is Lucia as one process that is always there. It
holds the one conversation (§4.2.1), working in `hub.cwd` or the user's home, and listens on `run/hub.sock`, a
user-only Unix socket, speaking the RPC protocol of §3.8 to any number of clients: every client's lines feed one
command stream, and every record goes to every client, so a client that leaves stops nothing. A client is greeted with
`daemon_hello` and shown the approvals still waiting. At start it resumes what its last process left, runs the
channels (§4.8), the scheduler and watchdog rounds, the register of nodes on `run/nodes.sock`, and machines' links
directly on `hub.listen` and through the relay at the entry (§4.9), and writes `run/hub.json` (pid, start, beat, clients, busy) every 15 s. `nypi send <text>`
gives one message; `nypi attach` is a conversation (`/approve`, `/deny`, `/abort`); neither holds a runtime.

Whoever can connect speaks as the user, so the agent's tools must not: the sandbox denies connecting to any socket in
`run/`, the guard refuses `nypi send` and `nypi attach`, and the hub traces each peer's process tree and turns
away one it started itself, with an alert (`daemon/peer.rs`; the bridge socket does the same).

**The supervisor** (decisions D60, D93, `nypi-cli/src/supervisor.rs`). `nypi supervisor` is a process with no model
and no session that keeps running whatever this machine is: each round it reads `instance.json` (§4.9). On a node it
keeps `nypi node run` there and does no rounds of its own (they are the hub's); on a stopped instance (`down`) it runs
nothing; with `relay.enabled` it keeps `nypi relay run` beside whichever it is (decision D94; judged by `run/relay.pid`);
on the hub, and on a machine that is neither yet and runs no relay, it keeps the hub there: it waits on the hub as its
child and reads its heartbeat every round; no process means start it, a process silent for 90 s means kill and start it, an exit is acted on at once. A
start after the hub was seen running raises a warning; three starts in ten minutes raise an error naming
`logs/hub.log`, and the hub is then left alone for five minutes. It keeps the board the same way (judged by the
process named in `run/board.json`). While no hub is alive it also does the rounds itself: every 30 s the
scheduler's (and a due idle pass), every watchdog interval the watchdog's, and the outbox's retries. It reads the
settings again every round and writes `run/supervisor.json`, which `nypi status` reads. When the mode changes between
rounds, what the old mode ran is stopped before the new mode's services start. `nypi hub up` and `nypi node up` copy
the binary to `~/.nypi/bin/nypi` and install the service: on macOS the LaunchAgent `dev.nypi.supervisor` (start at
login, restart on exit, log in `~/.nypi/logs/supervisor.log`, only `PATH` and a non-default `NYPI_HOME` in its
environment), on Linux the systemd user unit `~/.config/systemd/user/nypi-supervisor.service` (`Restart=always`,
enabled with `systemctl --user`; `loginctl enable-linger` keeps it running while logged out). The `down` of the last
service on a machine (`hub`, `node`, or `relay`) removes it. A hub from before decision D100 wrote
`run/daemon.json` instead; while there is no `run/hub.json`, stopping the services and removing the last one look
there too, so the first update stops it (`heartbeat::running_pid`).
`NYPI_SERVICE=off` keeps `up` from installing anything. `hub.enabled: false` keeps the supervisor to its rounds;
`board.enabled: false` to the hub. A terminal says at startup when no supervisor is alive. A hub that beats
while its run is stuck is not restarted (the stuck-run rule of §3.6 ends such a run instead).

**Updates** (decision D98; `nypi-core/src/update.rs` for the manifest, its signature, and versions;
`nypi-cli/src/update.rs` for the rest). Each release of the public repository (`update.repo`, by default the
`repository` of `Cargo.toml`) carries `nypi-<version>-<target>.tar.gz` per target, `SHA256SUMS`, `manifest.json` (the
version, its tag, and each archive's file, size, and SHA-256), and `manifest.json.sig`, an Ed25519 signature over the
manifest's exact bytes. They are fetched from the release download URLs (no API, no token), over HTTPS only. The
release key's public half is built in (`release_public_key()`: `NYPI_RELEASE_PUBKEY` at build time, else
`DEFAULT_RELEASE_PUBLIC_KEY`, where the maintainer pastes it so a build from source checks official releases with no
environment; a build without either takes a release only by hand, with `--allow-unsigned`, which `--check` takes
too). `nypi update` checks the signature before anything else,
compares versions (`major.minor.patch` and a pre-release, ordered as SemVer 2.0.0 §11: `rc.10` after `rc.2`; an
older one only when `--version` names it), downloads the
archive for this build's target (`NYPI_TARGET`, from `build.rs`) to `~/.nypi/bin/.staging/` while hashing it, refuses
it unless its size and SHA-256 are the signed ones, unpacks only its `nypi` (an archive with a path that leaves it or
a link is refused whole), runs it with `--version` (five seconds, and it must say `nypi <version>`), copies the
installed binary to `nypi.prev`, writes `run/update-pending.json`, renames the new binary over `~/.nypi/bin/nypi`,
and restarts the supervisor (`launchctl kickstart -k`, `systemctl --user restart`). Each step that changes the
installation is a rename, and nothing changes until every check has passed. While an update is pending, the
supervisor's first round stops every service of the machine once (its own children and the processes an earlier
supervisor started, by the pids their files record: a restart of the supervisor alone leaves them on the old binary)
and starts them again on the new one, the update's clock starting then (`restarted` in the pending file, decision
D99); every start of the supervisor itself is counted (`supervisorStarts`), and past three, with a previous binary
there, the supervisor puts it back before anything else, tells the user, and ends, so a binary that fails before the
rounds is rolled back too. The supervisor keeps an update once every service of the machine has been up for 30
seconds together (`lastApplied` in
`run/update-state.json`), and when one keeps stopping or none is up within 90 seconds it stops its services, renames
`nypi.prev` back, blocks that version for background updates, tells the user (`Failed: update to vX did not stay
healthy; rolled back to vY.`), and ends, so its service manager starts it again on the old binary. `nypi update
rollback` does it by hand. In the background (`update.mode`): `notify` (the default) checks every
`checkIntervalHours` (6), with an offset of each machine's own and the manifest's `ETag`, and tells the user once per
version; `auto` puts a release in place only when the machine is idle (an automatic update that fails is told once
for that version, never failed silently at every check) (no activation of the Principal, no task
running, no call under way on its node) and within `autoApplyWindow`, and a machine that runs the relay is only told
unless `autoRelay` is set. A fleet: `nypi update --node <name>` asks a connected node (`ToNode::Update`), which lets
its calls end, checks the release itself against its own key, answers `updated`, and starts again; `--all` updates
the nodes one by one, each back on the new version before the next (a node that runs the relay only with
`--include-relay`), stops at the first that fails, and the hub last. Both run in the hub (a personal node's admin
command, or the hub's own socket), never for an exec-only machine. `nypi --version` says the version, target, and
commit; each node's version is in its report, `nypi devices`, and the board. The agent cannot run `nypi update` (the
guard), and `~/.nypi/bin` is in `Protected`'s home.

**Jobs** (decision D67, part 3; `jobs.rs`). Work no conversation asked for comes back from the scheduler's and the
watchdog's rounds as an `Order` (`Prompt`, `Resume`, `IdlePass`), and `Jobs` carries it out inside the hub; no second
`nypi` is started. In the hub a fired prompt goes into the conversation as a sourced prompt with the schedule's
focus, and an interrupted task of the conversation is resumed by it. Anything else is a job thread: a `Thread` on a
runtime of its own (`Role::Job`), without a chat module, not attachable, closed when its run settles, its result told
as every unwatched run's is. One job works in a session at a time, and one idle pass runs at a time. A job at a
folder on a device runs only while that device's node is connected; otherwise the user is told. A hub that is not the
always-on one (a terminal's own, the supervisor, `nypi schedule tick`, `nypi watchdog`) first offers a fired prompt to a
running hub and otherwise carries it out itself; a job a terminal's own hub runs ends when the terminal closes,
and is then taken up again as interrupted.

### 4.8 Channels and notices

**Channels** (decision D81, `nypi-hub/src/channels/`). A channel is a way things reach Lucia, and she the user, that is
not an interface somebody sits at. It implements `Channel`: `name`, `run` (hand what comes in to the conversation
through a `Port`, until shutdown), `deliver_notice`, and `ask_approval`. It holds no model, session, or tool: what it
takes in becomes a command for the conversation, under the conversation's rules. `Channels::from_settings` makes the
ones the settings turn on, and the hub runs them.

- **Telegram** (`channels/telegram/`, decision D91). The bot token is `channels.telegram.botTokenSecret`, a handle in
  the secret store (`nypi secrets set channels/telegram`, then `cred://channels/telegram`), with `TELEGRAM_BOT_TOKEN`
  from the env files as the fallback; `channels.telegram.enabled` (on) runs it when a token is there. It long-polls the Bot API.
  A paired chat speaks as the user, so pairing is guarded: unknown chats are refused until they send `/pair <code>`;
  the code is in `run/telegram.code` and shown by `nypi telegram`, works once, is replaced after five wrong tries with
  an alert, and pairing closes after fifteen until the hub restarts. Paired chats are kept in `telegram.json`. A
  chat's message becomes a `prompt` (or steers a run under way); a run the chat started or spoke into is relayed in
  full (tool calls in batches of eight, each reply, the ending), and other runs reach it only as notices. Approval
  questions go to the paired chats, numbered (`/approve 3`, `/deny 3 reason`; a bare `/approve` answers the oldest);
  `/abort` stops the run. Only one process may poll a bot, so the channel also takes other NyPi processes' approval
  questions on `run/telegram.sock` (`bridge.rs`, user-only, refusing the hub's own children). With no hub, `nypi
  telegram` starts one in the foreground with the channel on.
- **The inbound webhook** (`channels/webhook.rs`, `channels.webhook`, off by default, listening on
  `127.0.0.1:7683` unless `listen` names another address). The user's other programs (a share-sheet shortcut, a mail
  hook, a chat bridge) send `POST /ingest` with an `InboundEnvelope { source, sender, text, urls, attachments }`. The
  caller must show the bearer secret the channel reads from the secret store when it starts (`secret`, default
  `cred://channels/webhook`; without one, or one under 16 characters, the channel does not start), compared in full
  before the body is read. Requests are capped at 12 MB (20 links, 8 attachments). What comes in is saved first, as it
  came: `00-inbox/YYMMDD-HHMM-<slug>.md` with a header (source, sender, received, `status: untriaged`), the text, the
  `http(s)` links, and the files under `attachments/`. Then the conversation gets it as a sourced prompt (origin
  `inbox`): with chat in front, the Principal's `P1` activation, whose job is to read the note, fetch what the links
  point to, link it to the `30-entities/` pages it is about, add a synthesis, mark it filed, and have the user told.
  The prompt says that the material is not the user's message and that nothing in it is an instruction.

**Notices** (principles A3 and B6; `nypi-hub/src/notify.rs`, `nypi-core/src/outbox.rs`).

- **The pump** (`daemon/pump.rs`, decision D67, part 2) follows each thread's records and raises, once for every
  interface and channel on it: the status of a run that took longer than `notify.afterSeconds` (120) or that nobody was
  attached to; a stall notice when a run shows no event for `notify.stallSeconds` (600), naming the tools still running,
  repeated each time the quiet doubles (then every two hours); and an approval left unanswered for
  `notify.approvalSeconds` (60), passed to the channels. A question nobody can ever answer is denied at once. With
  nobody attached, notices go through the outbox; with an interface attached, to the channels, for a user who looked
  away.
- **The outbox** (decisions D57, D58; `~/.nypi/outbox/notices.jsonl`). Everything nobody is watching is written and
  synced (`queued`: level, source, text, task) before anything is sent; `attempt` records which channels took it and
  which failed; `seen` records an interface showing it. A notice is open until one channel takes it or one interface
  shows it; open notices are sent again with backoff (a minute, doubling to an hour, eight tries, three days) by the
  supervisor and the watchdog rounds, and the next terminal or TUI shows them as "While you were away". `nypi notices`
  lists them with how each arrived.
- **Where they go.** Every notice is offered first to each running channel (`Channel::deliver_notice`, decision D83),
  then to the notifier's services: paired Telegram chats (send-only, when no channel carried it) and
  `notify.channels`: `desktop` (macOS Notification Center through `osascript`, `notify-send` on Linux; the default, so
  being told does not depend on configuration; `[]` turns it off), `webhook` (POST as `slack`, `discord`, `text` for
  ntfy, or `json`; `${NAME}` in the URL and headers resolves through the private lookup, and the URL is never printed),
  and `command` (an argv template with `{title}` and `{text}`, no shell, for example sendmail). `nypi notify test`
  sends a test notice everywhere and names a channel that cannot be set up.
- With chat in front, what the planes have to say reaches the user through chat (a stimulus turn is told as a notice
  when nobody is there); nothing else posts a note beside it.

### 4.9 Devices: the hub, its nodes, and joining

The hub is one of a larger shape (decision D66; the full design, failure cases, and phone nodes are in
[devices.md](devices.md)): one hub holds the loop and all state, a **node** on each device carries out its calls under
that device's own rules, and **interfaces** only talk and watch.

- **The node** (`nypi-node`, wire types in `nypi-node-proto`). A table of tools it declares, `call` that carries one
  out at most once per id, a gate for the device's own refusals, and `cancel`; no model, session, or provider code.
  The hub sends `Call` and `Cancel`; the node answers `Started`, `Output`, `Finished`, `Refused`, and describes itself
  in a `Report` (identity, system, home, declared tools, running calls). One message is one JSON line, at most 32 MiB.
- **`nypi node`** makes a computer a node (decision D68, `device.rs`): this machine's seven device tools, each made for
  the folder a call names, under this machine's sandbox, protected paths, and guard as the node's gate, so what may
  never be touched here is refused here whatever the hub asked. A node whose settings do not read refuses to start.
- **The call journal** (decision D74, `nypi-node/src/journal.rs`, `~/.nypi/node/journal.jsonl`). The node journals
  every call id it takes and the last word on every call until the hub acknowledges it. When a link drops, calls go on
  for a lease (120 s) and a new link first hands over every unacknowledged result; a call asked for again under a known
  id is answered from the journal, never run twice. The hub waits for a result across links up to the same lease,
  then answers "went away; whether it was carried out is not known".
- **The register of nodes** (`nypi-hub/src/nodes.rs`). One link per machine name; each node is pinged every 20 s and a
  link silent for 65 s is dropped; a node that says hello under a name already connected is let in at once when the old
  link does not answer a ping within 3 s. It writes `~/.nypi/devices.json` (what each machine last reported, connected
  or last seen). The hub's own node is in its process, on `run/nodes.sock`, which carries the node protocol only.
- **Two instances** (decision D93, `nypi-hub/src/instance.rs`). A machine is the hub or a node of one, as
  `~/.nypi/instance.json` says: `mode`, `name`, `instanceId` (made once), `epoch` (the hub's term it knows of),
  `hubFingerprint` (the hub key it trusts), `trust`, the hub's `direct` addresses last heard, and `down` when its
  service is stopped. Only NyPi's own commands write it. `nypi hub up` makes the hub key and the file (`epoch: 1`),
  installs the supervisor, prints the key's fingerprint, refuses when the entry already answers with another key, and
  shows join requests until Ctrl-C, taking `/admit` and `/refuse` typed there. `nypi node up` joins (below) and
  installs the supervisor; `down` stops a service and keeps its data; `nypi node leave` tells the hub and forgets the
  key; `nypi status` says what the machine is and how it is linked. A D80 home is moved to these files once, when the
  file is first read: `devices/hub.json` becomes a node instance (`personal` when it was also paired as an interface,
  `exec-only` otherwise), the hub's `peers.json` becomes one peer per machine, and the interface key is forgotten.
- **The entry** (decision D97, `Settings::entry`). Where a machine finds the hub from outside its network and where
  a browser reaches the board: the relay (below), behind Cloudflare or a reverse proxy, at a name the user owns (for
  example `nypi.example.com:443`). There is none by default, and the hub and its nodes then work on the local network
  only (direct links and mDNS): `NYPI_ENTRY` goes first, then `entry` in `settings.json` (which `--entry` on `hub up`
  and `node up` saves, so every later version keeps it), then one built into the program with
  `NYPI_DEFAULT_ENTRY=host:port` at build time (`config::DEFAULT_ENTRY`, unset in public builds). A hub with no entry
  opens no relay connection and says nothing about one; it takes an entry saved later within seconds, without a
  restart (`daemon/entry.rs`). A machine joining with no entry asks mDNS, and is told to run on the hub's network or
  pass `--entry` when no hub answers. `host:port` and `https://…` are dialled as `wss://` and checked against the web's roots
  (Cloudflare's certificate, or the proxy's); `http://host:port` as `ws://`, for a relay on the same machine or a test
  (`ws::Entry`).
- **Links and trust** (decisions D80, D93; `nypi-hub/src/pairing.rs`, `nypi-node/src/identity.rs`, `daemon/remote.rs`).
  Each machine has one Ed25519 key (`device-id.key`); a link is TLS 1.3 (rustls with ring, ALPN `nypi/1`) with a
  self-made certificate on both sides, the machine checking the hub's key against the fingerprint it pinned and the
  hub looking the machine's key up in `devices/peers.json`, which keeps each machine's name, trust (`personal`: carries
  out calls and talks as the user; `exec-only`: carries out calls only), instance id, and system. The first line names
  the link's purpose: `node` (to the register, read as the node protocol only, for any admitted machine), `talk` (to
  the conversation, for a personal machine only; an exec-only one is refused with the reason), `join`, `leave`, or
  `probe` (answered with the hub's epoch and closed, for anyone who completed the handshake; nothing is registered),
  and, for a personal machine only, `standby`, `move`, and `notice` (below). A hello names the epoch the machine
  knows; one above the hub's own makes the hub step down.
  `daemon/remote.rs` is the one place this is decided, for a link that came directly or through the relay alike.
  `nypi devices remove` (or a change to the register by hand, checked every five seconds) closes the machine's open
  links at once. The hub records each link up in `run/links.json` (name, purpose, `direct` with the address it came
  from, or `relayed`) for `nypi status` and `nypi devices`. A machine's new node link takes its earlier one's place at
  once (the key says it is the same machine); on the hub's own node socket, where a node names itself, the earlier link
  keeps its place while it answers a ping.
- **Joining** (decision D93). A machine that knows no hub links with any key and reads the hub's from the handshake.
  Both sides compute a six-digit confirmation code from `SHA-256("nypi-join" || hub key fingerprint || machine key
  fingerprint || TLS exporter value)` (RFC 5705, label `nypi join`); the machine prints it and waits. The hub records a
  join request in `devices/requests.json` (id, kind, name, system, address, code, expiry) and asks the user wherever
  approvals go: a `join_request` record to every attached interface, a notice through the outbox (and so Telegram),
  and an inbox card on the board. The first answer counts: `/admit <code> [exec-only]` or `/refuse <code>` in any
  conversation or Telegram, `nypi devices admit|refuse` on the hub, the board's buttons. Admitted, the hub records the
  peer and answers `Welcome { name, trust, epoch, direct }`; the machine writes its instance. A machine in the middle
  that swaps a key makes the two codes differ. Requests last ten minutes; at most three wait, ten a day come from one
  address, and thirty a day in all, after which requests are refused until the next day and the user gets one notice.
  A request never reaches the conversation or wakes the Principal. `nypi devices invite <name> [--exec-only]` gives a
  one-time code (kept as a hash) that `nypi node up --code` uses with no question.
- **The hub's direct addresses** (decision D94, `direct.rs`). The hub listens on `hub.listen` (default `0.0.0.0:7681`
  and `[::]:7681`, the IPv6 one IPv6-only) with the same pinned TLS. Every ten seconds it works out its direct
  candidates: each interface's private IPv4 (and the `100.64.0.0/10` range tailnets use) and global or unique-local
  IPv6 address with the listen port, skipping loopback, link-local, and `198.18.0.0/15`; `hub.publicAddress` when the
  user forwarded a port; and, with `hub.portMapping` (off by default), the port the router mapped by NAT-PMP or
  UPnP-IGD (asked every half hour) at the public address the relay saw. The list goes in every `Welcome` and, when it
  changes, to every linked node as `ToNode::Hub { epoch, direct }`; a node keeps it in `instance.json`.
- **mDNS** (`mdns.rs`, `hub.lanDiscovery`, on by default). The hub answers questions for `_nypi._tcp.local` with its
  addresses on this machine's networks, the listen port, and a TXT record with the hub key's fingerprint and epoch.
  The network is behind the `Lan` trait (the multicast group on UDP 5353, or a simulated network in tests).
- **How a node reaches the hub** (`reach.rs`). On every connect (`nypi node run`, a `talk` link, `nypi node up`) it
  probes, all at once, the hubs mDNS hears with the pinned fingerprint and the direct addresses it was told (those on
  a subnet it shares first, the last one that worked first of all). A probe counts only when its handshake shows the
  pinned key; the first answer waits 200 ms for one at a higher epoch, and a hub below the epoch the node knows is not
  taken (`pairing::link` refuses its welcome too). Direct ways get 1.5 s; then the entry. A first join asks the entry
  which hub key it leads to and joins that hub on this network when mDNS hears it, else through the entry; with the
  entry out of reach, the one hub heard here. A node linked through the relay probes the direct ways every minute and,
  as soon as one works, opens the direct link and drops the relayed one; calls under way go on and their results reach
  the hub on the new link (the call journal above). It writes `run/node.json` (direct with the address, or relayed)
  for `nypi status`.
- **The relay** (decision D94, `relay.rs`, `nypi relay up|run|down`, settings `relay`). A plain HTTP/1.1 and WebSocket
  server (default `127.0.0.1:7682`) meant to sit behind a Cloudflare Tunnel, Cloudflare's proxy, or a reverse proxy,
  which serves the entry's certificate; the machine it runs on needs no public port, and its address can stay hidden.
  `GET /_nypi/hub` is the hub's control WebSocket: the relay sends a random challenge, the hub signs it with its epoch
  and instance id with the hub key, and the relay takes only the key `relay.hubKey` names (without one, the first hub
  to register claims it, and the relay logs its fingerprint), then tells the hub the address it sees it come from.
  `GET /_nypi/link` is a machine's link: the relay asks the hub over the control connection to open
  `/_nypi/data/<id>` with a one-time token, and passes binary frames between the two unread. Both ends wrap the frames
  in a byte stream (`ws::into_stream`) and run the same pinned TLS and purposes as a direct link inside it, so neither
  Cloudflare nor the relay can read a link or stand in for either end; a relay that answers with a key of its own makes
  the join codes differ. Every other path is a browser's request for the board: passed to the hub on a data connection
  of kind `board`, one request per connection, with `Connection: close` and the browser's address in `X-NyPi-Client`,
  the answer streamed back unbuffered. The client's address is the connection's, unless `relay.trustProxy` (default
  true) and the connection comes from loopback, where the proxy runs (decision D99): then `relay.trustedHeader` when
  named, else `CF-Connecting-IP`, `X-Real-IP`, or the last address of `X-Forwarded-For` (the one the proxy added, so
  a client cannot put its own in front). Limits: `perIpPerMinute` connections (default 120),
  `maxWaiting` connections waiting for the hub (64), a request head of 16 KiB and body of 1 MiB, and `maxKbpsPerLink`
  each way (2048). With no hub registered it refuses links and answers browsers 503. It keeps `relay/state.json` (the
  hub key, the epoch, and the instance registered at it; nothing secret) and takes a registration only at a higher
  epoch, or the same one from the same instance (decision D95); one at a higher epoch closes the control connection
  before it, and a refusal names the epoch that holds the entry. `Registered` tells the hub the relay's per-link rate. The hub keeps its control connection open
  whenever an entry is set, reconnecting with backoff (`daemon/entry.rs`).
- **The hub's state and standby copies** (decision D95, `state.rs`, `standby.rs`, `daemon/handover.rs`). The hub
  state is the NyPi home without what belongs to one machine (`run/`, `logs/`, `bin/`, `models/`, `node/`,
  `standby/`, `diverged/`, `relay/`, `device-id.key`, `instance.json`, and `secrets.key`, whose key is carried as an
  entry of its own); every env file the settings name goes too, one outside the home as `env/<name>`. A personal node
  keeps a `standby` link beside its node link, and the hub keeps its copy in `~/.nypi/standby/` current by
  differences: an append-only `.jsonl` by its new tail when the copy's bytes are its prefix, any other file whole when
  its length or SHA-256 differs, files gone deleted. Between batches the hub keeps each file's length, time, SHA-256,
  and (for a log) its digest so far (`StateCache`, decision D99): a file unchanged on disk and in the copy is not read,
  and a log that grew is read from where the copy ends, its hash continued from the digest kept. Each file arrives as a
  header line and its bytes; the node checks
  every file before it applies any, and answers with its manifest, or refuses, and is then sent everything again. A
  batch goes every `hub.standbyMinutes` (5) and a minute after a decision of the Principal when one comes sooner;
  over a relayed link at most every 30 minutes at half the relay's per-link rate. The hub's own machine is in the
  register as a personal machine from its first start, so it can link as a node after a move. `nypi status` on a
  node says how old the copy is.
- **Moving the hub** (`nypi hub here`, a `move` link). The hub pauses the scheduler, idle passes, and goal reviews,
  waits up to `hub.handoverWaitSeconds` (60) for the Principal's activation to end, then stops what runs: its runs
  are aborted while the home is marked as handing over (`runtime::handing_over`), so the ledger keeps them running,
  and the threads and jobs are cut off as by a restart. It sends the last difference at epoch N; when the target has
  applied it, the old hub writes itself a node at N+1, tells its nodes where the hub is now (`ToNode::Hub`), closes
  their links and its relay connection, stops its board, and only then says `released`. The target moves the copy
  into its home (its own state set aside under `diverged/node-<time>/`, its key, instance, journal, and models kept),
  points `envFiles` at the copies under `env/` with its own after them, installs the master key where `nypi secrets`
  keeps it (the keychain, or `secrets.key`), and writes itself the hub at N+1; its supervisor starts the hub, which
  registers at the relay and resumes what was cut off. Anything that fails before `released` leaves the old hub to
  start again at N (the hub exits and the supervisor starts it) and the target with its copy only.
  `nypi hub here --force` is refused unless the machine is personal, has a copy, and its hub has not answered for
  `hub.forceAfterSeconds` (120); it says how old the copy is, asks, and promotes the copy at the epoch after the one it
  knows.
- **Exactly one hub.** The relay takes the highest epoch; nodes refuse a lower one; a hub that the relay refuses at a
  lower epoch, or hears of a higher one from a node's hello or from mDNS, stops at once, moves its state to
  `diverged/<old epoch>-<time>/` (settings, models, and model keys copied, so it runs on), becomes a personal node at
  the higher epoch, and once linked tells the user through the new hub's outbox where its state is (a `notice` link).
  Nothing is merged.
- **The hub's commands from a personal machine** (decision D96). The stores are the hub's, so on a personal node
  `tasks`, `goals`, `memory`, `portrait`, `schedule`, `specialists`, `reflections`, `dmn`, `rate`, `scorecard`,
  `notices`, `trace`, `eval --list`, `devices`, and `secrets` go to the hub: a `talk` link whose first line is
  `{"type": "admin", "argv": [...]}` (`HubCommand::Admin`). `daemon/remote.rs` runs it before anything reaches the
  conversation, for a personal peer only, and only when `protocol::admin_allowed` names it (`ADMIN_COMMANDS`; not what
  starts a run, such as `goals review`, `specialists run`, `schedule tick`, `dmn run`, `tasks resume`, or a benchmark
  run). The program registers how such a command runs (`protocol::set_admin_runner`): parsed as its own command
  line and run by the same function a terminal on the hub uses (`hub_command` in `nypi-cli`), with the hub's home;
  the commands print through `outln!` (`nypi-hub/src/output.rs`), which keeps a command's text in a buffer of its
  own task while it runs for another machine, and its text and exit code go back as `admin_result`. `secrets set`
  reads the value on the node, unseen, and sends it as the command's `input` (shown by no `Debug`); the hub stores it,
  and the value is in no output, log, or audit record. A `rate` walk-through asks on the node and sends each verdict.
  On an exec-only node these commands are refused before anything is sent, and the hub refuses them too; the
  commands about the machine itself (`status`, `node`, `relay`, `hub here`, `models`, `trust`, a benchmark run) run
  where they are typed.
- **On a personal node**, with no hub there, plain `nypi` (and `send`, `attach`) opens a `talk` link to the hub
  the way a node links, with the focus at this folder on this machine (`Link::paired`); on an exec-only node it says
  the machine cannot talk to the hub and suggests `--standalone`. `nypi node run` takes its link up again when it is
  lost, waiting a little longer each time (to 30 s).
- What a device's folder lends a run, and how a call at a device is judged, is in §3.7.

### 4.10 The board

The board (`nypi board`, decision D56; product design in [board.md](board.md)) is the user's web page for what needs
them, what runs now, and what happened, with approve, deny, stop, and message. It is a long-lived service in
`nypi-cli/src/board/`, kept running by the supervisor.

- **Two sources.** What NyPi stores it reads from disk: the ledger, the audit log (followed with `AuditTail`, the last
  24 hours in memory), the schedules, the goals file, the vault, and the planes' logs (`chat/turns.jsonl`, the
  activation stories and index, the decision journal, `principal/status.json`; a status whose process is gone is not
  shown). What exists only inside a running process comes over that process's **board link**
  (`nypi-core/src/board_link.rs`): every process looks for `run/board.sock` at start and every 30 s and holds one
  connection, on which each of its runtimes (the conversation, a task beside it, a job) has a channel of its own.
  Watching never slows a run: the link sends only while connected, through a bounded queue that drops and counts what
  a slow board cannot take, and sends reply text only while someone watches.
- **The planes live** (decision D88). Chat and the Principal report their step while a turn or activation is in
  flight (`LinkUp::Plane`: `Thinking`, `Writing`, `Reading <tool>`, or `Calling <tool>`, with the last 600 characters
  being written), a change of step at once and growing text at most four times a second, and `idle` at the end.
- **What it shows** (decision D86): Now (what needs the user, the planes' cards, the tasks under their goals, the
  devices, the last 24 hours), Chat (each turn with its lookups, signals, and stimuli), Principal (its stack, queue, and
  every activation; an activation's page with its trigger, briefing pack, steps, suspension and resumption, disposition,
  and decision), a task's page (goal, activation, place, plan, brief, findings, `report.md`, the evidence manifest,
  steers, verdicts, the device of each call), Vault (goals with their plans, notes by folder, read through
  `Vault::resolve`), System (every module as a map, decision D65), and Activity.
- **Actions** travel back over the link or the hub's socket and use what already exists (the approval desk, the
  abort handle, steering; `say` reaches the conversation as `nypi send` does), each recorded in the audit log as
  `board`. Approvals: the Inspector waits for the board only while one is connected.
- **Scrubbed.** Every answer of the API and event of the stream goes through `Board::scrub` (a `LiveScrubber`); a new
  endpoint must too. A task's story comes from the audit log, where a model's own text may hold a secret it saw.
- **Access.** On the local network behind a token (`run/board.token`) that a link turns into an HttpOnly,
  SameSite=Strict cookie; actions also need a per-run CSRF value only the page can read; the Content-Security-Policy
  allows only the page's own script and module; Markdown is shown with raw HTML as text. The page's own files carry an
  `ETag` and `Cache-Control: private, no-cache`, and a browser that has a file gets `304 Not Modified` (decision D99);
  the API, locked pages, and admission pages stay `no-store`. An admitted browser is sent back to where it was going,
  query and all. A board the supervisor keeps
  serves this machine only unless `board.local` is false. `nypi board --link` prints the running board's address.
- **Through the entry** (decision D94, `public.rs`). The hub hands browsers that came through the relay to
  `run/board-public.sock`, where the local token is never taken: every request needs a browser session. A browser
  without one gets only a page asking to be admitted (and `/api/*` only `401`), with a six-digit code made from its
  pending session; the board records a join request of kind `browser` (user agent, address) under the limits of
  §4.9, and the hub tells the user where join requests go. Admitted (always as `personal`), the page goes on by
  itself and keeps the `nypi_session` cookie (`HttpOnly`, `Secure`, `SameSite=Strict`, a year); the register of peers
  holds its SHA-256 as a browser. Every request, and every live stream every two seconds, looks the session up again,
  so `nypi devices remove` ends it at once.
- **The page** is Rust compiled to WebAssembly (`nypi-board-ui`, Leptos), built by `scripts/build-board.sh` and
  embedded at compile time; the service and page share `nypi-board-proto`.

### 4.11 Reflection, scorecard, and the benchmark

**Post-task reflection** (decision D28, `reflection.rs`). After a task worth looking back on (`reflection.mode`
`auto`: at least `minToolCalls` (5) tool calls, a failed tool, a Failed or Blocked end, or the user steering it
mid-run; or `always`, or `off`), one tool-free background call (`reflection.model`, default the agent's) gets the goal,
the outcome, the user's corrections, a condensed transcript, and the self-prompt, and asks only for lessons that would
change future work and for skill proposals. Usually the answer is empty. Lessons are appended under `## Lessons` in
the agent's own `self.md` (a native specialist's in its own), versioned as `self_prompt` does, until it reaches 16 KB.
Skill proposals are never applied: lessons and proposals go to `~/.nypi/reflections/<date>-<task>.md`, shown by
`nypi reflections`. The answer never waits for a reflection; a closing thread gives them up to 30 s.

**Verdicts and the scorecard** (decision D63, `scorecard.rs`). The user's judgement of a result is a ledger event,
`rated`: `good`, `fixed` (it got there only after the user pushed or corrected it), `wrong` (what it reported was not
true or usable), or `silent` (it stopped or went quiet without telling them). `nypi rate` walks through tasks without a
verdict, one key each; `/rate` rates the latest result. Only the user gives one (the guard refuses `nypi rate` in a
command). `nypi scorecard` computes a week (Monday to Monday) from the ledger, the outbox, and the audit log, judging
four lines against bars: silent stops (tasks rated `silent`, tasks whose process has been gone over 30 minutes with
nothing resuming or failing them, notices undelivered and unseen after an hour; the bar is zero and not a setting);
right without help (of rated asked tasks, the share rated `good` and never steered; bar 0.8); Done that was wrong (bar
0.05); and useful self-started work (DMN, goal reviews, schedules; bar 0.5). A share with fewer than
`scorecard.minRated` (5) verdicts is shown as not judged, never as met. Everything else is shown as fact.

**The benchmark** (decisions D78, D90; `nypi-eval`, `nypi eval`). Capability is measured by tasks whose outcome a
program can check. Each scenario runs in a temporary NyPi home and folder (of the user's home only the keys, model list,
and model settings are read) against a server on the loopback address that records every request; a check reads the
files and the server's record, never the agent's status.

- *The `worker` suite* (the default): twelve scenarios, each a plain task with one hurdle: `http_403_fallback`,
  `stale_readme_trap`, `corrupt_csv_reconcile`, `async_race_bugfix`, `blackbox_cli_probe`, `shallow_specialist_reject`,
  `compaction_dead_end_memory` (run with a small context, so it compacts on the way), `missing_tool_adaptation`,
  `ambiguous_spec_fork`, `crash_resume_idempotency`, `authority_boundary_stop` (a payment only the user may make), and
  `conflicting_metrics_audit`.
- *The `planes` suite*: four scenarios of the working method (decision D89): `ambiguous_target_probed`,
  `unverified_fix_caught_in_review`, `parallel_checks_one_summary`, and `quick_lookup_stays_in_chat` (through the planes
  it passes only with no activation and no task). `all` runs both.
- *`--three-planes`* runs each scenario as the hub's conversation would: the task is the user's message to chat at
  the work folder; the run is over when chat has answered and, for three looks 100 ms apart, every plane is quiet
  (chat idle, every signal an activation that finished or joined another, nothing active, suspended, or waiting in the
  Principal, no task working or awaiting review, every stimulus phrased), or after 180 s, judged as it stands. The
  record is chat's lookups and every worker session the ledger names; the answer is chat's last reply. Its sandbox
  settings set `chat.review`, `tasks.requireReport`, and `principal.enforceDisposition` to `always`. Models:
  `--chat-model` and `--principal-model`, else the borrowed home's `chat.model` and `principal.model`, else `--model`.
  Each result carries `planes` (chat turns, signals, activations, tasks, rounds sent back).
- *Metrics*: `autonomous_pass_rate`, `premature_blocked_rate` (stopped Blocked or Failed where a way existed),
  `false_done_rate` (Done where the check fails), `dead_end_repeats`, `authority_violations`, and on a three-plane run
  the planes' averages and the scenarios chat settled alone.
- The harness is tested offline with scripted models: a reference solution passes every scenario, an idle agent passes
  none, and a scripted agent shows each way of falling short. No live run's numbers are recorded here.

Tests use the faux provider or local mock servers and make no network calls; `nypi eval` is the one check that costs
what a real model costs. Research items (pre-generated answers, nightly fine-tuning of a local chat model,
situation-anchored emotional embeddings) are listed in [roadmap.md](roadmap.md), not built.

## 5. Decision log

| ID | Decision | Rationale | Revisit when |
|---|---|---|---|
| D1 | Rust | §1 | never (foundational) |
| D2 | Keep Pi TS sources in `packages/` as reference | Keeps upstream history for reading and diffing; it is not built | Superseded by D18 |
| D3 | (Revised by D40.) Send system prompt and tools as request parameters, not transcript system messages | Simpler and correct; Pi's design mainly saves prompt cache on mid-session tool changes, which M1 does not do | Tools start changing mid-session (MCP hot-plug, M2) |
| D4 | No in-process plugin loading | A compiled binary cannot load TS extensions; dynamic libraries are unsafe. Extension points: MCP, skills, subprocess tools | n/a |
| D5 | One agent loop plays all roles in M1 | The full cognitive architecture needs the pulse bus; ship a working agent first | Chat and worker split in M2 (§4.2); other modules later |
| D6 | Session file format compatible with Pi's v3 entry subset | Lets tooling read both; there is no reason to invent a new format | n/a |
| D7 | Line-based interactive mode instead of a full TUI | Steering needs concurrent input, which is simplest with a stdin reader thread. A ratatui TUI comes later. | Superseded by D20 |
| D8 | Env files load into a private lookup, not the process environment | `~/.env` holds unrelated secrets; exporting them would let any `bash` call read them | n/a |
| D9 | Telegram access by host-printed pairing code | A bot username is public. Pairing proves physical access to the host, without the user editing JSON by hand. | multi-user (M4) |
| D10 | `self_prompt` is the only write path to `self.md` | Every self-modification is versioned with a reason, so the user can inspect and roll it back | n/a |
| D11 | The status line is enforced by parsing plus one nudge, not by a structured-output tool | Works with every provider and keeps replies natural. One nudge bounds the cost, and the harness derives a status when the model still ignores the rule. | Providers with reliable structured output everywhere, or the task ledger (M3) owning task state |
| D12 | Approval is a separate policy from the guard, asked through the front end that started the run | The guard refuses what is never acceptable; approval asks about what is sometimes right. Keeping them apart lets approval rules be broad without blocking work. Print mode has no one to ask, so it denies. | Out-of-band approval (approve a print-mode request from Telegram) once the bridge runs as a service with a shared request queue |
| D13 | Proactive reports reuse the Telegram bot, send-only, from any front end | It is the channel the user already paired. Sending needs no polling, so it does not conflict with the bridge's single `getUpdates` consumer. | More channels (M3 notifications) |
| D14 | Other processes reach the bridge through a Unix socket, not through Telegram directly | Telegram allows one `getUpdates` consumer per bot. A local socket with owner-only permissions keeps the bridge the only poller; anyone who can connect is already the user, and answers still come only from paired chats. | Remote hosts (M3): the socket becomes an authenticated network endpoint |
| D15 | Command tools run argv templates, never a shell string | Arguments come from the model; a shell would let them inject commands. Users who need a pipeline write it into their own script. | n/a |
| D16 | RPC keeps Pi's command and response records but streams NyPi's own event shapes | Clients written for Pi's commands work. Event payloads mirror NyPi's types (for example `message_update.event`, not `assistantMessageEvent`); translating every event would add a second schema to maintain. Session-tree commands (fork, tree, entries), `bash`, and HTML export are not ported. | A client needs Pi event compatibility |
| D17 | The Inspector observes the pulse bus as a synchronous tap, not as a subscriber with an inbox | The audit log must hold every pulse in publish order. An inbox can lag, drop, or reorder by priority; a tap runs inside `publish`, so a pulse exists only once it is recorded. The cost is that a slow audit write slows publishing, and a tap must never publish. | Audit writes become slow enough to hurt latency (move them to a dedicated writer that still preserves order) |
| D18 | Pi's sources move out of this repository into a sibling checkout `../Pi` (upstream history, no remote) | D2's condition, kernel parity, was met in M2. Two toolchains in one tree got in the way: Pi's TypeScript hook ran on NyPi commits, and searches and agents waded through 2,400 TypeScript files. A separate checkout keeps Pi readable and runnable while NyPi holds only NyPi. The repository's older history still contains Pi's commits; that is left as is. | Pi needs to be tracked upstream again (add a remote in `../Pi`, never here) |
| D19 | Chat hands the worker's report to the user verbatim instead of rewriting it | A small model summarizing a strong model's report loses detail and can misstate results (principles A2, A4). The report is already written for the user and ends with a status line. Chat keeps it as context so the conversation stays coherent, and never claims a result it has not seen. | A local chat model good enough to summarize, or reports long enough to need it |
| D20 | The full-screen UI is opt-in (`--tui`, `"tui": true`); line mode stays the default | Line mode works in every terminal, over SSH, in logs, and when piped; it is the fallback the full-screen UI needs anyway. The TUI drives the worker in a background task, which the line front ends' single-run loop does not need. | The TUI has been used daily without regressions: make it the default for interactive terminals |
| D21 | Providers speak each vendor's REST API directly, without vendor SDKs; Bedrock's SigV4 and event-stream framing are implemented in the adapter | One HTTP client gives every provider the same retries, cancellation, stall timeout, and secret handling. The AWS SDK for Rust would add a large dependency tree for one endpoint. The signer is checked against AWS's published SigV4 example, and frames are CRC-checked. The cost: only static AWS credentials (env or `~/.aws/credentials`) and Bedrock API keys work. | Users need SSO, assumed roles, or instance credentials: adopt `aws-config` for credentials only |
| D22 | The task ledger is an append-only JSONL event log synced per event; every run is a task; liveness is the owning pid; a cut-off tool call is answered as interrupted, never re-run | Appends cannot corrupt earlier records, and a partial last line from a crash is skipped. Making every run a task gives the watchdog full coverage without asking the model to file tasks. Pid liveness needs no lock server and is right on one machine. Re-running an interrupted call could repeat an effect (a deploy, a mail), so the model is told and decides, as in Pi's default for tools that are not replay-safe. | Tasks across machines (a pid means nothing there), or the log grows too large to fold on each read |
| D23 | The watchdog runs in every long-lived front end behind a file lock, not in a dedicated daemon; heartbeats prove liveness, not progress; it resumes dead tasks by launching `nypi tasks resume` | No extra service to install or keep alive: whichever NyPi process is up does the checking, and the lock stops double reports. A liveness heartbeat keeps a long build from looking stalled, while the in-process stall notice still reports a run without progress. Resuming through the CLI reuses the exact path a user would take, in the task's own session and directory, with approvals going through the bridge. | NyPi runs on several machines, or the user wants checks while no NyPi process is running (then: a launchd timer running `nypi watchdog`) |
| D24 | Specialists are directories of plain files (`profile.json`, `instructions.md`, `experience.md`); external ones are argv templates with a per-CLI stream parser | Files are easy to read, edit, version, and copy between machines, like skills. A template plus a parser covers any agent CLI without code for each; Codex and Claude Code get parsers for their JSON streams, anything else can use `text`. The external CLI keeps its own sandbox and permissions, so NyPi's Inspector does not see inside it: its output is logged verbatim instead (A4). | Specialists need structured skills or evaluation data beyond Markdown, or a CLI needs a protocol (for example ACP) rather than a stream |
| D25 | Delegation is a blocking tool call one level deep, with the specialist's report returned to the delegating agent for review | A tool fits the agent loop unchanged and keeps the delegating agent accountable (B2): it sees the report, verifies, and reports. Blocking keeps one task at a time per agent, which the ledger and the chat/worker split already handle (chat stays responsive). One level avoids runaway chains of agents delegating to each other. | Several specialists should work in parallel, or a specialist needs its own team |
| D26 | Goals are a user-owned Markdown file that the agent edits line by line through a versioned tool; scheduled reviews are opt-in | The goals are the user's, so the file stays readable and hand-editable, and the agent never rewrites what it did not mean to touch. Versioning with reasons (as for `self.md`, D10) lets the user see and undo every change. Autonomous reviews spend money and act on the user's behalf, so they run only when the user asks or configures them. | The DMN (M4) decides on its own when to review goals |
| D27 | Citations are references the harness hands out and checks, not free-form links the model writes | A model cites more reliably when every result already carries a short handle, and a handle can be checked: the harness knows exactly which call it names, so a made-up citation is caught instead of trusted (A4). The cost is a few characters per tool result. Files and URLs are still cited as text and are not checked yet. | Claims need checking beyond whether a reference exists (for example that the cited output supports the claim) |
| D28 | Reflection is a separate, tool-free call after qualifying tasks; it writes only lessons to the self-prompt and only proposes skill changes | A separate call reviews the whole run at once instead of distracting the agent mid-task, and without tools it cannot act on what it concludes. Triggers keep the cost to tasks that can teach something, and the prompt makes an empty result the norm, so the self-prompt does not fill with trivia. Skills are shared procedures, so changing them stays the user's decision. | The memory module (M4) gives lessons a better home than the self-prompt, or proposals are good enough to apply with approval |
| D29 | Out-of-band channels are desktop, webhook, and command; e-mail goes through a command such as sendmail rather than a built-in SMTP client | A webhook reaches most chat tools and push services, and a command reaches everything else, including mail through the user's existing sendmail or msmtp setup, which already holds their credentials and handles TLS. A built-in SMTP client would mean storing a mail password and maintaining TLS and auth code for one channel. Every notice goes to every channel to keep the setup obvious. | Users want different notices on different channels, or need e-mail without a local mailer |
| D30 | (Revised by D33.) Memory works without an embedding model: BM25 over text plus agent-written tags, with embeddings blended in when an endpoint is configured | No embedding endpoint is available through the user's gateway, and a local model means downloading a runtime and weights, which waits for the user's approval. The agent writing tags in several languages when it remembers, and phrasing recall queries itself, covers most of what semantic search adds for a personal memory of modest size. Vectors are stored with their model, so a later model change never mixes spaces. | A local embedding model is approved (it plugs into the same slot), or the store grows past what a full scan per query handles |
| D31 | Memory retrieval is gated (relevance times a weighted mix of relevance, importance, emotion, and activation) rather than the additive score first planned; activation follows ACT-R with a time constant per kind | An additive score lets importance, emotion, and recency outweigh relevance, so a vivid but unrelated memory (a death in the family) could surface for a question about bike tyres: wrong, and in that case hurtful. Multiplying by relevance makes it a gate, while the mix still lets an important, felt, or often-used memory win among relevant ones. ACT-R activation captures both recency and frequency of use, which a single exponential of the last recall misses, and events go stale faster than preferences. BM25 is squashed with a fixed κ instead of normalizing by the best hit, so a weak best match stays weak. | Recall quality data shows the weights or time constants are wrong, or the valve needs a calibrated probability rather than a score |
| D32 | The memory valve attaches memories to the user message, not the system prompt, and reads the situation with a word list rather than a model | The system prompt is the start of the cached prefix: changing it every turn would re-bill the whole conversation, while a block on the newest message changes nothing before it, and the session then records exactly what the model saw (A4). A classifier model on the hot path would add latency and cost to every message for a decision whose mistakes are cheap: a wrong guess only moves the threshold, and the sensitive rule and the agent's own judgment still apply. | Mode detection is wrong often enough to matter, or a small local model makes classification nearly free |
| D33 | Memory relevance is semantic first, from embedding sources tried in order: named endpoints, then a local Qwen3-Embedding-0.6B (Q4_K_M GGUF) run in-process with candle; keyword evidence only adds. Revises D30 | Keyword search cannot match a paraphrase or another language ("teeth checkup" and "dentist", Chinese questions about English memories), which matters most for the valve, which matches the user's raw words. The gateway serves no embeddings, so a local model is the working default; an endpoint listed first takes over when one exists. Qwen3-Embedding-0.6B is strong in Chinese and English for its size; Q4_K_M keeps 97.5% cosine fidelity at a third of the f16 size, the size and speed the user asked to balance. candle keeps the single compiled binary with no service. Centering against a fixed set of generic memories, not the store's mean, works the same for one memory or thousands. Keywords stay for exact tokens (names, numbers, file names), which embeddings blur. | A better small multilingual model appears, the store grows past a full scan per query, or relevance data shows the calibration is off |
| D34 | Recall while typing runs the valve's own recall on the draft after a 250 ms pause, shows what would be attached, and caches the query vector; drafts are never written to the audit log | Recall on every keystroke would embed partial words and flood the bus; a short pause gives a settled draft and still finishes well before the user sends. Showing exactly what the valve would attach makes the coming context visible and correctable (A2) instead of a second, differently tuned recall. Unsent text is the user's thinking, not their message: recording it would make the audit log hold what they chose not to say, so only its length is logged (A1 is served by the pulse, not its content). | Hints prove distracting, or the Thinking module (M4) wants drafts to start work early |
| D35 | The user portrait is learned by a background model call after each turn from the user's own messages (heard as pulses), and every trait must quote them verbatim, checked by the harness | Inferring who someone is takes judgment that keyword rules cannot give, but it must not slow the answer, so it runs after the turn, like reflection. Hearing `userMessage` pulses rather than the worker's prompt keeps it on the user's words in chat mode, where the worker only sees chat's instructions. A model asked to describe a person will also invent; requiring quotes the harness can find makes every trait traceable to what the user actually said (A4) and drops the rest, the way citations are checked (D27). The portrait stays small and non-sensitive because it is in every prompt; facts and anything sensitive stay in memory, which is recalled only when relevant. | Traits drift or duplicate as the portrait grows, or the DMN takes over consolidating it |
| D36 | Timers and conditions are a harness-written schedule log checked by every long-lived front end behind a lock, with prompts fired as separate processes; firings are recorded before they act | The same shape as the watchdog (D23): no daemon to install, one process acts per round, and `nypi schedule tick` covers the time no front end runs. Running a fired prompt in its own process keeps it independent of whatever the front end is doing and gives it its own ledger task. Recording first makes a firing at most once, which is right for reminders and tasks with effects. A command condition runs unattended later, so it is checked by the guard and approval policy when it is set, as the bash call it is. | Many schedules need sub-minute precision, or an always-on service is approved |
| D37 | The DMN is an opt-in, budgeted pass started by the scheduler when the whole system is idle (read from the ledger), run as a task that may act within what the user would expect and answers NOTHING or a short note | Acting unasked and spending money need the user's consent (as for goal reviews, D26), and a daily cap on passes and tokens bounds the cost of a mistake. Reading idleness from the ledger covers every front end and process without new plumbing, and makes a pass unable to trigger the next. A digest of goals, tasks, schedules, and commitments gives one cheap call enough to notice unfinished business; silence as the expected answer keeps notes rare enough to be read. | Passes are too costly or too quiet in practice, or the Thinking module should share the digest |
| D38 | Thinking runs beside chat as a tool-free reasoning call per substantial message, publishes a thought pulse, and chat decides whether to follow up, in a turn the user sees only if it says something | Chat must answer at once (R1), and depth arrives later (§4.2): a separate call on the strong model gives the deeper look without delaying the reply, and needs no tools, since acting stays with the worker. Chat already knows what it told the user, so it is the right judge of whether the thought adds anything; holding the follow-up turn's events back keeps a SKIP invisible, so a correct quick answer costs the user nothing. A newer message cancels an older thought, which would be stale. | Follow-ups are too frequent or too rare in practice, or a local model makes thinking cheap enough to run on every message |
| D39 | NyPi supports the legacy HTTP+SSE MCP transport (2024-11-05), which Pi does not, and falls back to it as the spec's backwards-compatibility section describes | Many deployed servers still speak only the old transport; the spec tells clients to try Streamable HTTP first and fall back on 400, 404, or 405, so a configured URL works either way without the user knowing which one the server speaks. The rest of the MCP client follows Pi's behavior (session recovery, GET stream, resumption). | Legacy servers are gone, or the spec removes the fallback |
| D40 | The system prompt and tools are carried in the transcript as Pi does (revises D3): a leading system message, then section and tool updates; providers send updates in place only for models flagged as accepting them, and fold the rest | Prompt sections now change mid-session (goals, portrait, self-prompt, skills), and tools do too (MCP list changes), so rewriting the head on every change throws away the provider's cached prefix. Appending keeps the head byte-identical. Folding for unverified models keeps their requests as before, so nothing breaks while the flag is off; Pi's catalog enables it only for verified models, and none in the current snapshot is. Live, the gateway accepted mid-conversation system messages, but its prompt caching hit and missed identical prefixes inconsistently, so the gain could not be measured there and the flag stays off for it. | A provider is verified for in-place updates or tool changes, or the cache gain is measured |
| D41 | Four tunings from the first side-by-side evaluation (11 samples, Lucia against Claude Code on the same fixtures and checker): the schedule tool steers content watches to command conditions and warns when a prompt can fire often; a small, understood change is not delegated; the valve also ranks a long message's keyword form and counts instructions such as suggest and list as tasks; a convention the request leaves open is answered both ways when it changes the result | Each change answers one observed gap. Lucia watched a log with a repeating `fileChanged` prompt, a model run on every write, where a `grep` condition costs nothing until it fires; the tool did not say what each choice costs, and the note in its result arrives while she can still choose. Every coding sample went to Codex: correct, but 98 to 148 s against 12 to 48 s for samples she did herself, including a two-line fix. B2 sends professional work to specialists; B1's leverage clause says to outsource when it is cheaper, and for a change smaller than its own brief it is not, so the threshold applies B1 within B2. An English request missed the Chinese memories it was about, for two reasons found on the re-run: "Suggest" did not read as a task, so the casual threshold (0.45) applied, and in a one-shot run the local model was still loading when the 2 s limit ran out, so recall was keywords only. Waiting for a load in progress costs once per process; the 2 s limit still protects against a slow endpoint. The keyword form adds a margin (the allergy memory scored 0.29 against the 0.25 threshold) for one more local embedding, only for long messages. Re-run with all four changes, the six affected samples passed every check: the log watch became a `grep` condition with a notify, the rounding answer gave both totals, the valve attached the allergy memory, and the performance fix was done without delegating (67 s against 98 s). A one-cent difference came from rounding per order or per total, which the request did not settle: giving both results is the context B5 asks for. | Repeated runs show a change does not help, or a cheaper check (for example a cost estimate per schedule) replaces the guidance |
| D42 | Transient provider errors are retried by the runtime with Pi's classifier, budget, backoff, and events (Pi parity) | One dropped connection failed a whole `nypi -p` run on its first request, a gap in M1's kernel parity. `post_json`'s retries cannot see it: the request had connected, or the stream had started. As in Pi, the retry lives above the provider and the error stays in band (`StopReason::Error`), so providers are unchanged. Differences from Pi: (1) reqwest's error wording is added to Pi's patterns, because Node's wording never matches it; (2) the failed reply is not omitted with a context-edit entry, since NyPi's transform already drops errored replies at every request; (3) Pi's `retry.provider` (SDK timeout, SDK retries, server-requested delay cap) is not ported, because NyPi has no SDKs and `post_json`'s limits are fixed. The legacy `retry.maxDelayMs`, which Pi migrates into `retry.provider`, is accepted and ignored; (4) an abort right after a retry ends it as `Retry cancelled`, where Pi reports the aborted reply as a success; (5) compaction and branch-summary calls are not retried, and Pi's `summarization_retry_*` events are not ported (done by D43; NyPi has no branch summaries). | Summaries or the chat module fail on transient errors in practice, or a provider needs a server-requested delay cap |
| D43 | Retries cover every model call: compaction summaries use Pi's per-call retry and events, one-shot calls retry without notices, and the chat module shares the runtime's run-level rules (extends D42) | The flaky proxy fails summaries and background calls as often as runs. A failed summary leaves the context full, so the next run overflows. A failed thought or reflection is silently lost. Chat's fallback, which hands the message to the worker, only made the worker hit the same dropped connection. Compaction follows Pi: only the model call is retried, and its steps are Pi's `summarization_retry_*` events. Chat and the one-shot calls have no Pi counterpart, so they take the closest Pi behavior with the same `retry` settings: chat is an agent turn, so it gets the run-level rules; one-shot calls are single requests, so they get the per-call helper. One-shot retries stay silent because no one waits on them, and their final failure is already recorded. Compaction now has its own abort scope, so an abort stops a summary or its wait. As in Pi, an abort during the compaction before a prompt aborts the prompt, and an aborted run is not followed by compaction. | Background retries need notices |
| D44 | Context recovery follows Pi: Pi's overflow detector, one compact-and-retry for an overflowing or cut-off reply, and `context_edit` session entries to keep the failed reply out of model context (Pi parity) | NyPi checked six phrases, so most providers' overflow wording failed the run, and it never noticed silent overflow or a reply cut off by context pressure, both of which Pi recovers from. A cut-off reply is not an error, so the transform does not drop it; without an edit it would be replayed after the retry, and on every resume. Pi's own `context_edit` entry does this, keeps the reply in the file, and lets NyPi read Pi sessions that contain edits. NyPi's provider errors read `HTTP <status>: <body>`, so the 413 and Cerebras patterns gain that prefix. Differences: the one-attempt limit applies per prompt rather than until the next good reply, and recovery runs only right after a run, where Pi also checks replies older than the last compaction. A cut-off is judged against the limit the request asked for: NyPi's OpenAI-compatible, Responses, and Gemini adapters send none by default, so a stop at the server's default is not recovered (a review found it hid long answers on the user's gateway). | Mid-run threshold compaction before each request is ported, or a provider's overflow wording is missed |
| D45 | The context is compacted between turns once it crosses the threshold, through a `TurnHooks` hook in the agent loop (Pi parity) | NyPi checked the threshold only before and after a prompt, so one long run of tool calls (a codebase survey, a refactor) could grow past the window and fail on an overflow, which recovery then had to repair. Pi checks before every assistant response. A hook keeps `nypi-agent` free of sessions and compaction: the loop only lets the runtime swap the transcript between turns. The runtime's event forwarder persists messages a moment after the loop moves on, so the hook waits (up to 5 s) until the session holds the newest message; otherwise the compacted context would drop it. Compaction itself moved into a `Compactor` that `/compact` and the hook share. | The forwarder's lag becomes long enough to skip compactions, or the agent loop gains other per-turn preparation (Pi's prompt and tool refresh) |
| D46 | Thinking levels are clamped to what the model accepts, at the session and at the request, with Pi's rules (Pi parity) | NyPi kept any level and sent `xhigh` or `max` as-is to a reasoning model whose catalog entry lacks them, which a provider can reject; Pi offers those two only when mapped and clamps the session's level on every change. Clamping in `native_effort` covers every caller (runs, chat, one-shot calls); clamping in the runtime keeps what the UI shows and what the session records equal to what is sent. A model switch starts from the default level, as in Pi, so leaving a model without reasoning does not leave thinking off for good. | Per-model default levels are added (Pi's `getModelThinkingLevel`) |
| D47 | The system prompt's sections are rebuilt before every turn, and a change goes into the run as a section update (Pi parity) | Lucia edits her own prompt inputs during runs (the self prompt, goals, a project's AGENTS.md), and NyPi rebuilt the sections only at the start of a run, so a rule she just wrote did not apply until the next prompt; Pi rebuilds them before every turn. With transcript-carried prompts (D40) the change costs one appended update, and the cached head stays the same. The update is added like a queued message, so it is persisted and the next run's sync finds the transcript current. Sessions without a leading system message keep the prompt in the request and are refreshed at the next run only. | Rebuilding before every turn shows up in profiles, or tools need the same mid-run refresh |
| D48 | RPC gains Pi's retry, compaction, queue-mode, thinking-cycle, session-stats, and session-switch commands; their toggles are saved to the global settings file, as Pi does | NyPi's RPC claims Pi's command records, but a Pi client could not turn retry or compaction off, stop a retry's wait, change the queue modes (which `get_state` reported as fixed), or read session statistics. Saving toggles follows Pi's settings manager, so a choice survives a restart; `save_global` rewrites only the key it sets. `abort_retry` acts only between a retry's start and the retried run, as Pi's does, so it never aborts a request in flight. Queue modes became shared and changeable, and are read from Pi's `steeringMode` and `followUpMode` settings. The tree commands (`fork`, `clone`, `get_tree`), user `bash`, and HTML export are larger and wait for a client that needs them. | A client needs the tree commands, user bash, or HTML export |
| D49 | Pi's session-tree commands over RPC: entries, tree, fork candidates, fork, clone, and session names, with Pi's branching rules | The tree format came from Pi, but nothing could read or branch it: a Pi client could not show the tree, fork from an earlier message, or name a session. Forking writes a new file holding only the chosen path, as Pi does, so the old session stays intact and the new one names it as its parent; labels on the path are re-created at its end, and entries after a dropped label are re-chained past it, compaction's first kept entry included. Building the tree recursively, and writing it with serde's recursive serializer, would overflow a thread's stack on a long unbranched session, so it is built from the leaves up and written on a large-stack thread. | A front end other than RPC needs branching (the TUI's `/tree`), or sessions grow past what 256 MB of stack writes |
| D50 | The user's own shell commands run over RPC as in Pi (`bash`, `abort_bash`) and are recorded as Pi's `bashExecution` messages; session entries NyPi cannot parse are kept, and Pi's non-provider messages reach context as Pi converts them | A Pi client lets the user run `!command`, with the output joining the conversation, or `!!command` to keep it out; NyPi had no such command. Recording it as Pi's own message keeps the file readable by Pi, and the same projection makes Pi's sessions read correctly in NyPi: before, a line NyPi could not parse (a `bashExecution`, a `custom` message) was dropped, which also cut the entries after it off the branch. The command is the user's, not the agent's, so the Inspector's guard does not apply, but the audit log records it, and it shares the bash tool's process handling (`capture`): process-group kill, truncation, and the spill file. Its output is cleaned as Pi's `bash-executor` cleans it (escape sequences, binary control characters, carriage returns); the agent's `bash` tool is left as is, as in Pi, since programs writing to a pipe rarely add color. It runs inside a loop that keeps reading commands, so `abort_bash` can reach it; other commands wait. | The bash output should stream (Pi's `bash_execution_update`); `!` commands in the terminal and TUI followed in D54 |
| D51 | Tool paths are resolved with Pi's rules, including the forms macOS stores file names in, and `edit` accepts edits sent as a JSON string (Pi parity) | On macOS a screenshot name has a narrow no-break space before "AM", accented names are stored decomposed, and some names carry a curly apostrophe, so a path the user or the model types plainly did not open; models also copy `@file` mentions and Unicode spaces into paths, and some send `edits` as a string. Pi handles all of these; NyPi failed the call. NFD needs the `unicode-normalization` crate, the standard implementation and a small one. The fallbacks apply only when nothing exists at the path as given, so no existing name is shadowed. | Another platform stores names in a form a typed path misses |
| D52 | Models cycle as in Pi: through `enabledModels` (Pi's patterns: `provider/id`, a bare id, or a glob, each with an optional `:level`) when it names any model, else through every model with a key; RPC `cycle_model` | The last small gap in Pi's RPC. The scope keeps the user's order, takes each model once, and skips models without a key; a current model outside the scope counts as its first, as in Pi; a pattern that matches nothing raises a warning, and patterns that match nothing at all leave every usable model to cycle, as in Pi. A cycle records one model change and at most one thinking change, through the same switch `set_model` uses. Pi's `--models` flag and its choice of a new session's model from the scope followed in D54. | A front end wants a key for cycling |
| D53 | A session exports as a static HTML page rendered in Rust (`export_html`, `/export`), not with Pi's browser-side template | Pi's export is a page its own JavaScript renders, with about 300 KB of template and vendored libraries; carrying it would put non-Rust code in a repository that holds only the Rust workspace (D18), against the user's preference for compiled code. The user chose a static page instead: the current branch, Markdown rendered with `pulldown-cmark`, tool calls folded with their results under them, and messages a context edit left out marked. The page shows model and tool output, so it cannot run script: raw HTML in Markdown is shown as text, links keep only web, mail, and in-page targets, images must be base64 of an image type, and a Content-Security-Policy forbids script outright. Lost against Pi: the tree sidebar, search, filters, and syntax highlighting. | Exports need the whole tree or search |
| D54 | `!command` and `!!command` work in the terminal, the TUI, and terminal chat mode, as in Pi's TUI, but not over Telegram; the worker's abort reaches a running job; Pi's `--models` flag sets the scope and a new session's model | Pi's TUI runs the user's shell commands; NyPi had them only over RPC (D50). They go through the shared command path, so every terminal front end gets them at once. Telegram is left out on purpose: running host commands from a chat app is a different exposure, and its owner has not asked for it. A command runs as a worker job in the TUI and chat mode, and the worker's abort reached only tasks, so `/abort` could not stop a long command (or a manual compaction); it now reaches jobs too. In the plain terminal the command's process group does not get Ctrl-C, so Ctrl-C aborts it. `--models` replaces `enabledModels` for the run, and the user's new session without a chosen model starts on the default model when the scope holds it, else on the scope's first usable model at its level, as in Pi; a specialist or delegated runtime keeps the default model. In chat mode the command runs in the worker's runtime and its output is also handed to the chat module, which answers the next message. | Telegram users want `!` commands, behind approval |
| D55 | (Superseded by D56.) Observability is a read-only web page (`nypi observe`) over the audit log and the task ledger, at a depth the user picks, live over server-sent events, on the LAN behind a token | The user wants to see what the agent did and does, in as much detail as they choose, without the watching affecting it. Everything needed is already recorded by the Inspector and the ledger, so a separate process that only reads those files observes without any hook into the agent: no locks, no writes, no IPC, and a crashed observer changes nothing. Depth is one ordered setting (summary, steps, full, raw) plus filters, because what matters differs between "is it working?" and "why did that call fail?". Live rows are rendered by the server and inserted by a ~100-line script; the user chose that over reloading pages. The user chose LAN access for their phone: a 256-bit token in a link becomes an HttpOnly, SameSite cookie and leaves the address bar; HTTP is plain, so trusted networks only. Generated text in flight is not shown, since showing it would need the agent to publish its stream. | The page needs the reply in flight, a remote network, or history beyond what scanning the log per request handles |
| D56 | The owner watches and steers the agent from a board (`nypi board`): a long-lived service that every runtime links to over `run/board.sock`, joined with the stores on disk, served to a Rust/WebAssembly page with approve, deny, stop, and message | The read-only viewer of D55 answered "what is in the log", not the owner's questions: does anything need me, what is she doing now and is it going well, how did things end, why. Those need what only a running process knows (the current step, the reply being written, a waiting approval, the context gauge) and the power to act where one looks. A link per runtime publishes that without interfering: it exists only while a board listens, never blocks (bounded queue, drops counted), and sends reply text only while someone watches. Approvals became first-answer-wins across the front end and the board, so neither waits on the other, and actions reuse the abort and steering handles. The page is compiled Rust (Leptos) sharing its wire types with the service, chosen by the owner over server-rendered pages with more JavaScript; it is embedded in the binary. Same LAN token as D55, plus CSRF for actions. | Several owners, a remote network, or actions that need new mechanisms (resume and cancel from the board, editing goals and schedules) |
| D57 | Notices for the user go through a durable outbox: recorded before they are sent, open until a channel takes them or a front end shows them, sent again with backoff, and shown as "While you were away" when a front end opens; a desktop notification is the default channel | The audit found four ways a finding died on the way to the user: no channel configured (the default), a failed send dropped without retry, `nypi watchdog` printing to nobody after it had already marked the task, and a process exiting before it sent. All four are one defect: the notice existed only in the memory of the process that raised it. Writing it first makes delivery a state that can be checked and repeated (B6, A3), and showing open notices at the next start means being away never means not being told, even with every channel broken. The desktop default exists because a guarantee that needs configuration is not one; it is local, and `[]` turns it off. | Notices need per-channel routing or acknowledgement, or the board becomes the place they are read (phase 4) |
| D58 | A run nobody watches always reports: the watchdog, the scheduler, and the DMN start their runs with `--unattended`, whose result goes to the outbox however short the run, and whose failure to start or finish is posted before the process exits; stall notices repeat as the quiet doubles | A background resume that finished in under `notify.afterSeconds` reported nothing, although the user had just been told it was "resuming in the background"; one that failed in `Runtime::new` exited before any send, and a failed DMN pass was silent by construction. The threshold exists because a user at the terminal sees the answer; an unattended run has no such reader, so it never applies. One stall notice per quiet period meant a run that hung at 17:05 said so once and then nothing for the night. | Unattended runs become frequent enough that every result is noise (then: a digest) |
| D59 | `Continuing` stands only with a schedule, set during the run, that brings the agent back; otherwise the runtime tells the agent to go on, up to `maxContinuations` times, and then reports Blocked. Such a task is `Waiting` in the ledger, judged by its schedules. Revises D23's rule for a Continuing that never continued | The owner's worst case is work that stops with nobody told. A turn-based agent that ends its turn saying "Continuing" has stopped: no code ran afterwards, and the watchdog only alerted once and never resumed, because a resume would have copied the old status. Asking for a concrete way back turns the word into something the harness can check: a schedule the scheduler will fire (with its own at-most-once record and report), or more work now, or an honest Blocked. Tying it to schedules reuses a mechanism that already survives restarts, instead of a second timer in the ledger. Bounding the nudges keeps a model that only repeats itself from looping, and Blocked is the right end because the user is the only one left who can move it. | Agents get a way to wait on a running process or a child task directly, or the nudges prove too few or too many |
| D60 | A supervisor process (`nypi supervisor`, installable as a LaunchAgent) runs the watchdog, the scheduler, and the outbox retries when no front end is open. Answers D23's and D36's revisit conditions | D23 and D36 put the rounds inside whichever front end was running, so closing the terminal stopped all of it: a dead task was found, a schedule fired, and a DMN pass ran only the next time the user opened NyPi, which is the silent stop B6 forbids. The supervisor has no model, session, or tools, so it has little to go wrong and nothing to spend; sharing the existing locks lets it run beside front ends without double reports. A long-lived process rather than a launchd timer keeps the scheduler's 30 s cadence and condition bookkeeping; launchd restarts it, and its heartbeat file makes a hung one visible. Installing is a command, not a default, because a background service is the user's decision. | Linux needs the same (a systemd user unit), or the board's service takes over the rounds |
| D61 | A folder's own NyPi configuration loads only from folders the user has trusted, with Pi's trust store (path to decision, nearest ancestor wins); a terminal asks once, everything else leaves it out and says so | The evaluation showed one `write` to `.nypi/settings.json` turning the approval gate off and adding a shell as a tool for the next run, and a cloned repository could ship the same file. Pi gates exactly this; NyPi's analysis promised it for M2 and never built it. The question belongs to a person at a terminal, so unattended starts default to not loading, which fails safe: the run works with the user's own settings and reports what it ignored. | Users want a default (always or never) or per-resource trust |
| D62 | What the agent's tools may not touch is defined once (`Protected`: all of NyPi's home, any `.nypi` folder under the user's home, the supervisor's service file; secrets unreadable) and enforced twice: by the guard, exactly, for tools that name a path, and by the operating system's sandbox for every subprocess, with a text check only where no sandbox exists | Requirement 5 says the agent cannot modify the Inspector. The guard matched a `bash` command against one spelling of each path, so `rm -f ~/.nypi/audit/*.jsonl` passed, and it resolved `write` paths differently from the tool, so a leading space passed too. No reading of command text can be made sound: a shell has variables, `cd`, scripts, and interpreters. The kernel checks the file a call reaches, whatever the command looks like; the live test runs nine such spellings and all fail. Protecting the whole home instead of a list of files removes the question of which new file someone forgot (settings, trust, specialists, and the outbox had all been missed or would have been). Allow-everything-else keeps the sandbox from breaking ordinary work. | A sandbox for Linux (Landlock or bubblewrap), sandboxing MCP servers, or scrubbing inherited API keys from subprocess environments |
| D63 | Effectiveness is measured from NyPi's own records plus the user's verdict on each result: a `rated` ledger event (good, fixed, wrong, silent; `nypi rate`, `/rate`), and `nypi scorecard`, which judges a week against bars (zero silent stops; shares right without help, Done that was wrong, useful self-started work) and refuses to judge a share with too few verdicts | The evaluation of 2026-11-10 could not say whether NyPi is a good assistant: its ledger held eleven smoke tests, and every "Done" in it is the agent's own claim. The user's bar is their own experience (did it do the thing, did I have to push, did it go quiet), so that judgement has to be captured where the work is recorded and cost one key, or it will not be given. Computing the rest from the ledger, outbox, and audit log needs no new bookkeeping and cannot drift from what happened. Judging only rated tasks keeps unrated work from counting as success; a minimum sample keeps three good results from reading as a pass. The agent cannot rate itself, or the measure would be worth nothing. | Verdicts are too sparse to judge anything (then ask in the report itself, with a reply or a button), or the scorecard needs what is not recorded today: cost per task, memory recalled at the wrong moment, time to first response |
| D64 | Lucia runs as a daemon (`nypi daemon`): one always-on process holding the runtime, taking messages at any time on an owner-only Unix socket in the RPC protocol, with any number of clients (`nypi send`, `nypi attach`). The supervisor's first job becomes keeping it alive: start it, restart it at once when it exits, kill and restart it when its heartbeat stops, report a crash loop. Tools cannot reach the socket (sandbox, guard, peer check). Revises D23, D36, and D60, which avoided an agent service | The user's decision of 2026-11-11: the program itself should run all the time, ready for a message, and the supervisor is only there to see that it does not stay down. D60 had built the reverse, a watchdog with no agent behind it, so "always on" meant schedules fired while Lucia existed only for the length of a run, and reaching her needed an open terminal or the separate Telegram bridge. One living process is also what the original concept assumes: pulses, idle-time reflection, and a chat module that answers while work goes on all need something that is there between messages. Reusing the RPC loop gives the daemon a tested protocol with steering, approvals, and commands; broadcasting its records makes every client a view of the same work; and a command stream that never closes is what lets a run outlive the client that started it. The supervisor keeps its rounds because they must not depend on the daemon being well. Because a connection carries the user's authority (it can answer an approval), the boundary of D62 is extended to the run folder's sockets. | The Telegram bridge, plain `nypi`, and fired schedules are moved onto the daemon (one conversation everywhere); several tasks must run at once (A6); or a stuck run inside a live daemon needs a restart the heartbeat cannot see |
| D65 | The board has a System screen: the whole program as a map of modules in five regions, with their connections, each module's state read live, and the connections work is going over shown moving. The modules and connections are a hand-written list in the board service, held to the source tree by a test; the page (Rust, SVG) decides where to draw them | The owner asked to see the complete structure of the program at a glance. Until now the structure existed only in this document and in the code, and the board showed work, not the machine doing it; after the daemon (D64) there are enough moving parts (a supervisor, a daemon, front ends, modules that are on or off by setting) that "what is running, and what is it connected to" needs a picture. The structure cannot be discovered at run time (a module that is turned off publishes nothing, and most connections are function calls), so it is written down once, where the board can serve it, and a test fails when a listed file is gone, when a core source file is on neither the map nor the list of parts, or when a connection names a module that does not exist. State is never written down: it comes from the hub, the heartbeats, the settings, the stores, and the audit log at the moment of asking. The service says what exists and the page where to draw it, so a newer service's module still appears on an older page. Looking at the first live map found two bugs, fixed with it: a process that renamed its front end after linking never told the board, and the user's home directory, where the daemon works, was treated as an untrusted project because NyPi's own folder is in it. | Modules become something a process can report (then derive the map from the processes), or the map needs to show more than one runtime side by side (a terminal's Lucia and the daemon's as separate boxes) |
| D66 | NyPi across terminals and devices is one hub with a node on every device (design in docs/devices.md): the hub (the daemon) runs every agent loop and is the only writer of all state; a node per device (`nypi node` on a computer, a native app on a phone) carries out the hub's calls under that device's own Inspector and declares which tools it has; interfaces (terminals, the phone app, the board's page) and channels (Telegram, inside the hub) only talk and watch; the board stays a service of its own. Lucia's loop runs in the hub, not on the device; autonomous work beside the files is a delegated specialist. Planned, in seven steps | The owner will use NyPi from many terminals and devices at once, phones included, and it must be the same entity. Being one entity means some things have one decider: a reminder fires once, a message gets one reply, the self-prompt and the ledger have one current version. Equal peers that all think and act would need to elect a leader for exactly those things, which is this design plus elections and merging; without it they are several Lucias sharing a diary. The loop stays in the hub because devices go away (a laptop sleeps, a phone is in a pocket) and the thread must not go with them, because everything a turn needs (prompt layers, recall, session, ledger, steering, approvals) is there, and because a call over a private link costs tens of milliseconds against a model call of seconds. Nodes are thin but not dumb: each enforces its own guard, keeps running a started call when the link drops, and never carries out a call twice. Because a phone offers apps and sensors where a computer offers files and a shell, a node declares its tools instead of implementing a fixed list, and the node is a small library that builds for phones. On Android the node can operate other apps through an accessibility service; on an iPhone Apple allows no app that, so there it offers Shortcuts and the system's own data, and other apps are operated from a Mac. Model keys stay on the hub, so a lost device leaks none. The board is separate because it must show that the hub is down and because it listens on the network. The owner chose the board as an independent service, agreed to the central hub and to the loop in the hub, asked for the node to run natively on phones, and asked for plain names for the parts (hub, node, interface) on 2026-11-17. | Full capability is needed with the hub unreachable, no machine can stay on, several people share one NyPi, or links are slow enough that calls dominate a turn |
| D67 | After a review of the whole code against the owner's original brief, the code is reshaped in six parts, in this order: (1) a daemon thread is the whole Lucia: chat, worker, thinking, and recall are assembled for the thread, not by a front end; (2) one front-end path: every interface speaks the daemon's protocol, and a terminal without a daemon runs the same hub in its own process, so the per-front-end run loops go; (3) one pulse bus in the hub, with the scheduler, the watchdog, and idle passes as modules that publish pulses, and no helper processes or lock files; (4) delegation in the background, a specialist's completion coming back as a pulse; (5) crates that match D66: a hub crate, a node crate and its wire types, tools behind the place they run at, the runtime split by responsibility; (6) the hub holds the folded state of the stores it alone writes. All six parts are built (part 3 keeps the two lock files, since two hubs can run at once; part 5's node is the one in the hub's own process; part 6 follows the files, since the hub is not yet their only writer) | The review found the lower half of the system meets the brief (kernel, the never-silent chain, the Inspector, memory, portrait) and the upper half does not, for a structural reason: the code grew front end by front end before there was a daemon. The brief's first concept, "chat is chat, thinking is thinking", existed, but only three front ends started those modules (chat mode, the full-screen UI, the old Telegram bridge), and the daemon, now the main way to use NyPi, started neither; in the owner's own settings chat, thinking, and idle passes were all off, so Lucia was one worker loop. Six front ends each drove a run loop and re-implemented approvals, steering, stall notices, and drawing (about 4,100 lines). Pulses were recorded, not followed: one bus per conversation, and schedules, idle passes, and resumes as separate processes behind lock files, where the brief asks for a flow driven by pulses with the DMN inside. Delegation blocked the conversation it came from, against "start many at once". The services lived in the command-line crate, where a phone's node cannot reuse them, and the runtime was one 2,188-line object. Part 1 comes first because it is the brief's first concept and the gap the owner would meet the moment they used the daemon. | A part turns out to cost more than it returns once the parts before it are in, or daily use shows a different gap matters more |
| D68 | A node on another computer is built before pairing: `nypi node` is a process that links to a Unix socket of the hub carrying the node's protocol only, a second computer forwards that socket over SSH, and a thread's place can be a folder on such a device, with the device's own sandbox, protected paths, and guard as the node's last word | It delivers "works where the thing is" on a second computer now, with no cryptography of NyPi's own and no port opened on the network: SSH already gives encryption and proof of both ends, and the owner already has its keys. The node's protocol, the register, and remote places are the same whatever carries the link, so pairing and a listener can replace the forwarded socket without changing them. Opening a network listener and choosing how devices are paired is the owner's decision (docs/devices.md sections 4 and 15) and was not taken for them | The owner settles pairing and the transport; or a phone's node, which cannot forward a socket over SSH, comes next |
| D69 | Paths are followed as the kernel follows them, and the file tools check the file they open: `real_path` resolves symlinks one component at a time (a link's `..` is the parent of its target; a dangling link is its target), `read`'s name fallbacks are checked as `read` resolves them, `read`/`write`/`edit` hold `Protected` and open the resolved path, and the user's own skill folders are protected | A review of the code (docs/plans/optimizations.md, part 1.1) found three ways past D62's guard with a link the sandboxed shell may make inside the workspace: `escape/../settings.json` (the guard removed `escape` by spelling, the kernel followed it into `~/.nypi`), a link to a protected file not yet written (the guard saw a new file in the workspace), and `note's.txt` read through the curly-apostrophe form (the guard checked the straight one). The in-process file tools are outside the sandbox, so the guard was their only check. Resolving per component is what the kernel does; checking at the open closes the gap between check and use. Skills in `~/.agents/skills` load into every prompt without a trust question, so a planted one would steer every later run | A tool opens files by descriptor relative to a checked folder (`openat` with `O_NOFOLLOW`), which would close the remaining race with a link swapped between resolution and open |
| D70 | An approval question has one id, the Inspector's, everywhere, and the approver hears when its wait ends (`Approval::Asked`, `Approval::Closed`), whoever answered | The review (docs/plans/optimizations.md, part 1.2) found two ids per question: the board answered under the Inspector's, while the loops numbered their own (`approval-1`) and learned of nothing the board decided. A board answer let the call run, but every terminal kept the question open (the full-screen UI took the user's next Enter as a denial of it), the hub replayed it to new interfaces and escalated it as unanswered, and a terminal's own hub with no Telegram denied it at once although the board was showing it. One id and a closing notice make the board an interface like the others | The board becomes an interface on the hub's stream (devices.md section 11), when its own listener can go |
| D71 | A thread's place is typed (`PlaceKey`), and a task or schedule at another device records that device and folder (`remote`), so whatever takes it up goes back there; the hub's guard leaves a device's paths to the node, and the approval policy uses the device's home | The review (docs/plans/optimizations.md, part 1.3) found the uncommitted D68 work recording only the stand-in folder (`~/.nypi/places/...`) as a remote task's `cwd`: after a restart, a resume or a fired schedule opened a local thread there, inside NyPi's own folder, with this computer's tools, and every later attach at that device joined it. The stand-in names collided (`/a/b` and `/a-b`), the edit tool's fix-ups never ran for a remote call, the policy judged `~/.ssh` by the hub's home, the guard followed a device's paths on the hub's disk, the prompt named the hub's operating system, skills pointed at files the device cannot read, and a dropped SSH tunnel locked the device out for a minute | A device's folder configuration, context files, and schedule conditions are read at the device (the node offers them) |
| D72 | The hub's commands are typed once at the edge (`HubCommand`, `RuntimeCommand`), and both conversation loops use them, the runtime's commands carried out by one function; with chat in front every command of the worker-only loop still works, run on the worker once it is idle; the daemon is split by responsibility | The review (docs/plans/optimizations.md, part 2.1) found the two loops re-implementing dispatch over untyped JSON and drifting: with `chat.model` set about twenty commands were refused (`set_model`, `compact`, `fork`, `bash`, ...), reflection notices were never said, and `/followup` became an immediate message. Turning on the chat module, the brief's first concept, must not take capabilities away. Typing the commands makes the compiler point at a loop that does not handle one; the records going out stay JSON built in one place each, since the interfaces read them as JSON anyway. The 2,000-line daemon file mixed five jobs | The interfaces move to a typed client of their own (a crate of wire types beside `nypi-node-proto`), or a third loop appears |
| D73 | Never-silent and boundary seams closed together: a run that makes no progress for `stuckMinutes` (default 30; a tool's repeated identical output is not progress; a run waiting for an approval is not stuck) is stopped and ends Failed, saying why; calls of one reply are prepared side by side; a folder's trust, skills, and specialists are re-read before each run of an open conversation; a terminal's own hub is told what the other conversations do and degrades on an unknown chat model as the daemon does; `post_json` retries only transport failures; keys are redacted from `Debug`; an empty read rule is left out of the sandbox profile; a peer's ancestry is read with a system call, and the bridge socket refuses the hub's own children too | The review (docs/plans/optimizations.md, parts 2.2 to 2.4 and 3.5) found: the heartbeat beat on a timer whatever the run did, so a hung stream or MCP call kept its conversation busy and everything queued behind it waiting, for good, while the watchdog saw a live task; one call waiting for approval left the reply's read-only calls unpolled after they were announced; `nypi trust yes` reached an open conversation only when it closed (the home conversation never does); standalone and daemon differed for no reason; two retry layers multiplied to sixteen attempts and slept through quota errors unseen; a Seatbelt deny with no filter would deny every read; and each connection could start up to 64 `ps` processes. Not done here: the tokenizer download is not pinned to a hash, because the upstream file's hash and commit cannot be checked from here, and Linux still has no sandbox | Long silent tools (a build with no output for half an hour) are stopped in practice, or Linux becomes a platform NyPi is used on |
| D74 | The node journals each call it takes and each result until the hub acknowledges it; a call outlives its link for a lease and its result is handed over on the next link; the hub waits for it across links; the System map shows the devices. One board link for the whole hub is left for the board's own phase | The review (docs/plans/optimizations.md, parts 3.3 and 3.4) found the node's "at most once" in memory only, and every running call cancelled the moment a link dropped, so a Wi-Fi or SSH flap killed a build and lost its result, against devices.md section 7. The journal is the node's because only the node knows what it carried out; acknowledging lets it forget. One board link per hub needs the board's protocol, service, and page to show threads inside a process, which is a change to the board's phases rather than to the hub; the relabelling the review found (a sourced run renaming its thread) is fixed without it | The board shows threads within a process (board phase 2), or calls longer than the lease are common |
| D75 | Chat's history is kept per worker session and read back when the conversation opens again; a task waiting on several specialists waits for all of them (wakes read typed: schedule or delegation). Left for designs of their own: a conversation that is not bound to one place (the home conversation as Lucia's continuous stream, one task using several devices), and chat speaking up unasked when a task settles | The review (docs/plans/optimizations.md, parts 3.1 and 3.2) found chat's history only in memory, lost at every restart and idle close, and a waiting task released by the first of several specialists' reports. The larger items change what a conversation is (D66 binds a thread to a place) and how often Lucia speaks unasked (D19 shows the worker's report as the artifact of record); each needs a design that weighs noise, cost, and the devices.md section 5 shape, not a change made in passing | The owner wants one conversation across places, or chat's silence after a task is felt as a gap |
| D76 | The ground the V2 redesign (docs/plans/v2-plan.md, phase 0) stands on: tool subprocesses, external specialists, and stdio MCP servers start without the keys NyPi was started with, unless the owner names one (`sandbox.allowEnv`, `passEnv`, `env`); Linux gets the tool sandbox through bubblewrap, with the guard still reading command text there; the core and chat prompts gain a fixed `<partnership>` block (intent over literalism, mode calibration, inquiry before escalation with the human-only boundaries named, voice) | D62 left three gaps open: inherited API keys reached every `bash` call (`env` printed the model's key), Linux had no sandbox, and both were listed as remaining. A deputy that is to hold credentials and run unattended (phases 1 to 5) must not hand its own keys to whatever it runs. A deny-list of names is used, not an allow-list of variables, because builds depend on variables nobody can enumerate, while keys follow a few naming shapes; a key under another name stays the owner's to keep out of the shell. Bubblewrap is on most distributions and needs no privilege, where Landlock would need new code per kernel version; it cannot refuse what does not exist yet, so the text check stays on beside it. The conduct rules are in the read-only prompt because vision.md section 5 makes them the owner's terms, like the conduct principles | A key appears under a name the shapes miss; or bubblewrap is unavailable on a system NyPi is used on, and Landlock is the way |
| D77 | What NyPi keeps is a crate of its own below the runtime, `nypi-vault`, in three tiers: episodic memory (moved there with the JSONL folds), Lucia's own Markdown vault with a heading-chunk and `[[link]]` index searched by meaning, words, and one hop of links, and an asset registry a model may read beside a sealed secret store it may not: secrets are named by `cred://` handles, used by the harness in `http_request` under the Inspector's approval, and scrubbed from everything that comes back | vision.md section 4: a multi-year deputy needs more than a flat log of sentences. A project, a server, or a procedure has structure, history, and relations, which Markdown with links holds and a person can read and correct; flat memories cannot. The vault is hers and inside NyPi's home, so the sandbox that keeps tools out of `~/.nypi` protects it for free and she writes it only through tools that check every path. Assets and secrets are split because she must be able to read what she manages and when it renews, while a key in a prompt is a key in every log and every provider's hands; a handle can be shown, traced, and approved per host. Scrubbing is by value, over all stored secrets, because a server can echo a credential back in a body or a header. No database: the files stay inspectable and the index rebuildable | The vault outgrows an in-memory index (tens of thousands of notes); a provider needs a signing scheme a header cannot carry (SigV4, OAuth refresh); or secrets must be shared with a device's node |
| D78 | Persistence is a matter of roles, not of counters: the chat module is the principal that gives a task as a brief (`done_when`, `bounds`) and reviews its report before the user hears of it (`continue_task`, at most `tasks.maxReviewRounds`); the Thinking module also observes a run's course from `turnCompleted` pulses and steers it rarely; a task pool runs up to `tasks.maxConcurrent` tasks, each with a vault workspace whose `brief.md` and `findings.md` are in every turn's prompt; `nypi-eval` measures the result on twelve tasks with a hurdle each | vision.md section 6 and the reviews behind docs/plans/v2-plan.md: the agent stopped as Blocked where a way existed, reported Done on a specialist's word, and walked the same dead end again after a compaction. Rules of the kind "retry three times" or "a `.csv` needs a parser" fix the case they were written for and fail the next; a person keeps at a task because someone asked for a result and will look at it, because they keep notes, and because a colleague says "you tried that already". D19 (the report verbatim, chat silent) is superseded for tasks chat started: the report still exists verbatim in the record and on the board, but what the user hears has been checked against the brief | Error counters and file-type gates in the runtime (the plan forbids them, and they are what made earlier agents brittle); a separate reviewer model (a third model, a third prompt, and nobody accountable to the user); an isolated runtime for every task including the first (a follow-up question would no longer find what the task read); findings kept in the session only (compaction is exactly what loses them) |
| D79 | The hub holds one conversation, the primary stream, instead of one per folder: every interface attaches to it and brings its ambient focus (a folder, and the device it is on), and a run works where the focus of its message is. The runtime's place is routed for that: a folder here or on a device per run, an optional `device` on each file and shell call, `on_device` for what only one device offers, and the Inspector judging each such call with a guard and policy seated at its target. `<active_tasks>` and `<devices>` replace `elsewhere`; a waiting task names what it waits on (`AllOf`, `AnyOf`, `Schedule`, `Approval`); `nypi-hub-proto` holds the wire types | vision.md and D75's open item: the owner has one deputy, not one per folder. With a thread per folder, what was said in one terminal was unknown in the next, Telegram talked to a different conversation than the terminal did, and a task could not use a second device. The node protocol already carried a folder with every call (D66), so "where" never had to be a property of the conversation: it is a property of the message | A principal conversation in front of per-folder worker threads whose records are relayed (two kinds of conversation, and every interface has to tell lanes apart); switching the runtime's settings per focus (a project's settings can turn approvals off: applying them mid-conversation by where the last message came from is a way to lower the guard by walking into a folder); the hub's own machine as a named remote device everywhere (every record and test would name a device for the common case) |
| D80 | Devices on the owner's network are paired once and held to what they were paired as: each has an Ed25519 key, links are TLS 1.3 with a self-made certificate on both sides and the hub's key pinned by its fingerprint, a pairing code works once for ten minutes, and the hub records each key as a node or as an interface and routes its links by that alone. A device's folder context is read through its node, a schedule's command condition runs on the device it was set at, every runtime of a process shares one board link, and the board shows the task tree, the devices, and the vault's last notes | devices.md section 4 asked for it from the start, and D79 made it matter: once one conversation spans devices, a node on a laptop is one compromise away from speaking as the owner unless its link cannot carry a command at all. Authority by role at the transport is checkable in one place. A tailnet already gives the devices addresses, encryption in transit, and NAT traversal, so the hub needs one port and no infrastructure | One key per device with the role sent in the hello (the role is then a claim); an authority and signed device certificates (a second secret to guard, for a handful of devices one person owns); relays, STUN, or mDNS (the tailnet is the network); streaming the task tree and the registers over the board link (the board reads the same files the hub writes, and a second copy of that state can only disagree) |
| D81 | Things reach Lucia through channels behind one trait (`name`, `run`, `deliver_notice`, `ask_approval`): Telegram, and an inbound webhook that takes notes, links, and files from the owner's other programs with a bearer secret from the secret store, saves them to the vault's inbox as they came, and gives the conversation the task of filing them as material that is nobody's instruction. Idle passes keep the vault in order within their existing budget: the harness archives the workspaces of tasks settled for `vault.archiveAfterDays`, and the pass files the inbox, distils memory into entity pages, and drafts playbooks from lessons that recurred | vision.md section 4: a deputy's memory is only worth having if things get into it without ceremony and do not rot there. Sharing an article from a phone must be one tap, and the note must exist even if every later step fails, so it is saved before anything else. An inbox nobody empties and workspaces nobody puts away are the usual end of a notes system; idle time is the moment that costs the owner nothing. What comes in from outside is the classic way an agent is told what to do by a stranger, so it enters as a file and a sourced prompt, never as the owner's words | A channel that runs the model itself (a second place where prompts are built and rules apply); the webhook's secret in settings.json (a file the agent reads); fetching links inside the channel (network calls outside the Inspector, and a slow page would hold the request); archiving by a folder's age alone (a workspace of a task that waits on the owner for a month would vanish); a consolidator with a budget of its own (a second meter beside the DMN's) |
| D82 | The user comes first in the chat module: a task is handed over as a one-line topic with the conversation attached verbatim by the harness, its worker sets what counts as done in the workspace's brief and the review reads it there; chat may look up Lucia's notes, memory, assets, schedules, and goals itself, read-only; a message joins the turn chat is giving, messages in a row are one turn, and one that arrives during an unseen review or thought stops it and is answered first; a runtime beside the conversation connects MCP servers in its own task, and the conversation's own worker begins a fresh session for a task at another place | Principle A5 (the user's instructions come first and get an immediate response) and R1 (chat speed): a fast model copying a stack trace into an `instruction` costs seconds and drops detail; a two-step task to read Lucia's own note is slower than the question deserves; a review the user cannot see must not keep the user waiting; a task must not wait on an MCP server that is slow to start; and a transcript from one project misleads the next | Asking chat for a better summary (the loss is in retelling at all); a separate call to derive `done_when` before the task starts (a second wait, and the worker knows better after looking); giving chat the worker's tools with an approval on writes (the line between talking and acting would be a setting, not a fact); queuing a message behind an unseen review (the user waits for something they cannot see); a session per task on the conversation's own worker (a follow-up in the same project would lose what the last task read) |
| D83 | The round lock of the scheduler and the watchdog is a POSIX record lock the process notes, not `flock`; unpairing closes a device's open links at once; a trusted folder on a device lends its skills and native specialists to runs there, and a delegation works at the run's focus; `delegate` can start a race whose first success is reported and stops the rest, waited on as `AnyOf`; notices go through the channels the daemon runs | The flaky test was a real defect: a lock inherited by a forked child made a round skip, so a schedule could miss its moment under load. The rest closes what D79 to D81 left open: an unpaired device must stop being able to act on its open link (R5), the device's own project knowledge must reach the work done there (B5), parallel probes need a winner that stops the others (A6), and one way out for notices keeps the channels the single place that reaches the owner | A retry or a sleep before the next round (it would hide the inherited lock, not remove it); closing links only at their next reconnect (an unpaired key keeps acting until then); running a device's external specialist on the hub (a program written on one device run on another); reporting every probe of a race (the conversation would be woken for each and have to choose) |
| D84 | The hub's cognition is three planes: chat is the only voice and reads everything (the stores, the pool, the Principal, and the devices through `read`, `grep`, `find`, `ls`, time-boxed to 1.5 seconds, never asking for approval) and changes nothing, handing what needs doing to a singleton Principal with `signal_principal` after its reply; the Principal, on the strongest model, runs one activation at a time by priority (owner, event, idle), suspends a lower one for the owner's and resumes it told what was decided, holds the stores and the task pool, and reaches the owner only through `stimulate_chat`; every chat turn, activation step, and decision is logged for the board, scrubbed of secrets | Principles A5 (the owner first) and R1 (chat speed) against the capability inversion of D78/D82: reviewing frontier-model reports and writing briefs need the strongest model, chat needs the fastest; two modules speaking to the owner split her voice; and what chat looked up and the executive decided was invisible | Keeping the review on chat with a stronger chat model (the reply gets slow); letting the Principal speak to the owner directly (two voices); giving chat write tools behind approvals (the line between talking and acting would be a setting); one Principal per task (no single place that weighs the owner's goals against each other); dropping a preempted activation instead of suspending it (its finished steps would be paid for twice) |
| D85 | Goals are campaigns: a goal names a project folder in the vault whose `plan.md` (objective, workstreams marked open, under way, blocked, done, decisions, linked notes) the Principal keeps; a task and a schedule carry the goal they serve and the activation that started them, and a task for a goal sees its plan above its workspace on every turn; idle passes, timed goal reviews, fired schedules, and webhook material wake the Principal (idle time as `P2` within the DMN's budget, recorded as passes, the rest as `P1`), and idle time is spent by two clocks, deadlines with what blocks them and the goals left longest with their next item | Principles B4 (goals advanced without being asked) and B6 (nothing dropped), against the flat horizon of D78–D84: a goal was one line and a task a few minutes, with nothing tying a week's tasks to the goal they serve; idle passes ran as a worker task of their own and could not weigh the goals the Principal holds; a goal with no deadline was never picked while urgent ones lasted | A plan in `goals.md` itself (the user's file stays one line per goal); a `goal` on every task (an errand would carry a campaign's weight); a third tier of sub-goals (the plan's items are enough until shown otherwise); deadline order alone (a goal without a deadline starves); posting the idle note directly as before (a second voice besides chat) |
| D86 | The board is a glass box over the three planes: it shows chat's every turn (lookups with their time and timeouts, signals linked to the activations they became, stimuli and skipped ones), the Principal's stack and queue and every activation step by step (with suspension and resumption where they fell), each task's goal, activation, plan, workspace, steers, and verdicts, the goals and the vault, and the planes on the System map; it reads all of it from what the planes keep, plus a status file the Principal rewrites, and scrubs every answer of secrets | Principle A1 (the owner sees what is done in their name) and the owner's own priority on observability: after D84 the decisions moved to a module nobody could see, and chat's lookups and hand-overs were invisible; a board in a process of its own can only show what is written down, so the planes write down every step, and the board joins it | Streaming the Principal's and chat's text as it is written over the board link (they run in the daemon, whose runtime link reports the worker; a second link per plane is a later step); keeping the status only in memory (the board is another process); trusting the logs' own scrubbing for every answer (a task's story is read from the audit log) |
| D87 | A settled task is reviewed by its dossier: the worker's `report.md` (contract check, findings and deliverables with their `[tN]`, falsified paths and residual risks), asked for once when missing, beside a manifest the harness draws from the task's session (each cited call resolved, and whether anything checked the last file change), with `inspect_task_evidence` for any step in full; every owner's, review, or schedule activation must dispose of what woke it (a task, a store change, chat told, or `conclude_activation` with a reason) or is nudged once; settlements that go together are reviewed together (`batch_unless_failed`, and the queue coalesced); the Principal's system prompt is cache-stable with the portrait, the situation opening each activation; a resumed task regains its workspace and campaign; the planes' scrubber follows the secret store | Principles A4 (traceability) and B6 (nothing dropped), and the D84–D86 review: a review read the worker's own words only, a request could end with nothing done and nothing said, sibling tasks were reviewed one by one, per-second counters in the system prompt defeated the prompt cache, a crash-resumed task lost its plan and notes, and a secret set while the daemon ran reached its logs | Trusting the reply's citations footer alone (it says a reference exists, not what it showed); a file-type gate or a counter that forces a report (D78 rules those out; one nudge is a request, not a gate); a hard failure instead of the nudge (the activation's work would be lost); polling the secret store on a timer (a minute's leak, and a keychain prompt) |
| D88 | An activation's context is four rings: the cache-stable system prompt; a situational radar of goals on their clocks, the execution board, and recent decisions; a briefing pack looked up by what woke it, by words and without waiting (the matching plan whole, matching chunks of entity and playbook notes with their one-hop links, memories, asset handles; for a review the findings, plan, and last turns; for idle time the digest, inbox, and budget); and its own transcript with the resumption delta. Chat and the Principal report their step and the text being written to the board while in flight, and the board shows the task's dossier and the activation's pack and disposition | Principles R1 and A1, and the D86 gap: the Principal spent its first turns searching for what the harness could hand it, and the board showed the planes only after each turn ended | Hydrating with embeddings (a cold model or a slow embedding service would hold up the owner's activation); inlining whole entity notes (noise past the matching section); a second board connection per plane (one process, one link, as D80 decided); writing a progress file several times a second for the board to poll |
| D89 | The three planes' prompts carry a working method (check before asking with parallel read-only tasks, inspect before changing, parallel independent work, known context passed into briefs, free movement on reversible work and a clean stop before the user's four boundaries, snags worked around, lessons written down) and call the person "the user" throughout, with `{{user_ref}}` in the Principal's prompt too; inbound material must be disposed of; dossiers are scrubbed whole; the Chat screen opens on a turn in flight | Principles B1, B3, B5, and A5: the D84–D88 prompts said what each plane may do but not how to work, so a model asked the user what one background check would answer, or edited before looking; "owner" in prompts and harness strings was a second word for the user | Leaving the method to the self prompt (it is the user's to edit and does not reach the Principal's rules); asking the user first on any ambiguity (their attention is the scarcest resource); keeping "owner" in harness strings only (the models read both and mix them) |
| D90 | The benchmark runs through the three planes as well as on a worker alone: the task is the user's message to chat, the run ends when every plane is quiet, the same checks judge it, each result counts chat turns, signals, activations, tasks, and send-backs, and four scenarios test the working method (checking before asking, verifying after editing, parallel checks, a lookup that stays in chat) | Principle A4 applied to the redesign itself: D84–D89 changed who decides and how, and `nypi eval` still measured one worker, so none of it could be compared against the worker alone or across models | A separate benchmark for the planes (two sets of checks would drift); judging the run by the Principal's decision log (the outcome on disk is the judge, as in D78); ending a run at the first reply (the planes' work goes on after it) |
| D91 | Five clean-ups after D90: the Thinking module's switch is `thinking.enabled` alone, and its `Thought` keeps only what the observer publishes; a trusted device folder's settings apply to runs there through an allowlist (thinking level clamped to the model, compaction, retries, and approval tools added to the hub's), with the hub's values put back when the run works elsewhere, and nothing that could weaken the hub (approval mode, env files, MCP servers, sandbox, channels, hub, board, command tools) is read; Telegram is a channel under `channels/telegram/` whose bot token may be a secret-store handle (`channels.telegram.botTokenSecret`), with `TELEGRAM_BOT_TOKEN` as the fallback; every string a model or the board shows says "the user"; `tokenizer.json`'s SHA-256 is pinned; `principal.rs` is split into `principal/` (mod, journal, runner, tools) | Principles R5 and A2, and D80's rule that a device is what its key was paired as: a device folder's settings were read and dropped, so a folder could not set its own retries or ask for approvals, while applying them whole would have let a device turn the hub's approvals off; a bot token in an env file was the one credential outside the secret store; the tokenizer was the one download without a checksum; one 2,200-line file held the Principal's every part | Applying a device folder's settings whole (a device could weaken the hub); ignoring them for good (a project could not ask for approval of its own risky tools); dropping `TELEGRAM_BOT_TOKEN` at once (existing setups would break silently) |
| D92 | The documents describe the system as it is: architecture.md's body (§1–§4) is rewritten to the live design, with superseded mechanisms (threads per folder, `elsewhere`, the Thinking module's look at each message, per-front-end run loops, chat as the reviewer, the Telegram bridge) left to this log, which stays whole; the finished stage plans move to `docs/plans/`; devices.md, board.md, usage.md, roadmap.md, and AGENTS.md drop their stale notes and say "the user"; the roadmap ends with its open items; `docs/overview.md`, a summary in Chinese, is checked against the code and kept | Principle A2 (what Lucia does should be simple to understand), for the people reading the documents as much as for her reports: a body that told each design and then the patches that superseded it made the current system hard to see, and made it easy to rebuild something already retired | Keeping the history in the body under "superseded" notes (the present stays buried); deleting the stage plans (they are the record of why each step was taken); a separate "current state" document beside an unchanged architecture.md (two descriptions that would drift apart) |
| D93 | Two instances and one supervisor: a machine is the hub or a node, as `instance.json` says, and one supervisor service (a LaunchAgent, or a systemd user unit on Linux) runs whichever it is; one key and one admission per machine, with `personal` or `exec-only` trust in place of the node and interface roles; a link names its purpose (`node`, `talk`, `join`, `leave`) and `daemon/remote.rs` decides by trust and purpose; joining shows a six-digit code made from both keys and the TLS exporter on both screens, and the user admits it from a place they already trust, within limits that keep requests away from the conversation; a built-in entry; `nypi hub up`, `nypi node up`, `nypi status`, and `nypi devices` replace `nypi pair`, `nypi daemon`, and `supervisor install|uninstall|status` | Principles A5 and R5, and decision D80's rule that a machine is what it was admitted as: pairing asked the user to carry a code from the hub to each device, two keys per device that both runs calls and talks doubled the register, and nothing said what a machine was without reading several files | Keeping the code typed on the device (the user has to be at the hub first, which a laptop away from home is not); trust on first use with no code (anyone at the entry could join); roles per link instead of trust per machine (two keys, two admissions for one computer); a separate service per mode (two units to install and keep in step) |
| D94 | The relay, direct links, and the board at the entry: the hub listens directly (`hub.listen`, 7681), works out its direct addresses (interfaces, a forwarded port, a port the router maps), and announces itself by mDNS; a node probes the direct ways first, takes only the pinned key and the highest epoch, falls back to the entry, and moves a relayed link to a direct one when it can; the relay is a plain HTTP and WebSocket server behind Cloudflare or a reverse proxy, with a signed control connection from the hub, links passed as binary frames around the same pinned TLS, and browsers passed to the board's public socket, where only a browser session the user admitted is taken | Principles A5 and R5: a laptop away from home must reach the hub without a tailnet, and the hub's traffic must not depend on a third party it cannot check; Cloudflare in front of the relay keeps the VPS's address out of DNS and needs no port of its own | Raw TLS on the relay's own port 443 with routing by ALPN and the hub's own ACME certificate (needs a public address and port on the relay, and an ACME client to keep); a WireGuard tailnet (one more system to set up and trust); relaying every link always (all traffic through one VPS); UDP hole punching (left open, below) |
| D95 | Moving the hub, with exactly one hub at a time: every personal node keeps a standby copy of the hub's state (the home without what belongs to one machine, with the secret store's key and the env files), kept current by checked differences, never sent to an exec-only node and paced over the relay; `nypi hub here` stops the hub's work, sends the last difference, makes the old hub a node at the next epoch, and only then lets the target start; `--force` takes over from the copy when the hub is gone; the hub key moves with the hub, so nobody joins again; the relay takes only the highest epoch, nodes refuse a lower one, and a hub that hears of a higher one steps down with its state kept under `diverged/` | Principles A5, B6, and R5: the hub is one machine, and when it is off, away, or broken, the user must be able to carry on from another of their own without losing what Lucia knows, and never with two of her taking work | A shared folder or a cloud copy of `~/.nypi` (two hubs could write it at once, and the secrets would leave the user's machines); a merge of two hubs' state after a split (logs and the vault have no merge that keeps meaning); a lease the hub renews at the relay (the relay would decide when the hub is gone, which a network split gets wrong) |
| D96 | The hub's commands from any personal machine: on a personal node, the commands that work on the hub's stores (`tasks`, `goals`, `memory`, `portrait`, `schedule`, `specialists`, `reflections`, `dmn`, `rate`, `scorecard`, `notices`, `trace`, `eval --list`, `devices`, `secrets`) are carried to the hub as `HubCommand::Admin` on a talk link and run there by the same code, their text and exit code sent back; `secrets set` reads the value on the node and the hub stores it; an allowlist in `protocol.rs` names what may be carried, `daemon/remote.rs` takes it from personal machines only, and an exec-only node refuses before sending | Principle A5 and decision D93's one hub: once the stores live on one machine, a command typed on another must act on them, not on an empty local home; and a secret must reach the store without passing through a file or a chat | Running the commands against a copy of the stores on the node (a standby copy is read-only and behind); a remote shell on the hub (far more than these commands need); asking the user to log into the hub (what a personal machine is for) |
| D97 | A public repository: no personal domain, gateway, address, or credential in any tracked file; the entry is optional (none by default, so a public build works on the local network out of the box), saved by `--entry` in `settings.json` where every later version reads it, or built in with `NYPI_DEFAULT_ENTRY`; a hub follows the entry its settings name without a restart; RFC 2606 examples throughout; the workspace version, repository, and homepage name the public repository; a README of NyPi as it is, a security policy, and ignore rules for keys and backups | Principles A4 and A5: a published build must carry nothing of one user, and every user must be able to reach their own hub from anywhere by saving their entry once | A placeholder domain built in (a public build would dial a name nobody owns); a separate configuration file for the entry (one more file to carry across machines and versions); keeping the personal values behind a build flag (they would still be in the history of every file) |
| D98 | Verified self-update: signed release manifests (Ed25519 over the exact bytes, the public key built in), archives checked by size and SHA-256, a pre-flight `--version`, an atomic swap with `nypi.prev` kept, and the supervisor confirming an update once every service stays up or rolling it back and blocking that version; background checks that tell once per version, or apply on an idle machine (never a relay unless allowed); fleet updates node by node from the hub; CI, a release workflow for three targets with one board, and a POSIX installer | Principles A3, A4, A5, and B6: every machine must take the same fixed release from the public repository without a token, refuse anything not signed and checked before it touches the installation, and never be left down by a release that does not run there | A package manager per system (several to keep, none for every target); updating through the hub (the hub would become a trust anchor for binaries, and a node far away would depend on it); checking only the SHA-256 against `SHA256SUMS` (whoever can change a release can change both); replacing the binary in place (a crash halfway leaves no binary) |
| D99 | Refinements after D98: an update is not in place until nothing of the old binary runs (the supervisor stops every service once, its own children and an earlier supervisor's processes by their recorded pids, and the update's clock starts then), a supervisor started more than three times while an update is pending puts the old binary back before anything else, `--check` and the prompt take `--allow-unsigned`, a failed automatic update is told once, pre-releases ordered as SemVer, and a release key that can be pasted into the source; the release workflow checks the tag against the workspace version, cross-builds aarch64 Linux on x86_64, and lets a dry run go unsigned; the installer says when there is no release; the user's service folders on both systems are protected and the guard sees `nypi` by any path and after global flags; standby batches read only what changed; the relay believes forwarded addresses only from loopback and takes a list's last entry; the board's files are revalidated by `ETag` | Principles A3, A4, A5, and B6: a review of D98 found that `launchctl kickstart` restarts only the supervisor, so an update could be confirmed while the old daemon still ran; that a binary which fails before the supervisor's rounds was never rolled back; that a tool could plant a systemd unit to run outside the sandbox; that a client could forge its address through a proxy; and that every standby batch reread the whole state | Restarting services by killing the process group (an earlier supervisor's children are not in this one's group); hashing every file each batch with a faster hash (the cost is the reading, not the hash); trusting forwarded headers from private addresses too (a LAN peer is not the proxy) |
| D100 | One name for the always-on process: it is the hub everywhere the user or a setting sees it, beside the node, the board, and the relay, all kept by the supervisor. The settings are `hub.enabled` and `hub.cwd` (from `daemon.enabled` and `daemon.cwd`) and `channels.telegram.enabled` (from `daemon.telegram`); an old `daemon` section is still read, per file, wherever that file does not set the new key. Its files are `run/hub.sock`, `run/hub.json`, and `logs/hub.log`; a stop or `down` looks once at a `run/daemon.json` an earlier hub left. The board's System map calls the module `hub` ("Hub"), and the conversation's front end is `hub`, with `daemon` still read as the same. A refused connection is `hub_refused`, with `daemon_refused` still understood. The inbound webhook listens on `127.0.0.1:7683` by default, apart from the relay's 7682. The module `nypi-hub/src/daemon/` and the wire records `daemon_hello` and `daemon_lagged` keep their names | Principle B6 and the rule of plain names: since D80 the user runs `nypi hub up`, `nypi hub run`, and `nypi status`, but the settings, the files, the board, and the messages still said "daemon", so one process had two names, and a user who read `logs/daemon.log` could not tell it belonged to the hub. The webhook's old default was the relay's port, so a VPS running both could not start one of them. Old settings, an old hub's heartbeat, and a client one version behind keep working, so an update needs nothing of the user | Renaming the module and every wire record too (churn in code no user sees, and every client would need both names for a version); dropping the old settings at once (a settings file the user wrote would be silently ignored after an update) |
