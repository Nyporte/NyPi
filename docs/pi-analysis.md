# Analysis of Pi (the upstream TypeScript implementation)

NyPi started from a clone of [earendil-works/pi](https://github.com/earendil-works/pi), commit `70ca8e896`. The
TypeScript sources are kept as a read-only reference in the sibling checkout `../Pi` (decision D18). This document
records what Pi does and how, and what NyPi keeps, changes, or drops. Paths refer to `../Pi/packages/`.

## 1. Layering

Pi is a stack of npm packages. Each layer depends only on the ones below it.

```
coding-agent   CLI, tools, sessions, compaction, skills, extensions, TUI/print/JSON/RPC modes
    |
agent          Turn loop: stream -> tool calls -> tool results -> repeat; steering/follow-up queues; hooks
    |
ai             Provider-neutral message model + streaming event protocol; ~40 provider adapters
```

Side packages: `tui` (differential terminal renderer), `mcp` (MCP client), `durable` (crash-safe conversation/task
runtime), `chord` (plugin/RPC host), `codemode` (sandboxed JS that calls tools), `telemetry`, `env` (remote exec daemon).

## 2. `ai`: one message model, many providers

`ai/src/types.ts` defines the vocabulary every other layer speaks:

- **Messages**: `system | user | assistant | toolResult`.
  - Assistant content is an ordered list of `text`, `thinking` (with an opaque provider signature for replay), and
    `toolCall {id, name, arguments}` blocks.
  - Every assistant message records `provider`, `model`, `usage` (input/output/cache tokens + cost), `stopReason`
    (`stop | length | toolUse | error | aborted`), and `errorMessage`.
- **System prompt and tools live in the transcript.** The leading system message carries the base prompt, named
  `sections`, and `toolsAdded`. Later system messages patch sections or add and remove tools. Replaying the transcript
  always reproduces exactly what the model saw. Pi uses this to keep provider prompt caches stable across tool changes.
- **Streaming protocol**: providers emit `start`, `text_start/delta/end`, `thinking_*`, `toolcall_start/delta/end`, then
  `done` or `error`. Every event carries the partial assistant message, so a consumer can render from any event alone.
- **Contract: providers never throw.** A failure becomes a final assistant message with `stopReason: "error"`. The agent
  loop handles failure as data.
- `transform-messages.ts` adapts the history across providers. Examples: thinking blocks from another provider become
  plain text, images become placeholders for non-vision models, tool-call IDs are normalized.
- Provider adapters (`api/anthropic-messages.ts`, `api/openai-completions.ts`, ...) translate this model to each wire
  format. Each adapter also handles prompt-cache breakpoints, thinking-effort mapping, and compatibility flags.

**NyPi keeps** the message model, the never-throw contract, and the event protocol, expressed as Rust enums.
**NyPi starts** with two adapters: Anthropic Messages, and OpenAI Chat Completions, which also covers OpenAI-compatible
endpoints such as DeepSeek, OpenRouter, Groq, Ollama, vLLM, and llama.cpp. More adapters come later.
**NyPi keeps** transcript-carried system messages since M4 (decision D40): a leading system message, then section and
tool updates, sent in place for models flagged as accepting them and folded for the rest. M1 to M3 sent the current
prompt and tool list on every request.

## 3. `agent`: the turn loop

`agent/src/agent-loop.ts` is the heart of Pi. In pseudocode:

```
emit agent_start
pending = steering queue
loop:
  while has_tool_calls or pending:
    append pending messages
    msg = stream assistant response          # transformContext -> convertToLlm -> provider
    if msg.stop in (error, aborted): emit end; return
    if msg has tool calls:
      if msg.stop == length: fail all calls  # arguments may be truncated
      else execute calls                     # sequential or parallel, see below
      append tool results
    pending = steering queue                 # user typed while we worked
  pending = follow-up queue                  # queued for after the run
  if empty: break
emit agent_end
```

Tool execution has three steps:
1. **Prepare**: look up the tool, validate arguments against its JSON schema, and run `beforeToolCall`, which can block
   the call.
2. **Execute**: run the tool. Tools may stream partial updates. A thrown error becomes an error result.
3. **Finalize**: run `afterToolCall`, which can rewrite the result.

In parallel mode, step 1 runs in order and step 2 runs concurrently. Results are appended in the assistant's original
order. A tool can force sequential mode.

Two queues implement interruption:
- **Steering** messages are injected after the current turn's tool calls complete.
- **Follow-up** messages are injected when the agent would otherwise stop.

`abort()` cancels the current stream or tool through an `AbortSignal`.

**NyPi keeps** all of it: the queues, the hooks, the parallel/sequential split, and the length-truncation guard. Steering
is the direct implementation of "priority response" (principle A5). `beforeToolCall` is where the Inspector's guard sits.

## 4. `coding-agent`: the product

| Concern | Pi implementation | NyPi M1 |
|---|---|---|
| Tools | `read` (offset/limit, 2000 lines / 50 KB head truncation, images), `write`, `edit` (multiple exact unique replacements, diff output), `bash` (tail truncation, full output spilled to temp file, timeout), `grep`/`find`/`ls` (gitignore-aware, result limits) | Same tools and limits |
| System prompt | `system-prompt.ts`: preamble, tool snippets, rules, project context (`AGENTS.md`/`CLAUDE.md` up the directory tree), skills list, cwd, each in an XML-tagged section | Same, plus a persona section and the read-only conduct principles |
| Sessions | `session-manager.ts`: JSONL file. Header `{type:"session", id, cwd}`, then entries `{type, id, parentId, timestamp, ...}` that form a tree. The active branch is the path to the current leaf. Entry types: message, model_change, thinking_level_change, compaction, branch_summary, label, custom | Same file format (version 3 subset): message, model_change, compaction |
| Compaction | When tokens exceed `contextWindow - reserveTokens` (16384), walk back to keep about 20000 recent tokens and summarize the rest into a structured checkpoint (Goal / Constraints / Progress / Decisions / Next steps / Critical context). The summary entry replaces older messages in later requests; original entries stay in the file. | Same algorithm and prompt |
| Skills | Agent Skills spec. Directories with `SKILL.md` (frontmatter `name`, `description`). Only name, description, and path go in the prompt; the model reads the file on demand. | Same; also reads `~/.pi/agent/skills` so existing skills keep working |
| Extensions | TypeScript modules loaded in-process (jiti). They register tools, commands, providers, renderers, and event hooks. | Not portable to a compiled binary. Replaced by: MCP servers (out of process), skills, and subprocess "command tools" (M2) |
| Modes | Interactive TUI, print (`-p`), JSON event stream, RPC (JSONL over stdio), SDK | Interactive (line-based, streaming, steering), print, JSON. RPC in M2. |
| Trust | Project trust gate before loading project settings and resources | Project context files are loaded; the trust gate (Pi's store and nearest-ancestor rule) gates project settings, specialists, and skills since decision D61 |

## 5. `durable`: what Pi already has for long-running work

`durable` commits conversation, turns, tool calls, and user state to storage before showing them, so a crashed process
resumes where it stopped. It also has child tasks and a task graph. This is the closest upstream answer to principle B6
(the action chain never breaks silently). NyPi's task ledger (M3) borrows its central idea: persist intent before effect,
and on restart treat any request that was in flight as a failed attempt and retry it.

## 6. What Pi deliberately does not do, and NyPi must

Pi's README states that it skips sub-agents and plan mode and leaves them to extensions. Pi is also purely reactive: it
does nothing until the user types. The NyPi vision (see [principles.md](principles.md) and [roadmap.md](roadmap.md))
inverts both:

- **Proactive**: pulses, the default mode network (DMN), and a watchdog. Pi has no notion of idle-time activity.
- **Multi-agent**: Lucia manages long-lived specialist agents.
- **Memory**: Pi has session history and compaction, but no long-term associative memory.
- **Split cognition**: a fast chat module separate from slower thinking, memory, and tool modules. Pi has one
  synchronous loop.

The kernel (sections 2 and 3) is the right foundation for all of these: each NyPi "module" is either an agent loop with
its own prompt, tools, and model, or a plain Rust task that exchanges pulses with them.
