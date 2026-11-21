# Using NyPi

## Build

```bash
cargo build --release
```

The binary is `target/release/nypi`. It is a single file with no runtime dependencies.

To check formatting, lints and tests before every commit, enable the repository's hook once per clone:

```bash
git config core.hooksPath scripts/githooks
```

## Configuration (`~/.nypi/`)

| File | Purpose | Who may write it |
|---|---|---|
| `settings.json` | agent name, default model and thinking, tools, compaction, retries, env files, skill paths, MCP servers | user (agent can, through tools) |
| `models.json` | extra models (JSON array; same shape as the catalog) | user |
| `auth.json` | `{ "provider": "api-key" }` fallback for keys | user only (Inspector-protected) |
| `.env` | `KEY=value` file read for API keys | user only (Inspector-protected) |
| `prompt/core.md` | optional override of the built-in read-only core prompt | user only (Inspector-protected) |
| `agents/<name>/self.md` | the agent's self prompt; edited through the `self_prompt` tool, history in `agents/<name>/history/` | agent (versioned) |
| `AGENTS.md` | global instructions loaded into every session | user |
| `skills/` | Agent Skills (`<name>/SKILL.md`) | user |
| `sessions/` | JSONL session trees, one directory per working directory | harness |
| `audit/` | append-only Inspector log, one file per day | Inspector only |
| `telegram.json` | paired Telegram chat IDs | Inspector only (written by pairing) |

Project-level overrides: `.nypi/settings.json` and `.nypi/skills/` in the working directory. Context files
(`AGENTS.md`, or `CLAUDE.md` when there is no `AGENTS.md`) are loaded from every directory from `/` down to the
working directory.

Example `settings.json`:

```json
{
  "agentName": "Lucia",
  "userName": "Alex",
  "defaultModel": "gateway/gpt-6.1-sol",
  "defaultThinking": "medium",
  "envFiles": ["~/.env"],
  "compaction": { "enabled": true, "reserveTokens": 16384, "keepRecentTokens": 20000 },
  "retry": { "enabled": true, "maxRetries": 3, "baseDelayMs": 2000, "maxAgentDelayMs": 60000 }
}
```

`retry` controls what happens when a reply fails with a transient provider error, such as a dropped connection, an
overloaded or rate-limited provider, or a 5xx. NyPi waits `baseDelayMs` (2 s, then 4 s, then 8 s, never longer than
`maxAgentDelayMs`) and continues the run, up to `maxRetries` times. The values above are the defaults, as in Pi.
Quota or billing errors, bad requests, and context overflows are not retried. Every front end shows the retry, and
an abort (Ctrl-C, `/abort`, RPC `abort`) cancels the wait. The same settings cover the chat module's replies,
compaction summaries, and background calls (reflection, portrait, Thinking), which retry without a notice. Pi's
`retry.provider` keys are ignored.

### API keys

For each request NyPi looks up the key in this order:
1. The model's own `apiKeyEnv` variable, if it names one.
2. The provider's standard variable (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`,
   ...).
3. `NYPI_<PROVIDER>_API_KEY`.
4. `auth.json`.

Each variable is read from the process environment or from the files in `envFiles`. Values from env files are kept
private to NyPi and are not exported to commands the agent runs.

**Amazon Bedrock** (`amazon-bedrock/...` models) takes a Bedrock API key in `AWS_BEARER_TOKEN_BEDROCK`, or AWS
credentials: `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` (plus `AWS_SESSION_TOKEN` for temporary keys), or a
profile with static keys in `~/.aws/credentials` (`AWS_PROFILE`, default `default`). SSO, assumed roles, and
instance credentials are not supported yet. The region comes from an ARN model id, `AWS_REGION`,
`AWS_DEFAULT_REGION`, the profile's `region` in `~/.aws/config`, or us-east-1. `AWS_BEDROCK_SKIP_AUTH=1` sends
unsigned requests (for a proxy that signs them), and `AWS_BEDROCK_FORCE_CACHE=1` adds cache points for application
inference profiles whose ids hide the model.

**API dialects.** A model's `api` picks the wire format: `anthropic-messages`, `openai-completions`,
`openai-responses`, `google-generative-ai`, or `bedrock-converse-stream`. A gateway that speaks one of them works
with a custom model entry in `models.json`, for example a Responses-compatible server:

```json
{ "id": "gpt-6.1-sol", "name": "GPT 6.1 (Responses)", "api": "openai-responses", "provider": "mygateway",
  "baseUrl": "https://gateway.example/v1", "apiKeyEnv": "MYGATEWAY_KEY", "reasoning": true,
  "contextWindow": 400000, "maxTokens": 128000 }
```

### Models

```bash
nypi models
```

```bash
nypi models sonnet
```

Any OpenAI-compatible server works through `models.json`. [examples/custom-gateway-models.json](examples/custom-gateway-models.json)
configures an OpenAI-compatible gateway (`api.example.com/v1`). A local Ollama entry looks like this:

A model that accepts system messages in the middle of a conversation can keep its prompt cache when Lucia's goals,
portrait, or tools change: set `"compat": { "supportsMidConvoSystemMessages": true }` on its entry. Changes are
then sent as short updates after the conversation so far, instead of a rewritten system prompt. Leave it off (the
default) unless you know the model handles it; without it, the full prompt is sent as before.

```json
[{ "id": "qwen3:32b", "name": "Qwen3 32B (local)", "api": "openai-completions", "provider": "ollama",
   "baseUrl": "http://localhost:11434/v1", "contextWindow": 32768, "maxTokens": 8192 }]
```

### MCP servers

Add servers to `settings.json`. The format is the same one Claude Desktop and Cursor use:

```json
{
  "mcpServers": {
    "filesystem": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/Users/me/notes"] },
    "github": { "command": "github-mcp-server", "args": ["stdio"], "env": { "GITHUB_TOKEN": "..." }, "disabled": false }
  }
}
```

A server with a `url` is reached over HTTP instead of being started: Streamable HTTP, or the older HTTP+SSE transport
when the server only speaks that (found automatically; `"transport": "sse"` or `"http"` forces one). A server that
restarts and forgets the session is reconnected without you noticing, and notifications it sends between requests
arrive on a stream NyPi keeps open. `${NAME}` in `env` and `headers` is
replaced with the variable from the environment or your env files, so tokens can stay in `~/.env`:

```json
{
  "mcpServers": {
    "linear": { "url": "https://mcp.linear.app/mcp", "headers": { "Authorization": "Bearer ${LINEAR_API_KEY}" } }
  }
}
```

Servers start with the session. Their tools appear as `mcp__<server>__<tool>`; a server that offers resources also
gets `mcp__<server>__list_resources` and `mcp__<server>__read_resource`. When a server announces that its tools
changed, they are re-listed before your next message. A server that fails to start produces a warning and an alert;
the agent still starts without it.

### Command tools

Any program can become a tool. Lucia sees the description and the parameter schema, and NyPi fills the arguments
into the command. No shell is involved, so an argument can never run a second command. This example makes the
Codex CLI a coding specialist:

```json
{
  "commandTools": {
    "codex": {
      "description": "Delegate a self-contained coding task to OpenAI Codex. Give it the full task, the files involved, and how to verify the result.",
      "command": ["codex", "exec", "{task}"],
      "parameters": {
        "type": "object",
        "properties": { "task": { "type": "string", "description": "The complete task description" } },
        "required": ["task"]
      },
      "timeoutSeconds": 3600
    }
  }
}
```

- `{name}` inside an element is replaced with the argument. An element that is exactly `{name}` is dropped when the
  argument is missing, and expands to several arguments when the argument is an array. Write `{{` and `}}` for
  literal braces.
- `stdin` is a template sent to the program's input. `cwd` defaults to the session's working directory. `env`
  values may use `${NAME}` secrets from your env files.
- Output and exit codes are handled as for `bash` (the last 2000 lines or 50 KB, with the full output saved to a
  file). The default timeout is one hour.
- To require approval for a command tool, list it in `approval.tools`.

### Approvals

Before a risky call runs, the Inspector asks you. Examples are `git push`, `git reset --hard`, `sudo`, `curl ... | sh`,
publishing a package, `gh pr create`, `kill -9`, `terraform apply`, `DROP TABLE`, `launchctl load`, sending mail,
`scp`, `rm -r` outside the working directory, and edits to `~/.zshrc` or `~/.ssh`.
- In the terminal: type `y` to approve or `n` to deny; `/deny <reason>` tells the agent why not. Anything else you
  type is a message to her, not an answer. In the full-screen UI the editor asks: `y` approves, Enter denies, other
  text denies with that reason.
- In Telegram: reply `/approve` or `/deny <reason>`.
- With `nypi -p` nobody is at the run. With the hub running, the question goes to your paired Telegram chat, or
  waits for you as a notice (answer in a terminal attached to that folder). Without the hub and without Telegram
  it is denied at once, and the agent reports `Blocked`.
- A terminal request still unanswered after `notify.approvalSeconds` is also sent to Telegram, or to your other
  notification channels. Answer in either place; the first answer counts, and the terminal says when it was
  answered elsewhere.
- Several requests can wait at once in Telegram. Each one has a number: `/approve 3`, `/deny 3 not today`. A bare
  `/approve` answers the oldest.

Settings (the defaults are shown, plus two example rules):

```json
{
  "approval": {
    "mode": "risky",
    "commands": ["deploy\\.sh"],
    "tools": ["mcp__gmail__*"],
    "timeoutSeconds": 900
  }
}
```

`mode` is `risky`, `all` (everything except read-only tools), or `off`. Catastrophic commands (`rm -rf ~`, `mkfs`,
...) are always refused, whatever the mode. Using one of your stored secrets is always asked about, whatever the
mode, unless you trusted it for that host (see [Secrets](#secrets)).

### What the agent cannot touch

Whatever the agent is asked or tricked into doing, its tools cannot change NyPi's own files or read your keys:

- nothing in `~/.nypi` can be written (the audit log, tasks, settings, trust decisions, specialists, prompts), nor
  a `.nypi` folder in any project under your home directory;
- `auth.json`, `~/.env` and any file in `envFiles`, and the tokens in `~/.nypi/run` cannot be read, and a search
  passes over them.

On macOS this is enforced by the system's sandbox around every command the agent runs, so it holds however the
command is written; the agent sees "Operation not permitted". On Linux the same is done with bubblewrap (`bwrap`)
when it is installed: those folders are read-only and the key files read as empty; without it NyPi can only check
what a command says, which stops mistakes rather than determined attempts, and says so at startup. Commands you
type yourself (`!command`) are not restricted. If the sandbox gets in the way of something, `"sandbox": "off"` in
`~/.nypi/settings.json` turns it off.

Commands the agent runs also start without your keys: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GITHUB_TOKEN`,
`AWS_SECRET_ACCESS_KEY`, and anything named `*_API_KEY`, `*_SECRET_KEY`, or `NYPI_*_TOKEN` are taken out of their
environment, so `env` in a command shows none of them. Everything else (`PATH`, `SSH_AUTH_SOCK`, your toolchains'
variables) is passed on as usual. To let one through, name it:

```json
{ "sandbox": { "allowEnv": ["GITHUB_TOKEN"] } }
```

(`"sandbox": { "mode": "off", "allowEnv": [...] }` turns the sandbox off and still keeps the rest out.) MCP servers
and command tools get the variables their own `env` names.

### Project trust

A project can carry its own NyPi configuration: `.nypi/settings.json`, `.nypi/specialists`, `.nypi/skills`, and
`.agents/skills`. Because settings can turn approvals off and add programs as tools, NyPi loads these only from
folders you trust. The first time you start NyPi in a terminal in such a folder it asks; started any other way
(print mode, Telegram, a schedule), it leaves the configuration out and tells you.

```bash
nypi trust
```

shows what applies to the current folder. `nypi trust yes` loads its configuration from now on, `nypi trust parent`
does the same for everything under the parent folder, `nypi trust no` never loads it, and `nypi trust forget`
clears the decision. Decisions are kept in `~/.nypi/trust.json`.

### Status lines and reports

Every answer ends with a status line: `Done:`, `Blocked:`, `Failed:`, or `Continuing:`. If the agent forgets it, NyPi
asks once. If it is still missing, NyPi derives the status from how the run ended. Turn this off with
`"enforceStatusLine": false`.

`Continuing:` is held to its word. When a run ends with it, something has to bring the agent back, and the only
thing that can is a schedule it set during the run (see [Schedules](#schedules)); the task then shows as `waiting` in
`nypi tasks`. Without one, NyPi tells the agent its turn is over and to do the rest now. After three such promises
(`"maxContinuations": 3`; 0 turns the rule off) the run is reported as `Blocked`, and you are asked how to proceed.

A run in which nothing happens at all (no reply text, no tool call starting or ending, no new output from a tool)
for `"stuckMinutes"` (default 30; 0 turns it off) is stopped and ends `Failed`, saying so, so a model stream, MCP
server, or command that never answers cannot hold its conversation forever. Waiting for your approval does not
count. `nypi tasks` resumes the task if you want it taken up again.

NyPi reports out of band, so you hear about things when you are not watching the terminal:

- The status of any run longer than `afterSeconds`.
- A notice when a run shows no progress for `stallSeconds`.
- A notice when an approval waits in the terminal for longer than `approvalSeconds`.
- The watchdog's alerts: stalled, resumed, and failed tasks.

They go to the paired Telegram chats (when a bot is set up) and to every channel in `notify.channels`. Unless you
set `channels`, it is a desktop notification; `"channels": []` turns that off.

Anything raised while nobody is watching (by the watchdog, a schedule, an idle pass, or a run started in the
background) is written to an outbox first. If no channel takes it, it is sent again later, and the next time you open
NyPi in a terminal it is shown as "While you were away". To see what NyPi tried to tell you and how each notice
arrived:

```bash
nypi notices --all
```

The channels:

```json
{ "notify": {
    "telegram": true, "afterSeconds": 120, "stallSeconds": 600, "approvalSeconds": 60,
    "channels": [
      { "type": "desktop" },
      { "type": "webhook", "url": "${SLACK_WEBHOOK_URL}", "format": "slack" },
      { "type": "webhook", "url": "https://ntfy.sh/my-private-topic", "format": "text" },
      { "type": "command", "command": ["/usr/sbin/sendmail", "me@example.com"],
        "stdin": "Subject: {title}\n\n{text}\n" }
    ] } }
```

- `desktop`: Notification Center on macOS (shown as coming from Script Editor; allow it in System Settings →
  Notifications), `notify-send` on Linux.
- `webhook`: `format` is `slack` (`{"text"}`, also Mattermost and Google Chat), `discord`, `text` (the body is the
  text and a `Title` header holds the title, as ntfy expects), or `json` (`{"title", "text", "source"}`). Put secrets
  in `~/.env` and reference them as `${NAME}`; optional `headers` work the same way.
- `command`: any program, run without a shell, with `{title}` and `{text}` filled in, in the arguments or in `stdin`.
  E-mail works through a configured `sendmail` or `msmtp`.

Check the setup with:

```bash
nypi notify test
```

It lists each channel and whether the test notice went out, and names channels that could not be set up.

## Running

| Command | What it does |
|---|---|
| `nypi` | interactive session: with the hub running, this folder's conversation with it; otherwise a new session in this process, which works the same way |
| `nypi -c` | continue the most recent session in this directory |
| `nypi --session FILE` | open a specific session |
| `nypi -p "task"` | run one task and print the final answer; stdin is appended when piped. With the hub running, the task goes to this folder's conversation with it |
| `nypi --standalone` | a session of its own in this process, even when the hub is running |
| `nypi --json -p "task"` | print every event as a JSON line |
| `nypi -m provider/id -t high` | choose model and thinking level |
| `nypi --models 'gateway/gpt-*,claude-sonnet-4-6:high'` | models to cycle through (replaces `enabledModels`); a new session starts on one of them |
| `nypi --tools read,grep,find,ls` | restrict tools (for example read-only) |
| `nypi --no-session` | do not persist |
| `nypi sessions` | list sessions for this directory |
| `nypi tasks` | list open tasks from the task ledger (`--all` includes finished ones; see below) |
| `nypi rate` | say how each finished task went, one key each; `nypi rate <id> <verdict> [note]` rates one (see Scorecard) |
| `nypi scorecard` | the week against your bars, from NyPi's own records and your verdicts (`--weeks N`, `--json`) |
| `nypi send "task"` | give the always-on hub a message and print the result; `--no-wait` returns at once (see Always on) |
| `nypi attach` | talk with the hub: watch, steer, approve, stop; leaving does not stop her |
| `nypi watchdog` | check the ledger once now (stalled, interrupted, and hopeless tasks) |
| `nypi telegram` | show how to pair a chat with the hub's Telegram channel; without a hub, start one here in the foreground with the channel on |
| `nypi --chat-model gateway/gemini-3-flash` | chat mode: a fast model answers, the worker does the work (see below) |
| `nypi --no-chat` | one agent does everything, even when settings name a chat model |
| `nypi --tui` | full-screen UI with a live task panel (see below); `--no-tui` forces line mode |
| `nypi rpc` (or `--mode rpc`) | let another program drive the agent over stdin/stdout (see below) |

### Interactive commands

`/model`, `/models`, `/thinking`, `/new`, `/compact`, `/session`, `/sessions`, `/export [file.html]`,
`/rate <verdict> [note]`, `/skills`,
`/skill:<name> [args]`,
`/prompts`, `/mcp:<server>:<prompt> [args]`, `/reload`, `/quit`. `!command` runs a shell command in the working
directory and shows its output to the model with your next message; `!!command` keeps the output from the model.
Ctrl-C or `/abort` stops a run (a `!` command itself runs to its end). Telegram does not take `!` commands. MCP prompt arguments are `key=value` pairs (quote
values with spaces); a prompt with one argument takes the whole text. `/thinking` with a level the model does not
accept sets the nearest one it does, and says so; `/model` starts the new model at `defaultThinking`. While the agent
is working:
- A typed line is a **steering** message. It is injected right after the current step.
- `/followup <text>` queues a message for after the run.
- `/abort` or Ctrl-C stops the run.

End a line with `\` to continue the message on the next line.

### Full-screen UI

`nypi --tui`, or `"tui": true` in settings, opens a full-screen interface: the conversation on the left, a panel on
the right with the running task (time, instruction, last tool calls), recent activity, and any approval waiting,
and the message editor at the bottom. It works with or without chat mode and with or without the hub (with it,
Ctrl-D leaves her running), and falls back to line mode when stdout is not a terminal or stdin is piped. Under the
editor it shows what Lucia remembers for the message you are typing.

| Key | Action |
|---|---|
| Enter | send (while work runs: steers it, or chat decides in chat mode) |
| Alt+Enter, Ctrl-J | new line |
| ↑ / ↓ | previous / next sent message |
| PgUp / PgDn | scroll the conversation (PgDn back to the bottom follows new output) |
| Esc, Ctrl-U | clear the editor; Ctrl-W deletes a word |
| Ctrl-C | abort the running work; quit when idle |
| Ctrl-D | quit (empty editor) |

When an approval is waiting, the editor asks: `y` approves, Enter denies, other text denies with that reason.
`/followup <text>` queues a message for after the task, `/task` and `/abort` work as in chat mode, and other slash
commands wait until the run has ended. Paste works, including several lines.

### Chat mode

A fast model talks with you, and the strong model works in the background, so you never wait for a reply. Turn it on
in `~/.nypi/settings.json`:

```json
{ "chat": { "model": "gateway/gemini-3-flash" } }
```

(`thinking`, default `off`, and `historyMessages`, default 80, are optional.) The worker keeps `defaultModel`, every
tool, approvals, and the session. This is part of Lucia herself, not a mode of one program: every conversation has
the chat module in front, with the hub or without it, whichever way you reach it (`nypi` in a folder, the
full-screen UI, `nypi -p`, `nypi send`, `nypi attach`, Telegram). Restart the hub after changing the setting. `nypi -p` prints
chat's answer when she answers herself, and the worker's report when she hands the work over.
- Talk normally. Chat answers simple things itself, and hands anything to be done, decided, or worked out to the
  Principal: Lucia's executive, on the strongest model, which starts the tasks and checks them. Chat tells you it
  is on it; the worker's tool calls show as `· task: ...` lines (in Telegram, as batched logs). Chat is the only
  one who talks to you: what the Principal wants you to know, chat tells you, in the same voice.
- What you said reaches the task in your own words: the Principal names the task in a line, and the recent
  conversation is handed over with it as it was (a pasted error or log arrives whole). The worker writes down in
  the task's workspace what will show the task is done, and the Principal checks the report against that.
- Chat looks things up itself, without starting a task: your notes in the vault, what it remembers, your accounts
  and their renewal dates, schedules, goals, what is being worked on and what the Principal decided, and files on
  your devices (`read`, `grep`, `find`, `ls`, from the folder you are in). It only reads; changing anything is the
  Principal's or a task's. A file read that would need your approval is not read, and one that takes over 1.5
  seconds is stopped: chat tells you and hands the search on instead.
- Send several messages in a row and they are answered as one; a message sent while chat is answering joins that
  answer. You never wait behind chat phrasing something the Principal sent: your message comes first.
- Your request goes first with the Principal too: if it is busy reviewing a task or with idle work, it sets that
  aside, with what it has done so far, handles yours, and goes back to it knowing what it decided for you.
- While a task runs, keep talking: corrections go into the running task, and "stop" cancels it. A separate request
  starts a task of its own beside it, up to `tasks.maxConcurrent` (4) at once; its lines are marked with its task.
  `/task` shows what is running; `/abort` or Ctrl-C cancels the conversation's task.
- **You hear a checked result, not a raw report.** The Principal gives a task as a brief: what to do, what counts
  as done, and where to stop. When the task settles, it reads the report against that brief first. If it holds,
  chat tells you the result in a few sentences (the full report is in the task's record and on the board). If it
  falls short (it stopped where another way was open, or says Done without the proof the brief asked for), the
  Principal sends it back with what is missing, without telling you, at most `tasks.maxReviewRounds` (2) times;
  after that you hear the report with what stays open said plainly. While a report is under review the interfaces
  say so. `{ "chat": { "review": "off" } }` shows every report verbatim instead.
- A task with a real goal gets a folder in the vault, `10-workspaces/<date>-<name>/`: `brief.md` is what was asked,
  `findings.md` is what the worker has found and ruled out. It is the worker's memory for the task (it sees both on
  every turn, so a long task does not repeat what already failed), and yours to read. Before it ends Done or
  Blocked the worker writes `report.md` there too: each item of what counts as done checked, what it found and
  delivered, and what it ruled out, every claim with its `[tN]`. A task that forgets is asked once
  (`{ "tasks": { "requireReport": "off" } }` stops that).
- **The review checks the evidence, not only the words.** Under the report the Principal is shown what the task's
  tool calls actually returned for each `[tN]` it cites, references that match nothing, and a warning when the task
  changed a file and said Done without checking it afterwards; it can read any step in full. Several tasks for one
  goal can be reviewed together, and you then hear their outcome once.
- `/followup <text>` queues a message for after the task, as without chat. Other slash commands (`/model`,
  `/compact`, ...) act on the worker and run once its task ends, and so does every RPC command a program sends
  (`set_model`, `fork`, `bash`, ...): turning chat on takes none away.
- **Lucia checks before she asks.** When a request could mean two or three things that a quick look can tell
  apart (which service, which file), she checks them in the background and comes back once with what she found
  and what she recommends. She asks you first only for a matter of taste, or when the first step is yours alone:
  spending money, a password or second factor, proving who you are, or sending something that cannot be taken back.
- If the chat model fails, your message goes straight to the Principal.
- With nobody attached, what chat tells you for the Principal arrives as a notice (Telegram, `notify.channels`).

**The Principal.** It uses the worker's model unless you name one, at high reasoning effort:

```json
{ "principal": { "model": "anthropic/claude-opus-5-5", "thinking": "high", "maxActivationsQueued": 32 } }
```

Nothing you hand it is dropped in silence: whatever it handles ends with a task, a change to its notes, goals, or
schedules, a word to you, or a stated reason why none is needed (`noted_only`, `already_handled`,
`waiting_on_running_task`, in the decision's `disposition`). If it stops with none of these, it is asked once to
finish (`{ "principal": { "enforceDisposition": "off" } }` stops that). A task it starts with `batch_unless_failed`
waits up to `tasks.batchHoldSeconds` (60) for the other tasks of its goal before it is reviewed.

What it does is kept: `~/.nypi/stores/decisions.jsonl` (one line per thing it handled, what it decided),
`~/.nypi/principal/activations/<id>.jsonl` (every step of each), and `~/.nypi/chat/turns.jsonl` (every chat turn:
what it looked up, how long each lookup took, what it handed on). Secrets never appear in them.

**Watching tasks.** With `{ "thinking": { "enabled": true } }`, a strong reasoning model looks over a running task
now and then: every `workerCadenceTurns` (5) turns, and right after the task's context was compacted, it reads the
brief, the findings, and what the last turns did. Almost always it says nothing. When the task is going in circles,
trusting a document over the source, or drifting from the brief, it puts one note into the task
(`[Metacognitive note]: ...`, shown as a thought). That costs one reasoning call per look;
`{ "thinking": { "enabled": true } }` turns it on, and leaving it out keeps it off. Options: `model` (defaults to the
worker's), `level` (reasoning effort, default `high`).

## RPC mode

`nypi rpc` reads one JSON command per line on stdin and writes JSON records on stdout. Diagnostics go to stderr.
Commands use Pi's RPC format, so an optional `id` is echoed in the matching `response`:

```json
{"id": "1", "type": "prompt", "message": "Summarize README.md"}
{"id": "1", "type": "response", "command": "prompt", "success": true, "data": {"disposition": "started"}}
```

- Events stream after the response: `message_update` (with `event.type` `text_delta`, `thinking_delta`, ...),
  `tool_execution_start`/`_end`, `alert`, `auto_retry_start` (`attempt`, `maxAttempts`, `delayMs`, `errorMessage`)
  and `auto_retry_end` (`success`, `attempt`, `finalError`), the compaction summary's `summarization_retry_scheduled`
  (same fields as `auto_retry_start`), `summarization_retry_attempt_start` (`source`, `reason`) and
  `summarization_retry_finished`, and finally `agent_settled` with `status` (`done`, `blocked`, `failed`,
  `continuing`) and its text, plus `pulse`, the ID to pass to `nypi trace`, the final `answer`, and the `task` id.
  The hub's socket (`~/.nypi/run/hub.sock`) speaks this same protocol, after a `daemon_hello` record.
- During a run, send `{"type": "prompt", "message": "...", "streamingBehavior": "steer"}` (or `"followUp"`), `steer`,
  `follow_up`, `abort`, `clear_queue`, or `get_state`. Other commands wait until `agent_settled`.
- `{"type": "typing", "text": "..."}` reports the user's current draft (send it when they pause). NyPi answers with
  `memory_hint` records (`memories`: the ones it would bring in if the draft were sent) whenever that set changes.
  Drafts are not written to the audit log.
- An `approval_request` record (`id`, `tool`, `summary`, `reason`) needs
  `{"type": "approval_response", "id": "...", "approved": true}`, or `false` with an optional `reason`.
- Idle commands: `new_session`, `set_model` (`provider`, `modelId`), `cycle_model` (through `enabledModels` when
  set, as Pi's patterns: `"gateway/gpt-*"`, `"claude-sonnet-4-6:high"`; else every model with a key), `get_available_models`,
  `set_thinking_level`,
  `get_available_thinking_levels`, `cycle_thinking_level`, `compact`, `get_messages`, `get_last_assistant_text`,
  `get_commands`, `get_session_stats` (message counts, token and cost totals, `contextUsage`), `switch_session`
  (`sessionPath`), and `export_html` (`outputPath`, else `nypi-session-<file>.html` in the working directory: a
  static page of the current branch, without script).
- Session tree, as in Pi: `get_entries` (`since`: an entry id), `get_tree` (nested nodes with `entry`, `children`,
  `label`), `get_fork_messages` (user messages with their `entryId`), `fork` (`entryId` of a user message: continues
  from just before it in a new session file and returns its `text`), `clone` (a copy of the current branch), and
  `set_session_name` (`name`; `get_state` reports `sessionName`).
- `{"type": "bash", "command": "..."}` runs the user's own shell command in the working directory, as Pi's `!`; the
  response holds `output` (terminal color codes and carriage returns removed, as in Pi), `exitCode`, `cancelled`,
  `truncated`, and `fullOutputPath`. The output reaches the model
  with the next prompt, unless `"excludeFromContext": true` (Pi's `!!`). `abort_bash` stops it. The audit log records
  the command.
- Settings toggles, saved in `~/.nypi/settings.json` as Pi saves them: `set_auto_retry` and `set_auto_compaction`
  (`enabled`), `set_steering_mode` and `set_follow_up_mode` (`mode`: `"all"` or `"one-at-a-time"`). During a run,
  `abort_retry` ends a retry's wait without aborting anything else.
- Close stdin to stop NyPi; a running prompt is aborted first.

## Telegram

Telegram is a channel of the hub: a paired chat talks to the same Lucia as `nypi send` and `nypi attach`.

1. In Telegram, talk to **@BotFather**, send `/newbot`, and copy the token.
2. Store it in NyPi's secret store, and name it in `~/.nypi/settings.json`:
   ```bash
   nypi secrets set channels/telegram
   ```
   ```json
   { "channels": { "telegram": { "botTokenSecret": "cred://channels/telegram" } } }
   ```
   The token is then sealed on disk and taken out of everything NyPi writes. An older setup with
   `TELEGRAM_BOT_TOKEN=123456789:AA...` in `~/.env` still works when `botTokenSecret` is not set; when it is set and
   nothing is stored under it, Telegram stays off and the hub's log says why.
3. Start (or restart) the hub: `nypi hub up` again; or `nypi hub run` in the foreground.
4. Ask for the pairing code on the machine NyPi runs on, and send it to your bot as `/pair <code>`:
   ```bash
   nypi telegram
   ```

Only paired chats are served. A chat that is not paired is told nothing about the agent and is recorded in the audit
log. A code pairs one chat and is then replaced. Five wrong codes replace it and raise an alert; after fifteen,
pairing closes until the hub restarts.

In the chat, send tasks as normal messages. A message while Lucia is working goes into the task she is on, `/abort`
stops her, and the other commands work once she is idle. What the chat receives:

- a task the chat started or spoke into: tool activity in compact batches, each reply, and how it ended;
- an approval question from any task, wherever it was started: answer `/approve` or `/deny [reason]`, with the
  number when several wait. The first answer counts, from the chat or from a terminal;
- a task started elsewhere is not shown step by step; its result arrives as a notice when nobody was watching it.

`"channels": { "telegram": { "enabled": false } }` in settings turns the channel off.

Without a hub, `nypi telegram` starts one in the foreground, in the directory it was run in, with the channel on;
Ctrl-C stops it.

## Tasks

Every run is a task in `~/.nypi/tasks/ledger.jsonl`, written before the run starts. Each tool call is recorded
before it executes and when it finishes.

```bash
nypi tasks
```

lists open tasks (`id  state  last heartbeat  origin  goal`); `--all` includes finished ones, and
`nypi tasks show <id>` shows one in full, with the `nypi trace` id of the message that started it.

If NyPi dies in the middle of a task (crash, kill, power loss), the task stays open with the dead process's id and
shows as `interrupted`. It resumes the next time its session is opened: `nypi -c` in the same directory, or the
hub when it restarts. A tool call that was cut off is not run again; the model is told it was interrupted
and checks its effects before deciding what to do. A task interrupted five times fails instead of resuming. Print-mode
tasks have their own session, so resume them explicitly:

```bash
nypi tasks resume t-dc32a39bf266
```

`nypi tasks cancel <id>` closes a task that will not continue.

### Benchmark

```bash
nypi eval --list
```

```bash
nypi eval
```

runs twelve tasks, each with one thing in the way that real work has (an endpoint that answers 403, a README two
versions old, a tool that is not installed, a specialist that says Done without having done it, a payment only you
may make, ...), and judges each by what is on disk and what reached the scenario's own server afterwards, not by
what the agent says. Every scenario runs in a temporary NyPi home and folder against a server on this machine: of
your `~/.nypi` only the keys, the model list, and the model settings are read, and nothing in it is written. It
uses your default model (`nypi eval -m provider/id` picks another) and costs what its requests cost: twelve real
tasks of a few to a few dozen turns each. No run has been timed or priced here yet, so try one scenario first.

`--scenario <id>` (repeatable) runs some of them; `--json` prints the report as JSON. The exit code is 0
when every scenario passed. The summary:

| Measure | Meaning |
|---|---|
| Autonomous pass rate | the task was really done, unaided |
| Premature Blocked rate | the agent stopped (Blocked or Failed) although a way existed |
| False Done rate | the agent said Done and the check says otherwise |
| Dead-end repeats | times an approach already shown not to work was tried again |
| Authority violations | times something only you may do was done (must be 0) |

Run it before and after a change to prompts, models, or the runtime; a number that got worse is the review.

```bash
nypi eval --three-planes
```

runs every scenario through all three parts of Lucia instead of one worker: each task arrives as your message to
chat, which answers or hands it to the Principal, which starts tasks, reviews them, and has chat tell you. A
scenario ends when every part is quiet (at most three minutes), and is judged by the same checks. The report adds,
per scenario, chat's turns, the hand-overs, the Principal's activations, the tasks it started, and the times it
sent a task back, and says how many scenarios chat answered alone. Four scenarios are made for this way of working
(`nypi eval --suite planes`): a request that names no service (Lucia should check, not ask), a fix that must be
checked after the edit, two services to inspect for one answer, and a lookup chat should answer without starting
anything. `--chat-model` and `--principal-model` pick those models; otherwise your settings' `chat.model` and
`principal.model` are used, then the worker's. With `--three-planes` every review, report, and disposition rule is on.
`--suite worker|planes|all` picks the scenarios (default: `worker`, or `all` with `--three-planes`).

### Scorecard

Whether NyPi is doing its job well is measured from two things: the records it already keeps, and your verdict on
each result. A verdict is one of four words:

| Verdict | Key | Meaning |
|---|---|---|
| `good` | `g` | it did what you asked |
| `fixed` | `f` | it got there, but only after you pushed or corrected it |
| `wrong` | `w` | what it reported was not true or not usable |
| `silent` | `s` | it stopped or went quiet without telling you |

```bash
nypi rate
```

goes through the tasks of the last 14 days (`--days N`) that have no verdict, oldest first, and takes one key for
each: a verdict's key, Enter to skip, `q` to stop. `nypi rate <id> <verdict> [note]` rates one task, and a later
verdict replaces an earlier one. Inside a session, `/rate g` (or `/rate w it booked the wrong day`) rates the
latest result; after a run long enough to be reported (`notify.afterSeconds`), the terminal reminds you. Only you
rate: the agent's tools cannot run `nypi rate` or write the ledger.

```bash
nypi scorecard
```

shows the current week, Monday to Monday, and the three weeks before it (`--weeks N`; `--json` for the numbers):

```
NyPi scorecard · week of 9 Nov (so far)

  Silent stops              1         bar 0             MISSED · stopped with nobody told: t-cut
  Right without help        3 of 5    bar at least 80%  MISSED · 60%
  Done that was wrong       1 of 5    bar at most 5%    MISSED · 20%
  Useful self-started work  0 of 0    bar at least 50%  not judged · 0 rated, 5 needed to judge
```

- **Silent stops** counts tasks you rated `silent`, tasks whose process has been gone for over 30 minutes with
  nothing picking them up, and notices that reached no channel and no front end within an hour. The bar is zero.
- **Right without help**: of the tasks you asked for and rated, those rated `good` that you never sent a message
  into while they ran.
- **Done that was wrong**: of the rated tasks that reported Done, those rated `wrong`.
- **Useful self-started work**: of the rated tasks NyPi started itself (idle passes, goal reviews, schedules),
  those rated `good`.

The three shares count rated tasks only, so unrated work never passes for success, and a share with too few
verdicts is shown as not judged. Below the bars come the facts that need no verdict: how many tasks you gave it and
on how many days, how they ended, how many it recovered after an interruption, how many notices reached you, and
how many finished tasks still have no verdict. The bars are yours to set:

```json
{ "scorecard": { "rightWithoutHelp": 0.8, "falseDone": 0.05, "usefulSelfStarted": 0.5, "minRated": 5 } }
```

### Specialists

A specialist is an expert agent with its own instructions, model, tools, and experience. To see which agent CLIs this
machine already has, and whether they are signed in:

```bash
nypi specialists detect
```

It looks on PATH and inside the ChatGPT and Claude desktop apps, which bundle their own (often newer) Codex and Claude
Code CLIs. A CLI must be signed in before it can work: `codex login`, or `claude auth login` for Claude Code (use the
path `detect` prints). Create a specialist from a template:

```bash
nypi specialists new coder --from codex
```

`--from native` makes a NyPi agent; `codex` and `claude` wrap those CLIs (`codex exec --json`, `claude -p
--output-format stream-json`). This writes `~/.nypi/specialists/coder/` with `profile.json` and `instructions.md`; edit
both. A project can keep its own in `.nypi/specialists/`. Then give it a task in the current directory:

```bash
nypi specialists run coder "Fix the failing test in crates/nypi-ai and explain the cause"
```

Progress goes to stderr, the answer to stdout. The run is a task in `nypi tasks`, the CLI's raw output is in
`~/.nypi/specialists/<name>/runs/<task>.log`, and a one-line summary is added to `experience.md`, which the next run
reads. `nypi specialists` lists them, and `nypi specialists show <name>` prints the profile and the briefing it gets.

Once specialists exist, Lucia sees them and delegates on her own: ask a coding question and she hands it to the coding
specialist, checks its report, and answers. In a conversation (the hub's, or a terminal's own) the specialist works
in the background: she tells you it is under way, you can go on talking and giving her other work, she can start
several specialists at once, and when one finishes its report comes back by itself, she reviews it, and you get the
result (as a notice when you are not there). The terminal shows `[a specialist's report came back; Lucia is reviewing
it]`. With `nypi -p` she waits for the specialist instead, since nothing would be there to take the report. Each
delegation is a child task in `nypi tasks show`; a task that is `waiting` on a specialist closes when the report has
been reviewed. Leaving a terminal that has a session of its own stops the specialists working for it (it says how
many); they are taken up again when NyPi next runs. With the hub they keep working. A native specialist's risky calls ask you for approval
the same way Lucia's do. A small change she already understands (a few lines in one or two files) she makes herself,
since handing it over takes minutes. To keep a session from delegating, run it with `--tools` and leave `delegate` out.

Profile fields: `kind` (`native` or `external`), `description`, `domain`, `model`, `thinking`, `tools` (native), and
for external ones `command` (argv, with `{task}`, `{cwd}`, `{model}`), `stdin` (template, usually `{task}`), `format`
(`text`, `codex-json`, `claude-stream-json`), `timeoutSeconds`, `env`, and `passEnv` (names of keys in your
environment, such as `OPENAI_API_KEY`, that the CLI is started with; every other key and token is taken out, as
for the agent's own commands). The external CLI uses its own login,
sandbox, and permissions: the Codex template allows writes inside the working directory (`-s workspace-write`), and the
Claude template accepts file edits (`--permission-mode acceptEdits`).

### Sources

Every tool result Lucia sees starts with a reference such as `[t3]`, and she cites the results her claims rest on.
Under each answer NyPi lists what the references point to:

```
Sources:
[t2] bash grep -n 'RESTART_LIMIT' crates/nypi-core/src/ledger.rs · trace 489cdf47-5
[t9] matches no tool result in this session: treat that claim as unverified
```

`nypi trace <id>` shows the call and what led to it. A report that used tools but cites none says so. In RPC mode the
same data is in `agent_settled.citations`. A specialist's references appear as `[name:t2]` in its report.

### Memory

Lucia keeps long-term memory across sessions in `~/.nypi/memory/memories.jsonl`. Tell her something that lasts (a
preference, a person, a decision, a commitment) and she remembers it, with tags that help find it later; ask about it
in another session, in any language, and she recalls it first. She never stores passwords or keys.

| Command | What it does |
|---|---|
| `nypi memory` | the count and the newest memories |
| `nypi memory search "dentist"` | search, with scores (`--explain` shows why) |
| `nypi memory add "Mei is my sister" --tags family,妹妹 --kind person` | add one yourself |
| `nypi memory forget m-415f97932c5e` | forget one |

Search ranks by relevance first: a memory that does not match is never shown, however important. Among the ones that
match, the important ones, the ones that carried feeling, and the ones in recent use come first; older events fade
faster than preferences or people. `--explain` prints the four factors (relevance, importance, emotion, activation),
each from 0 to 1. The weights are settings, for example to let importance count more:

```json
{ "memory": { "weights": { "importance": 0.3, "tauDays": { "event": 30 } } } }
```

Lucia also brings up what she remembers without being asked, when it bears on what you say: for a request or a
question, anything that clearly matches; in casual chat, only a strong match. Sensitive memories (health, money, loss,
conflict, or anything remembered as painful) never come up unasked while you are joking around. A memory she brought
up once in a conversation is not repeated. To see why something came up, look for `memoryValve` in `nypi trace` or the
TUI's activity panel. In the TUI she recalls while you type: pause for a moment and the input box's bottom border shows
what she remembers about it ("remembers: ..."), before you send. The first message after a start waits for the local
embedding model to load (a few seconds at most), so memories in another language are found from the start. What you type and delete is never recorded. To tune
or turn it off:

```json
{ "memory": { "valve": { "enabled": true, "taskThreshold": 0.25, "casualThreshold": 0.45, "limit": 3 } } }
```

Unset weights keep their defaults (relevance 0.55, importance 0.2, emotion 0.1, activation 0.15; they are scaled to
sum to 1).

Search matches by meaning, not just words: "when is my teeth checkup?" finds the dentist, and a question in Chinese
finds a memory written in English. This uses a small local embedding model that runs inside NyPi, so nothing leaves
your machine. Install it once (about 400 MB):

| Command | What it does |
|---|---|
| `nypi memory model download` | fetch the model and verify its checksum |
| `nypi memory model` | which embedding sources are set and whether each is ready |
| `nypi memory reembed` | give every memory a vector from the current model (also done in the background, 200 at a time, when Lucia starts) |

Without the model, search falls back to keywords and tags (Chinese and Japanese work without spaces; English plurals
and verb forms match, so "bikes" finds "bike"). Exact names and numbers count in both modes.

To use an embeddings endpoint instead, list it first; the local model stays as the fallback:

```json
{ "memory": { "embeddings": [
  { "baseUrl": "https://api.openai.com/v1", "model": "text-embedding-3-small", "apiKeyEnv": "OPENAI_API_KEY" },
  { "local": "qwen3-embedding-0.6b-q4" }
] } }
```

An endpoint that fails is skipped for 10 minutes. After switching models, run `nypi memory reembed`. `"embeddings": []`
turns semantic search off.

### Vault

Memory holds single facts. What has structure, Lucia keeps as Markdown notes in a vault of her own,
`~/.nypi/vault/` (separate from any notes of yours), which you can open in any editor:

```
00-inbox/        what came in and is not sorted yet
10-workspaces/   one folder per substantial task (YYMMDD-<slug>/)
20-playbooks/    how something is done
30-entities/     projects, systems, devices, people
90-archive/      finished workspaces, by month
attachments/     files copied in (screenshots, PDFs, exports)
```

She reads and writes it with her own tools (`vault_search`, `vault_read`, `vault_write`, `vault_list`,
`vault_attach`, `vault_archive`); shell commands cannot touch it. Notes link to each other with `[[name]]`, and a
search finds sections by meaning and by words and brings along the notes linked to or from the best hits. Optional
frontmatter between `---` lines: `title`, `kind`, `tags`, `status`, `aliases`, `updated`. You may edit notes by hand;
she sees the change at her next search. `--tools` without `vault` leaves the vault out.

### Assets

`asset_manage` keeps a registry of what she looks after for you and for herself: accounts, servers, domains,
subscriptions, identities, deliverables, with when each renews (`~/.nypi/assets.jsonl`). Ask her what renews soon,
or to record a new server. A record never holds a secret; it names one by its handle.

### Secrets

Passwords, API keys, and tokens Lucia may use for you are stored sealed, and she never sees their values:

```bash
nypi secrets set github/lucia-bot
```

```bash
nypi secrets list
```

```bash
nypi secrets rm github/lucia-bot
```

`set` asks for the value without showing it (or takes it from a pipe: `pbpaste | nypi secrets set github/lucia-bot`);
it is never an argument. `list` prints handles and dates only. The values are in `~/.nypi/secrets.enc`, encrypted;
the master key is in your login keychain on a Mac (entry `sh.nypi.secrets`), and elsewhere in `~/.nypi/secrets.key`,
readable by you alone. Neither file can be read by the agent's tools.

Lucia refers to a secret as `cred://github/lucia-bot`. When she makes an HTTP request with it (`http_request`), the
harness puts the value into the request and removes it from everything that comes back, so it never reaches the
model, the session file, or the audit log. You are asked before each use. To let a secret be used for a host
without asking:

```json
{ "approval": { "trustedCredentials": { "github": ["api.github.com"], "corp": ["*.example.com"] } } }
```

The key is the secret's scope (the part before the slash); `*.example.com` covers a domain and the hosts below it.
Redirects are not followed with a credential.

### Schedules

Ask Lucia to remind you or to check back on something ("remind me at 9 tomorrow to call the dentist", "tell me when the
build passes", "every weekday at 8 summarize my inbox") and she sets a schedule. You can also set one yourself:

| Command | What it does |
|---|---|
| `nypi schedule` | what is set (`--all` includes finished ones) |
| `nypi schedule add --at "2026-11-02 09:00" --notify "Call the dentist"` | a reminder |
| `nypi schedule add --daily 08:00 --weekdays mon,tue,wed,thu,fri --prompt "Summarize what changed in ~/work overnight"` | a task Lucia runs |
| `nypi schedule add --when-command "cargo test -q" --notify "Tests pass"` | when a command succeeds (`--changes`: when its output changes) |
| `nypi schedule add --when-file ~/inbox/report.pdf --prompt "Read the new report and summarize it"` | when a file changes |
| `nypi schedule cancel s-e3e5ad687fe6` | cancel one |
| `nypi schedule tick` | fire whatever is due now |

A notice shows in the open terminal and goes to your notification channels. A task runs in the folder it was set
from, inside the hub (in that folder's conversation) or inside the terminal that is open, and its result
reaches you as a notice. Schedules are checked while the hub runs or a terminal is open; anything missed fires
once when one starts again. A task a terminal was running for a schedule is cut off when you close that terminal,
and is taken up again as interrupted; with the hub that does not happen. To check with neither, run
`nypi schedule tick` from launchd or cron: it carries out what fired and returns when it is done. Conditions fire once unless added with `--repeat`.

A notice costs nothing; a task is a full model run each time it fires. To be told when something appears in a file,
test for it with a command (`--when-command "grep -q FAILED build.log" --notify "The build failed"`): it fires once,
when the text shows up. `--when-file` with `--repeat` fires on every write.

### Idle time (DMN)

When nothing is going on, Lucia can look over her goals, recent tasks, schedules, and commitments in memory, and act
on what deserves it: take a goal's next step, set a reminder, or tell you that something is coming due. Most passes
find nothing and stay silent; otherwise you get a short note through your notification channels. It acts without
being asked and costs tokens, so it is off until you turn it on:

```json
{ "dmn": { "enabled": true } }
```

It runs after 30 idle minutes (at most every 2 hours), or every 12 hours regardless, at most 6 times and 300,000
tokens a day (`idleMinutes`, `minGapHours`, `everyHours`, `maxPassesPerDay`, `dailyTokens`, and `model` to use a
cheaper model). A front end must be running, or call `nypi schedule tick` from launchd or cron.

With chat in front of the hub's conversation, a pass is the Principal's instead (on its model, within the same
daily budget, one at a time). It looks at your goals by two clocks: a deadline coming up or past, and what blocks it
(if only you can unblock it, chat tells you); and the goals left longest, one of which gets its next step started, so
none is forgotten. It also files what waits in the vault's inbox. You hear from it only through chat, and only when
it has something to say. A fired schedule's prompt and anything handed in through the webhook go to the Principal
too, to decide what to do with.

| Command | What it does |
|---|---|
| `nypi dmn` | whether a pass is due, today's budget, recent passes and their notes |
| `nypi dmn run` | one pass now |

A pass also keeps her vault in order, within the same budget. The workspace of a task that has been settled for a
week is moved to `90-archive/<year>-<month>/` (`{ "vault": { "archiveAfterDays": 7 } }`; a task that is open or
waits for you keeps its workspace, however old). Notes waiting in `00-inbox/` are filed into the pages they are
about under `30-entities/`, or into the workspace of a task under way. What keeps coming up in memory about one
person, project, or thing is written into its page. A lesson from her reflections that recurred becomes a draft
under `20-playbooks/`. She writes only inside the vault; your skill folders are not touched.

Notices (results nobody was there to see, a run gone quiet, an approval nobody answered) go through the channels
the hub runs too: a paired Telegram chat hears each once, whether or not `notify.telegram` is on.

### Handing things in (the inbound webhook)

Other programs of yours can hand Lucia something to file: a link shared from your phone, a forwarded article, a
note, a file. It is off until you turn it on. Store a secret for it (at least 16 characters), then enable it and
restart the hub:

```bash
nypi secrets set channels/webhook
```

```json
{ "channels": { "webhook": { "enabled": true, "listen": "127.0.0.1:7683" } } }
```

A caller sends one request with the secret:

```bash
curl -X POST http://127.0.0.1:7683/ingest -H "Authorization: Bearer $SECRET" -H "Content-Type: application/json" -d '{"source":"ios-share","text":"Worth reading","urls":["https://example.com/article"]}'
```

The fields are `source` (the program), `sender`, `text`, `urls`, and `attachments` (`[{ "name": ..., "data":
<base64> }]`); at least one of text, urls, or attachments must be there. The answer names the note it was saved
as, `00-inbox/<date>-<time>-<title>.md`, with files under `attachments/`. Saving happens first and does not
depend on anything else. Lucia then reads it as a task: she fetches what the links point to, links the note to
the pages it is about, adds a short synthesis, and tells you in a line or two what arrived.

To reach it from your phone, set `listen` to the hub's address in your tailnet. It takes nothing without the
secret, and what comes in is treated as material to file: text in a shared page is never taken as an instruction
from you. Without a stored secret the webhook does not start, and the hub's log says so.

### Portrait

Lucia also learns who you are from how you talk: lasting traits such as how you like answers, what you care about,
the languages you use, and what you want avoided. Every trait rests on your own words, quoted exactly; a trait she
cannot back with a quote is dropped. One-off requests, specific facts (those go to memory), anything sensitive, and
secrets are left out. When you change your mind, the old trait is replaced.

| Command | What it does |
|---|---|
| `nypi portrait` | the traits, most shown first |
| `nypi portrait show u-f4e2288cff4c` | the words of yours behind one, with the session and a trace ID |
| `nypi portrait forget u-f4e2288cff4c` | remove one |

It runs in the background after a turn, so answers never wait for it. To use a cheaper model for it, or turn it off:

```json
{ "portrait": { "model": "gateway/gemini-3-flash" } }
```

```json
{ "portrait": { "mode": "off" } }
```

### Reflections

After a task that can teach something (five or more tool calls, a failed step, a failed or blocked end, or a
correction from you mid-task), Lucia looks back on it in the background. Durable lessons go into her self-prompt under
`## Lessons`, versioned like her own edits; skill changes are only proposed. Most tasks teach nothing new, and then
nothing is written.

```bash
nypi reflections
```

shows the newest reflection in full and lists earlier ones. Settings, with defaults:

```json
{ "reflection": { "mode": "auto", "minToolCalls": 5, "model": null } }
```

`mode` is `auto`, `always`, or `off`; `model` (for example a cheaper `provider/id`) defaults to the session's.

### Goals

Your short-, medium-, and long-term goals live in `~/.nypi/goals.md`, which Lucia keeps in mind and advances.

```bash
nypi goals add --term short "Ship the M3 release" --next "write the release notes" --dir ~/Projects/nypi
```

`nypi goals` shows the file; `nypi goals done <id>` and `reopen <id>` change one. You can also edit the file by hand:
one `- [ ]` line per goal under `## Short term`, `## Medium term`, or `## Long term`, with optional `· next: …`,
`· due: YYYY-MM-DD`, `· dir: …`, and `· vault: 10-projects/<name>`. Lucia sees the open goals in every session and records progress with her `goals`
tool; she cannot rewrite the file any other way, and every change she or the commands make keeps the previous version,
with its reason, in `~/.nypi/goals-history/`.

**A goal that takes more than one task is a campaign.** With chat in front, the Principal gives it a folder in the
vault, `10-projects/<name>/`, names it on the goal (`vault: 10-projects/<name>`), and keeps a `plan.md` there: the
objective, the workstreams with each item open `[ ]`, under way `[~]`, blocked `[!]`, or done `[x]`, the decisions
taken, and the notes that belong to it. Every task it starts for the goal sees that plan above its own workspace, and
the ledger and the schedules record which goal each one serves (`nypi tasks show` shows it). You can write or edit a
plan yourself: it is an ordinary note.

To have her move a goal forward now:

```bash
nypi goals review
```

She picks the goal that matters most and can move without you, does the step (or delegates it), updates the goal, and
reports; if nothing can move without you, she says what she needs. A review works in the goal's `dir` and can change
files there, like any task. To have the hub review every few hours, behind whatever task is under way (with chat
in front, the review is the Principal's, by the same two clocks as idle time, below):

```json
{ "goals": { "reviewEveryHours": 6 } }
```

### Watchdog

While NyPi runs (the hub, or a terminal with a session of its own), it checks the ledger every minute:
- a running task whose process has written nothing for 5 minutes is marked `stalled`, and you get an alert;
- a task that ended with `Continuing:` and then nothing happened is reported to you as blocked;
- a task whose process died is taken up again after 2 minutes (unless you reopened its session first), inside the
  hub or the open terminal, and its result reaches you as a notice;
- a task that cannot be resumed (restart limit, no saved session, dead for over a day) is marked failed.

A task that is `waiting` on a schedule is left alone while the schedule is pending, closed when it fires (the work
goes on as the scheduled task), and handed to you as blocked if the schedule is cancelled first.

Alerts appear in the front end, in your Telegram chat when one is paired, and in the notification channels. Only
one NyPi process acts at a time. Settings, with their defaults:

```json
{ "watchdog": { "enabled": true, "intervalSeconds": 60, "stallSeconds": 300, "resume": true,
                "resumeAfterSeconds": 120, "giveUpAfterHours": 24 } }
```

### Always on: the hub and the supervisor

Lucia can run as one process that is always there, ready for a message at any time: the hub, on the machine that
is your **hub**. The supervisor is the small process, with no model, whose job is that it stays up.

```bash
nypi hub up
```

That makes this machine the hub, copies the binary to `~/.nypi/bin/nypi`, and installs the supervisor as a service
(a macOS LaunchAgent, or a systemd user unit on Linux) that starts at login and is restarted if it exits. The
supervisor then starts the hub and keeps it running. Run `nypi hub up` again after rebuilding NyPi. On Linux,
`loginctl enable-linger <user>` keeps it running while you are logged out.
Keys must be in an env file (`~/.env` or `envFiles`), since a service does not see your shell's environment.

With the hub running, every terminal is an interface of the same Lucia, in the same conversation.

**One conversation.** There is one conversation with her, whichever folder, device, or channel you reach her from.
What differs is where you are: a terminal tells the hub the folder it was opened in, and what you ask there is
done there.

```bash
nypi
```

joins the conversation from this folder: the greeting says `you are at <folder>`, you see what she is doing (also
what was asked from another terminal or from Telegram), what you type is a message (it steers her while she
works), Ctrl-C stops a run, `/approve` and `/deny [reason]` answer an approval, and `/detach` (or Ctrl-C when
nothing runs) leaves, and she keeps working. A message from this terminal has its files read and its commands run
in this folder; the same conversation continues when you open `nypi` in another folder, where the work is then
done. `/new` starts the conversation afresh, for every interface. The conversation is kept in
`~/.nypi/sessions/primary/`.

```bash
nypi -p "run the tests and fix what fails"
```

gives her one task, to be done in this folder, and prints the answer when it settles. Nobody can answer an approval
from `nypi -p`, so you are told as a notice when one waits.

A folder's own `AGENTS.md` is read when work is done there. A folder's own `.nypi/settings.json` (its command
tools, MCP servers, approval rules) is not: the conversation runs with your settings. For a session that loads
all of a folder's configuration, use `nypi --standalone` there.

**From anywhere.** Without a folder, you are at the hub's own folder (your home directory unless `hub.cwd`
says otherwise), for work that belongs to no project:

```bash
nypi send "check whether the backup ran last night and tell me"
```

```bash
nypi attach
```

`nypi send --no-wait "..."` returns as soon as the message is taken; the result then reaches you as a notice.
Telegram is the same conversation, at the hub's own folder.

**More than one thing at once.** Without a chat model one task runs at a time and a message during it steers it.
With one (see Chat mode), a separate request starts a task beside the first, up to `tasks.maxConcurrent`. She is
told what else is under way (tasks beside this one, jobs a schedule started, runs in other programs), so you can
ask about any of it.

**Schedules and goal reviews** go to the hub as messages: a schedule's prompt joins the conversation after
whatever is running, as a task of its own, and works in the folder (and on the device) it was set in;
`nypi goals review` works in the hub's own folder.

Conversations the hub held per folder before this version are still in `~/.nypi/sessions/`; open one with
`nypi --standalone -c` in its folder. The hub does not continue them.

**What still gets a session of its own**, even with the hub running: `--standalone`; a model, thinking level, or
tool set given on the command line; `--session` and `--no-session`; `--json`, `--mode`, and `nypi rpc`; and a chat
model of its own (`--chat-model`) or none (`--no-chat`). `nypi` says so when that is why. Such a session runs the
same hub inside its own process, so the terminal, `-p`, and the full-screen UI behave as they do with the hub;
it ends when you leave. Idle passes (the DMN) run inside the hub, as a job of their own, not in a conversation.

What the hub does when nobody is attached:
- a finished task's status line, and an approval she is waiting for, reach you as notices (see Status lines and
  reports), so nothing waits unseen;
- a task she was on when the hub died is resumed when it comes back.

What the supervisor does about the hub:
- it starts it again the moment it exits, and kills and starts it again when it has not shown life for 90 seconds;
- it tells you when it had to (a warning), and tells you as an error when the hub stopped three times in ten
  minutes, then waits five minutes before trying again. The hub's log is `~/.nypi/logs/hub.log`.

The hub works in your home directory and continues its own session there. Settings, with their defaults:

```json
{ "hub": { "enabled": true, "cwd": null },
  "channels": { "telegram": { "enabled": true } },
  "board": { "enabled": true, "port": 7878, "local": true } }
```

The supervisor also starts the board and keeps it running. `nypi board --link` prints its address with the token.
It serves this machine only until you set `"board": { "local": false }`, which puts it on your network for your
phone: a page that can approve and stop work should not appear on whatever network the machine joins unless you say
so. `"board": { "enabled": false }` leaves the board to you.

`"hub": { "enabled": false }` keeps the supervisor without the hub. `nypi hub run` runs the hub in the
foreground, for trying it or for another service manager. The agent's own tools cannot talk to the hub: a message on its socket
counts as yours.

The supervisor also does the rounds that must not depend on the hub: it checks tasks, fires schedules, starts
idle passes when they are due, and sends again any notice that has not reached you.

```bash
nypi status
```

It says what this machine is (hub or node), whether the supervisor and the hub run, how many machines are linked,
and how many notices have not reached you. `nypi hub down` stops the hub and removes the service, keeping all data;
`nypi hub up` starts it again. Its log is `~/.nypi/logs/supervisor.log`. `nypi watchdog` and `nypi schedule tick`
still do one round each, for cron.

Limits today: nothing runs while the Mac sleeps or you are logged out; recall while you type works in the
full-screen UI only, since the line-based terminal does not report what you are typing; and a terminal cannot
change where it is without being opened again in the other folder.

### More machines: nodes of the hub

Lucia can work on the files and the shell of other machines while the conversation, her memory, and the model keys
stay on the hub. Each other machine is a **node**: it holds no conversation and no key, and applies its own sandbox
and protected paths to everything it is asked to do.

| Command | Where | What it does |
|---|---|---|
| `nypi hub up [--entry <host:port>]` | the machine that becomes the hub | makes it the hub, installs the supervisor, prints the hub key's fingerprint, and shows join requests until Ctrl-C (the hub keeps running); `--entry` saves the name it is reached at from outside (none by default: the local network only), also on a hub that runs already |
| `nypi hub here [--force]` | a personal node | moves the hub to this machine (below); `--force` takes over from its standby copy when the hub is gone |
| `nypi node up [--name <name>] [--code <code>]` | a machine that should join | asks the hub to admit it with a six-digit code, installs the supervisor, and keeps it linked |
| `nypi relay up [--hub-key <fingerprint>] [--listen 127.0.0.1:7682]` | the machine behind the entry (a VPS with Cloudflare Tunnel or a reverse proxy) | runs the relay beside whatever the machine is, kept by the supervisor |
| `nypi hub down`, `nypi node down`, `nypi relay down` | that machine | stops that service and keeps all data; the last one also removes the supervisor service |
| `nypi node leave` | a node | unpairs it (tells the hub when it can) and forgets its key |
| `nypi status` | anywhere | what this machine is, its epoch and entry, whether the supervisor and the relay run, and the link: for a node, direct (with the address) or relayed; for the hub, machines linked and how many directly |
| `nypi devices` | the hub | admitted machines and browsers: name, trust, system, linked (direct or relayed) or last seen |
| `nypi devices pending`, `admit <code> [--exec-only]`, `refuse <code>`, `remove <name>` | the hub | answer join requests; unpair a machine or a browser (its open links close at once) |
| `nypi devices invite <name> [--exec-only] [--expires 1h]` | the hub | a one-time code for a machine nobody watches: `nypi node up --code <code>` admits it with no question |
| `nypi hub run`, `nypi node run`, `nypi relay run` | that machine | the same in the foreground, for another service manager |

**Joining.** On the new machine:

```bash
nypi node up
```

On the hub's own network it finds the hub by mDNS, with nothing to configure. From elsewhere, name the entry once:
`nypi node up --entry nypi.example.com:443` (it is saved in `settings.json`, and every later version keeps it; the hub
saves its own with `nypi hub up --entry …`). With an entry it asks the entry which hub it leads to, joins that hub
directly when it hears it on this network, and through the entry otherwise. With none, and no hub on this network, it
says so. It prints a six-digit code, and waits. The hub asks you, wherever you are: in a terminal attached to the
hub, in Telegram, and on the board, as "MacBook (macos aarch64) asks to join from 192.168.1.30. Code 482 913". Admit
it only when the code is the one on the new machine's screen:

```
/admit 482913              (it is your computer: it works and you can talk to Lucia from it)
/admit 482913 exec-only    (it only carries out calls)
/refuse 482913
```

The code is made from both machines' keys and the encrypted session, so a machine in the middle cannot make the two
screens match. A request lasts ten minutes; at most three wait at once, and after ten a day from one address (thirty
in all) requests are refused until the next day, and you are told once. A request is never a message to Lucia.

**Direct or relayed.** A node links to the hub directly when it can: the hub announces itself on the local network
(mDNS) and tells every node the addresses it can be reached at (each of its network addresses on port 7681, a port you
forwarded on the router as `hub.publicAddress`, or one the router maps for it with `hub.portMapping: true`). Otherwise
it links through the relay at the entry. A node linked through the relay tries the direct addresses every minute and
moves as soon as one works; what it is doing for Lucia carries on. Whichever way, the link is encrypted between the
two machines' own keys: the relay and Cloudflare pass it on and cannot read it. `nypi status` on the node says which
it is.

**The relay.** The entry is a relay you run on a machine with a public name, such as a small VPS, behind Cloudflare
(a Tunnel, or the orange-cloud proxy) or a reverse proxy that serves the certificate. It needs no port of its own: it
listens on `127.0.0.1:7682`, and with a Cloudflare Tunnel the VPS opens no inbound port at all and its address stays
out of DNS. Only the hub whose key it was given can register there, and it passes links and browsers on without
reading them.

**The hub's commands from a node.** On a personal node, `nypi tasks`, `goals`, `memory`, `portrait`, `schedule`,
`specialists`, `reflections`, `dmn`, `rate`, `scorecard`, `notices`, `trace`, `eval --list`, `devices`, and `secrets`
act on the hub's stores: they run on the hub and print here, with the hub's exit code. `nypi secrets set shop/key`
asks for the value here, unseen, and the hub stores it; it is never printed or logged. `nypi rate` asks here, one
task at a time. What would start a run (`goals review`, `specialists run`, `schedule tick`, `dmn run`, `tasks resume`,
a benchmark run) runs on the hub only; `status`, `node`, `relay`, `hub here`, `models`, `trust`, and `eval` are about
the machine they run on. On an exec-only machine the hub's commands are refused.

**Talking from a node.** On a personal node, plain `nypi` (and `send`, `attach`) talks to the hub's conversation, and
what you ask from a folder there is done in that folder on that machine. On an exec-only node it says so;
`nypi --standalone` runs a session of that machine's own.

**From the hub**, `nypi --on laptop --at /Users/you/site "run the tests"` joins the conversation as if you were at that
folder on the laptop. It is the same conversation as everywhere else, so a task can also use a node without you being
there: she is told which machines are connected and what each offers, her file and shell tools can run one call on a
named machine, and a tool only one machine has is called by that machine's name. Each such call is checked twice: by
the hub for the machine and folder it goes to (an approval question names the machine), and by that machine's own
node, which has the last word. Your own `!command` runs on the hub's computer only.

At a folder on a node Lucia reads that folder's `AGENTS.md` files. Where you trusted that folder there (`nypi trust
yes` on that machine), its skills, its native specialists, and its safe settings (thinking level, compaction, retries,
tools that need your approval) apply while she works in it. A specialist that is a program is never run for a folder
on another machine.

**Moving from D80.** A machine paired with `nypi pair` is moved to these files the first time it runs: a laptop that
was paired as a node and an interface becomes a personal node with one key, a node-only one an exec-only node, and it
links again without joining.

**A worked example.** A Mac mini at home is the hub, a VPS runs the relay, and a MacBook comes and goes.

1. On the Mac mini: `nypi hub up --entry nypi.example.com:443` (your own name; without `--entry` the hub is on the
   local network only, and an entry can be added later the same way). It prints the hub key's fingerprint and the line
   for the VPS.
2. On the VPS: `nypi relay up --hub-key <fingerprint>`, then point `nypi.example.com` at `http://127.0.0.1:7682` there
   (`cloudflared tunnel` with that name and address, or a reverse proxy with WebSocket on). Then `nypi node up` on the
   VPS too, and admit it as exec-only (`/admit <code> exec-only` on the mini): Lucia can run things there, and it
   cannot talk to her.
3. On the MacBook, at home: `nypi node up`. The mini is heard on the network, so the request goes straight to it;
   admit it as your computer. `nypi status` on the MacBook says "Linked to the hub, direct (192.168.1.5:7681)".
4. The MacBook leaves home: its link drops, and it links again through the relay ("relayed through
   wss://nypi.example.com"). Plain `nypi` there talks to Lucia as before. When it can reach one of the mini's direct
   addresses again (back home, or through a port you forwarded), it moves to it within a minute.
5. On the phone: open `https://nypi.example.com`. It shows a six-digit code and asks to be admitted; Telegram (and the
   board at home, and any terminal attached to the hub) shows the same code. `/admit <code>`: the phone's page turns
   into the board. `nypi devices remove iphone-browser` ends that session at once.
6. Moving the hub to the MacBook for a trip: `nypi hub here` on the MacBook. The mini stops its work, sends what is
   new since the MacBook's standby copy, and becomes a node of the MacBook; the MacBook starts as the hub at epoch 2,
   registers at the relay, and the VPS and the phone carry on through it without joining again. Back home,
   `nypi hub here` on the mini moves it back (epoch 3).

**Moving the hub.** Every personal node keeps a standby copy of the hub's state in `~/.nypi/standby/`: the
conversation, the vault, the ledger, goals, the stores, the register of machines, the hub key, the secret store with
its key, and the env files (no tool reads it; `nypi status` says how old it is). It is brought up to date every five
minutes (`hub.standbyMinutes`) and soon after Lucia decides something; through the relay every half hour, at half its
rate. An exec-only machine never gets one. `nypi hub here` on a personal node moves the hub there: the hub lets its
current step finish (up to `hub.handoverWaitSeconds`), stops what runs (the new hub takes it up again), sends what is
new, and steps down; if anything goes wrong before the end, nothing changes. Env files that were outside the old
hub's NyPi home arrive as `~/.nypi/env/<name>`, and nothing outside the NyPi home is written.

If the hub is gone (the machine died, or is far away and off), `nypi hub here --force` on a personal node takes over
from its copy once the hub has not answered for two minutes (`hub.forceAfterSeconds`). It says how old the copy is and
asks first (`--yes` does not ask): what the old hub did after the copy is not there. There is never more than one
hub: each move raises the epoch, the relay takes only the highest, and a machine that comes back as an older hub steps
down by itself, keeps what it had under `~/.nypi/diverged/`, and tells you so through the new hub. Nothing is merged.

Limits today: nothing was run between two real machines yet, only between processes on one machine; two machines that
are both behind routers without a forwarded or mapped port reach each other only through the relay; a call that is
running when a link drops goes on for two minutes and its result is handed over when the link is back; and a
schedule that watches a file cannot be set at a folder on a node.

## Audit log

Every event is a JSON line in `~/.nypi/audit/YYYY-MM-DD.jsonl` with `ts`, `session`, `seq`, `source`, and
`payload`. Records written during a run also have `cause`: the ID of the pulse (the user's message) that started it.
Pulses themselves are records with `source` `pulse`.

To see what a message led to:

```bash
nypi trace
```

lists the last 30 pulses (`id  time  [conversation]  source  kind  detail`; the conversation is its folder's name,
and a timer firing in the hub itself has none), and

```bash
nypi trace a18c67a6-1
```

prints that pulse's causal chain, the pulses it caused (tool calls, the run status, alerts), and the messages and tool
calls recorded while it was the cause. The hub numbers the pulses of all its conversations in one sequence, so the
chain of a scheduled task's result leads back to the timer that fired. `--days N` searches further back (default 7).

Example: what did the agent run today?

```bash
grep '"tool_execution_start"' ~/.nypi/audit/$(date +%F).jsonl | python3 -c "import sys,json;[print(json.loads(l)['payload']['arguments']) for l in sys.stdin]"
```

### Installing and updating

Install from the signed releases (macOS on Apple silicon, Linux on x86_64 or aarch64):

```bash
curl -fsSL https://raw.githubusercontent.com/Nyporte/NyPi/main/scripts/install.sh | sh
```

It checks the archive against the release's `SHA256SUMS`, installs `~/.nypi/bin/nypi` (the binary the supervisor
runs), and links `~/.local/bin/nypi` to it. `NYPI_VERSION=v0.4.1` installs that version.

| Command | What it does |
|---|---|
| `nypi update --check` | says whether a newer release exists, with its page |
| `nypi update [--version vX.Y.Z] [--yes]` | puts the latest (or that) release in place on this machine and restarts the supervisor; it asks first at a terminal |
| `nypi update rollback` | puts the previous binary back |
| `nypi update --node <name> [--version vX.Y.Z]` | (hub or personal node) the named node updates itself |
| `nypi update --all [--version vX.Y.Z] [--include-relay]` | (hub or personal node) every node in turn, each back on the new version before the next, then the hub; it stops at the first that fails; a machine that runs the relay only with `--include-relay` |
| `nypi update keygen [--out <file>]`, `nypi update sign <manifest.json> [--key <file>]` | for whoever publishes releases: the signing key, and a manifest's signature |

Nothing replaces the installed binary until the release's signature (made with the key built into `nypi`), the
archive's size and SHA-256, and the new binary's own `nypi --version` have all checked out. The supervisor then keeps
the update once everything has run for 30 seconds, or puts the previous version back, tells you, and never tries that
version again on its own. `nypi --version` says the version, the target, and the commit; `nypi status` and
`nypi devices` say what each machine runs.

In the background, in `settings.json`:

```json
{ "update": { "mode": "notify", "checkIntervalHours": 6, "autoApplyWindow": "02:00-05:00", "autoRelay": false } }
```

`notify` (the default) tells you once of each new release; `auto` updates the machine when it is idle (nothing
running) and within `autoApplyWindow` if one is set; `off` checks nothing. A machine that runs the relay is only told,
unless `autoRelay` is true. `update.repo` (`<owner>/<repo>`) takes releases from a fork.

### The board

```bash
nypi board
```

starts the board on port 7878 and prints a link with its token: one for this Mac, one for your phone on the same
network, and one on your tailnet when Tailscale runs (reachable away from home, through its encrypted tunnel). Proxy
tunnel addresses (198.18.0.0/15) are left out: other devices cannot reach them. Opening the link stores the token in a cookie, so later visits need no token; on a phone, "Add to Home
Screen" makes it an app. Leave it running (it is a service like the Telegram bridge): every NyPi process links to it
within 30 seconds, and stopping it changes nothing for them. Away from home, open the entry
(`https://nypi.example.com`) instead: the board there asks to be admitted with a code, as a new machine does, and
needs no token (section "More machines").

The home screen also shows the hub as a whole: **Tasks** is the tree of what is being worked on (a task under the
task it works for, with where it works, what it waits on, and its workspace in the vault), **Devices** lists every
device with whether it is connected and what it is paired as, and **Vault** lists the notes of the inbox and of
task workspaces that changed last.

- **Now** answers "does anything need me, and what is happening": the inbox (approvals with Approve and Deny, blocked
  and stalled tasks, unacknowledged alerts), one card per running process with its current step (thinking, writing
  with the reply streaming in, running `bash: cargo test` for 12 s, retrying 2/3, compacting, waiting for your
  approval), its context gauge and cost, and Stop and Message; then the last 24 hours (runs by outcome, tool calls
  and failures, retries, tokens, cost against a usual day) and the next schedule firings.
- **A task** reads as a story: what was asked, each reply with its reasoning folded, each tool call as one line
  that opens to its arguments and output, retries, compactions, approvals, steering, delegated work, and how it
  ended. While it runs, the story grows and the reply streams in; Stop and Message stay at the top.
- **Chat**, **Principal**, and **Vault** (with chat in front): every turn of chat's, with what it looked up, how
  long each lookup took, and what it handed to the Principal, linked to the activation it became; the Principal's
  stack, queue, and every activation step by step, with where it was set aside for you and taken up again; your
  goals with their plans, and every note of the vault. You can talk to Lucia from the Chat screen (the hub must
  run). A task's story shows the goal and activation it belongs to, the plan and workspace it works under, the
  observer's steers, and the Principal's verdicts. While chat answers or the Principal works, you see what it is
  doing and the text as it is written. A task shows its brief, findings, and report, what the harness found behind
  each `[tN]` it cites, and a warning when it changed a file and said Done without checking; an activation shows
  what was looked up for it before it began and how it was disposed of. Secrets are taken out of everything the
  board shows.
- **Activity**: tasks and notable events, newest first, by kind (tasks, alerts, approvals, schedules, your own board
  actions), time range, and text.
- **System** is the whole program as a map: every module in its region (You, Always on, Lucia, Tools and models, Kept on
  disk), the connections between them, and what each is doing now. Blue is working, a green dot is ready, a hollow
  dot is not open, dashed is off, red needs attention; a line that moves has work going over it. Point at a module
  to light what it is connected to; select it to read what it is for, its numbers, and where it lives in the code.
  The chips show one kind of connection at a time. Below the map the same modules are a list, for a phone.

Approvals can be answered from the board or where the run started, whichever comes first. Message steers a running
task; a blocked task is answered where it was started. Everything you do from the board is in the audit log.

The page is built once (and after changes to `crates/nypi-board-ui`) with `scripts/build-board.sh`, then
`cargo build`. Without it the board serves its API and says how to build the page.

`--local` keeps the board on this Mac, `--port N` picks another port, and `--new-token` replaces the token (old links
stop working). The connection is plain HTTP: use network access on networks you trust.

