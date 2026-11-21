# NyPi V4 Plan: Two Instances, One Entry, Direct Links, and Moving the Hub

This plan builds on `D92` (`c55ea2ced`). It replaces the D80 way of adding devices (a code made on the hub and typed
on the device, a separate interface pairing, a tailnet address) with this:

1. **Two kinds of instance: a hub and a node.** The same supervisor service keeps either one running, whichever the
   machine currently is. A third, optional service, the **relay**, runs on a machine with a public address.
2. **One built-in entry.** NyPi's default settings name the entry `nypi.example.com:443`, so `nypi node up` needs no
   address. The entry is where a machine first finds the hub and where a phone's browser reaches the board.
3. **Direct whenever possible.** The relay only introduces and falls back. A node and the hub link directly when they
   can (the same local network first, then any other address the hub can be reached at), and a link that had to start
   through the relay moves to a direct path as soon as one works. The relay's bandwidth is for what cannot go direct.
4. **Joining is authenticated by the user.** `nypi node up` shows a six-digit code; the hub asks the user, on a device
   the user already trusts, whether to admit the machine, showing the same code. The code is computed from both
   machines' keys and the encrypted session, so neither a stranger nor someone in the middle can get in.
5. **The hub can move between the user's own machines** (`nypi hub here`), carrying all of its state, keys, and
   secrets. The relay accepts only the hub with the highest term (`epoch`), so there is never more than one hub.

Execute the four parts in order as four clean, tested commits: `D93`, `D94`, `D95`, `D96` (include
`docs/v4-plan.md` in the `D93` commit). Every commit must pass `cargo fmt --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` (no network calls: everything
between machines is tested with several NyPi homes over loopback), and `./scripts/build-board.sh && cargo build`.
Update `AGENTS.md`, `docs/architecture.md` (body and decision log), `docs/devices.md`, `docs/board.md`,
`docs/usage.md`, `docs/roadmap.md`, and `docs/overview.md` in the same commits.

---

## The commands, all of them

Instance and device commands (new or changed; the rest of the CLI keeps its names):

| Command | Where | What it does |
|---|---|---|
| `nypi hub up` | the machine that becomes the hub | First time: creates the hub key, writes `instance.json` (`mode: hub`, `epoch: 1`), installs and starts the supervisor, prints the hub key's fingerprint and the `nypi relay up` line for the relay machine, then shows join requests live until Ctrl-C (the hub keeps running). Again: starts the hub if it is stopped; changes nothing that is right. Refused when the entry already has a hub with another key (says to use `nypi hub here` or another entry). |
| `nypi hub here [--force]` | a personal node | Moves the hub to this machine (Part 3). |
| `nypi node up [--name <name>] [--code <code>]` | any machine that should join | Finds the hub (Part 2), asks to be admitted with a six-digit code (Part 1) or uses an invite code, writes `instance.json` (`mode: node`), installs and starts the supervisor. Again: starts the node if it is stopped. |
| `nypi relay up [--hub-key <fingerprint>] [--listen 127.0.0.1:7682]` | the machine behind the public entry (e.g. a VPS with Cloudflare Tunnel or reverse proxy) | Writes the relay settings, installs the supervisor if needed, and starts the relay beside whatever instance the machine is. |
| `nypi hub down`, `nypi node down`, `nypi relay down` | that machine | Stops that service and keeps all data. `down` of the last service on a machine also removes the supervisor service. |
| `nypi node leave` | a node | Unpairs this machine (tells the hub when it can) and removes `instance.json` and its key. |
| `nypi status` | anywhere | What this machine is (hub or node, epoch, entry), whether the supervisor and relay run, and the link: for a node, connected or not and **direct (address) or relayed**; for the hub, machines connected, how many direct, and requests waiting. |
| `nypi devices` | hub or personal node | Admitted machines and browsers: name, trust, system, connected or last seen, direct or relayed. |
| `nypi devices pending \| admit <code> [--exec-only] \| refuse <code> \| remove <name>` | hub or personal node | Answer join requests; unpair a machine or browser (its open links close at once, as in D83). |
| `nypi devices invite <name> [--exec-only] [--expires 1h]` | hub or personal node | A one-time code for a machine nobody watches (`nypi node up --code <code>` admits it with no question). |

Foreground runs, for the supervisor and other service managers: `nypi hub run` (today's `nypi daemon`), `nypi node
run` (today's `nypi node`), `nypi relay run`, `nypi board` (unchanged, `--link` too). `nypi supervisor` stays as the
service's own entry point, hidden from help; `install`, `uninstall`, and `status` are done by `up`, `down`, and
`nypi status`.

Removed: `nypi pair` (all flags), `nypi node --connect`, `nypi node --hub`, `nypi daemon` (now `nypi hub run`),
`nypi supervisor install|uninstall|status`.

Unchanged names: talking (`nypi`, `-p`, `--tui`, `send`, `attach`, `--on`/`--at`, `--standalone`), and `tasks`,
`goals`, `memory`, `portrait`, `secrets`, `schedule`, `specialists`, `reflections`, `dmn`, `rate`, `scorecard`,
`notices`, `notify`, `trace`, `trust`, `models`, `sessions`, `eval`, `telegram`, `rpc`, `watchdog`, `board`. What they
do on a node is Part 4.

---

## Part 1 (Decision D93): Two Instances, One Supervisor, Joining With Approval

### 1.1 The default entry and the instance file

- Add `entry` to `Settings` (top level), default `"nypi.example.com:443"`, from one constant (`DEFAULT_ENTRY` in
  `crates/nypi-core/src/config.rs`). It can be overridden in `settings.json`, by `NYPI_ENTRY`, or by `--entry` on
  `hub up`, `node up`, and `relay up`. Someone building NyPi for another domain changes that one constant.
- Add `~/.nypi/instance.json` (in `Protected`, written only by NyPi's own commands, never by a tool):

```json
{
  "mode": "node",
  "name": "macbook",
  "instanceId": "<random, made once per machine>",
  "epoch": 1,
  "hubFingerprint": "<sha256 of the hub key>",
  "trust": "personal",
  "direct": ["192.168.1.20:7681"]
}
```

- `mode`: `hub` or `node`; the supervisor reads it every round. `name`: what the hub calls this machine.
  `instanceId`: tells this machine apart from another of the same name. `epoch`: the hub term this machine knows of
  (Part 3). `hubFingerprint`: the hub key this machine trusts; it stays the same when the hub moves. `trust`:
  `personal` (the user's own machine: carries out calls and talks to the hub as the user; may hold a standby copy and
  become the hub) or `exec-only` (carries out calls only). `direct`: the hub's direct addresses last heard (Part 2).
- Migrate old files once, when first read: the device side of D80 pairing (`devices/hub.json`) becomes
  `instance.json` with `mode: node`; a machine with `hub.listen` set that runs a daemon becomes `mode: hub`. Delete the
  old files after a successful migration.

### 1.2 One key and one pairing per machine; the interface role goes away

Replace `PeerRole { Node, Interface }` in `crates/nypi-hub/src/pairing.rs` with one record per machine:

```rust
pub struct Peer { pub name: String, pub trust: Trust, pub paired_at: String, pub instance_id: String }
pub enum Trust { Personal, ExecOnly }
```

- Each machine has **one** key (`device-id.key`); `interface-id.key` is no longer made. On migration, two
  `peers.json` entries with the same name (one node, one interface) merge into one `Personal` peer under the node's
  key, and the interface key is forgotten.
- A link says what it is for in its first line: `Hello { purpose: Node | Talk | Join, token: Option<String> }`.
  - `Node` goes to the register of nodes and is read as the node protocol only, for every admitted peer.
  - `Talk` goes to the conversation, **only** for a `Personal` peer; an `ExecOnly` peer is refused with a reason.
  - `Join` is a key the hub does not know asking to be admitted (§1.3).
  - `daemon/remote.rs` stays the one place this is decided, by the trust the machine was admitted with and the link's
    purpose, never by anything the machine says about itself. Reword the matching `AGENTS.md` rule.

### 1.3 Joining: a six-digit code checked by the user

`nypi node up`:

1. The machine makes its key if it has none, finds the hub (Part 2 adds the local network; in Part 1 it uses the
   entry), opens a TLS 1.3 link, and reads the hub's key from the handshake. It sends `Hello { purpose: Join }` with its
   name (default: the machine's name), operating system, and NyPi version.
2. Both sides compute the **confirmation code**: six digits from
   `SHA-256("nypi-join" || hub key fingerprint || device key fingerprint || TLS exporter value)`, the exporter value
   being the session's RFC 5705 keying material (`export_keying_material`, label `"nypi join"`). The machine prints it
   and waits.
3. The hub records a **join request** and asks the user, on every trusted place that can answer (§1.4):
   > MacBook Pro (macOS 16, NyPi 0.x) asks to join from 203.0.113.8. Code 482 913.
   > Admit as my computer · Admit as exec-only · Refuse
4. Admitted: the hub adds the peer and answers `Welcome { name, trust, epoch, direct }`. The machine saves the hub
   key's fingerprint, writes `instance.json`, and installs and starts the supervisor (§1.5). Refused or expired: it says
   so and exits.

A stranger who reaches the entry can only make a request, which nobody admits. Someone in the middle who swaps either
key makes the two codes differ, and the user admits only a request whose code matches the screen in front of them.
Requests last 10 minutes; at most 3 wait at once; at most 10 a day from one IP address and 30 in all, after which new
ones are refused until the next day and the user gets one notice saying so. Join requests are notices, never
activations of the Principal, and never reach the conversation. `--code` admits with no question (the invite code is
the user's approval); invite tokens are kept only as hashes, as in D80.

### 1.4 Where the user answers

A join request is a new record (`join_request`: `id`, `kind` (`machine` or `browser`), `name`, `system`, `from`,
`code`, `expires`) that goes wherever approvals go today; the first answer counts:

- the terminal of the hub or of a `personal` node (`nypi`, the full-screen UI): `/admit <code> [exec-only]`,
  `/refuse <code>`; and `nypi devices pending|admit|refuse` from any shell there;
- `nypi hub up` while it waits, so the first machine can be admitted from the hub itself;
- Telegram, when paired: the same `/admit` and `/refuse`;
- the board: an inbox card with the three buttons.

### 1.5 The supervisor runs whatever this machine is

`crates/nypi-cli/src/supervisor.rs`:

- Each round, read `instance.json` and the relay settings:
  - `mode: hub` → keep the daemon (`nypi hub run`) and the board running; do the scheduler, watchdog, and outbox rounds.
  - `mode: node` → keep `nypi node run` running; do **no** scheduler, watchdog, or outbox rounds (they are the hub's).
  - `relay.enabled` → also keep `nypi relay run` running (Part 2), whatever the mode.
  - When the mode changes between rounds, stop what the old mode ran before starting what the new one needs.
- Service install for `up`, removal for `down`:
  - macOS: the existing LaunchAgent (`dev.nypi.supervisor`).
  - Linux: a systemd user unit (`~/.config/systemd/user/nypi-supervisor.service`, `Restart=always`), enabled and
    started with `systemctl --user`; print that `loginctl enable-linger <user>` keeps it running while logged out.

### 1.6 A terminal on a joined machine

Plain `nypi` (and `--tui`, `-p`, `send`, `attach`) on a `personal` node opens a `Talk` link to the hub with the focus
at this folder on this machine. On an `exec-only` node it says this machine carries out calls but cannot talk to the
hub, and suggests `--standalone`.

### 1.7 Tests for Part 1

- Join over loopback TLS: both sides compute the same code; a swapped key gives a different one; admitted as personal
  the machine links as a node and talks; as exec-only it links as a node and is refused when it asks to talk; refused
  and expired requests end the join; the limits hold. An invite code admits once; an expired or used code is refused.
- The default entry is used without settings; `NYPI_ENTRY` and `settings.json` override it.
- Migration of a D80 home (both keys, two peer entries, `devices/hub.json`) to one peer and one `instance.json`, linking
  again without joining.
- Supervisor: node mode runs the node and no rounds; a mode change stops the daemon and board before starting the node,
  and the other way round; `relay.enabled` adds the relay in either mode.

---

## Part 2 (Decision D94): The Relay, Direct Links, and the Public Board

### 2.1 Who talks to whom

All traffic is between the hub and a node (or a terminal on a personal node, or a browser). Nodes never talk to each
other. So "direct" means: a node reaches the hub without the relay.

### 2.2 The hub's direct addresses

- The hub listens directly on `hub.listen`, default `0.0.0.0:7681` and `[::]:7681`, with the same pinned TLS, purposes,
  and join limits as everywhere.
- It keeps a list of **direct candidates**, refreshed every minute and when the network changes: each interface's
  private IPv4 and global or unique-local IPv6 address with the listen port (skipping loopback, link-local, and
  `198.18.0.0/15` tunnels); `hub.publicAddress` when the user has forwarded a port on the router; and, when
  `hub.portMapping` is on (default off), an address mapped on the router by UPnP-IGD or NAT-PMP, with the public IP the
  relay reports seeing (§2.4).
- The list goes to every linked node in its `Welcome`, and again whenever it changes. Nodes save it in
  `instance.json` (`direct`).
- On the local network the hub also announces itself by mDNS (`_nypi._tcp`, with the hub key's fingerprint and epoch
  in the TXT record) when `hub.lanDiscovery` is on (default on).

### 2.3 How a node reaches the hub

On every connect (`nypi node run`, a `Talk` link, `nypi node up`):

1. Try, all at once: the hub found by mDNS on this network; the last direct address that worked; and the saved direct
   candidates, those in a subnet this machine shares first. The first to complete a TLS handshake **with the pinned hub
   key** wins. Give direct attempts 1.5 s.
2. Then the entry (through the relay). For a first join, which has no saved candidates, mDNS comes first and the entry
   second.
3. A node linked through the relay keeps trying the direct candidates every 60 s, and as soon as one works it opens the
   direct link and closes the relayed one. Calls in flight are safe across the switch: the node's journal keeps them for
   their lease and hands their results to the new link (D74).
4. mDNS answers and candidate lists are only hints: nothing is trusted until the handshake shows the pinned key, and
   among hubs showing it, the one with the highest epoch wins (Part 3).

The hub records each link as `direct` (with the address) or `relayed`; `nypi status` and `nypi devices` show it.

### 2.4 The relay (HTTP + WebSocket, compatible with Cloudflare proxy and Cloudflare Tunnel)

`nypi relay run` (kept by the supervisor when `relay.enabled`):

```json
{ "relay": { "enabled": true, "listen": "127.0.0.1:7682", "hubKey": "<hub key fingerprint>", "maxKbpsPerLink": 2048, "trustProxy": true } }
```

The relay is a plain HTTP/1.1 + WebSocket server meant to sit behind Cloudflare proxy (orange cloud), a Cloudflare
Tunnel (`cloudflared`), or a reverse proxy such as Nginx or Caddy—so the VPS's real IP never needs to be exposed in DNS
and port 443 never collides with other sites.

Three paths on the relay:

- `GET /_nypi/hub` (WebSocket — the hub's **control connection**): the relay sends a random challenge; the hub signs
  `(challenge, epoch, instanceId)` with the hub key. The relay checks the signature against `hubKey` (without
  `hubKey`, the first hub to register claims the relay and its fingerprint is printed to compare with `nypi hub up`).
  It also tells the hub the client IP it sees (from `CF-Connecting-IP` / `X-Forwarded-For` when `trustProxy` is true,
  else the socket address) for §2.2.
- `GET /_nypi/link` (WebSocket — a machine's link: `Node`, `Talk`, or `Join`): the relay allocates a connection id and
  a one-time stream token, asks the hub over the control connection to open an outgoing WebSocket
  `GET /_nypi/data/<id>` carrying that token, and pumps raw binary WebSocket frames between the two sockets. It never
  inspects the payload inside the frames.
- `/*` (every other HTTP path, including SSE `/events` — browser board traffic): the relay asks the hub for a
  `kind: "board"` data WebSocket (`GET /_nypi/data/<id>`) and forwards the HTTP request and streamed response (with zero
  SSE buffering) to the hub's `run/board-public.sock`, passing the browser's IP (`CF-Connecting-IP` / peer address).

The relay keeps one small file: the hub key fingerprint, the current epoch, and the `instanceId` registered at that
epoch (Part 3). Nothing in it is secret.

Limits: connections per IP per minute, connections waiting for a data connection, bytes waiting per connection, and
`maxKbpsPerLink`. With no hub registered, it rejects incoming WebSockets and answers browser requests with `503`.

**How the hub and nodes dial the entry.** For a normal `host:port` entry (default `nypi.example.com:443`) or
`https://…`, clients dial `wss://…` using standard system/WebPKI root certificates (so Cloudflare's edge certificate
validates automatically). For `http://host:port` (used in loopback tests and local-only relays), clients dial `ws://…`.
The hub keeps its control WebSocket open whenever the entry names another host, reconnecting with backoff.

Setup on the VPS (also an exec-only node), printed by `nypi hub up`:

```bash
nypi relay up --hub-key <fingerprint>
nypi node up        # admit it as exec-only
```

Then point Cloudflare Tunnel (`cloudflared`) or an origin reverse proxy for `nypi.example.com` to `http://127.0.0.1:7682`
(with WebSocket enabled). With Cloudflare Tunnel, the VPS opens no inbound ports at all and its IP stays hidden.

### 2.5 End-to-end inner TLS 1.3 inside relayed links

- **Direct links** (`hub.listen`, default `0.0.0.0:7681` and `[::]:7681`): devices run pinned mutual TLS 1.3 directly
  over TCP—no WebSocket, no relay, no Cloudflare.
- **Relayed links** (`wss://<entry>/_nypi/link`): both the device and the hub wrap the binary WebSocket frames in a
  small `AsyncRead + AsyncWrite` byte stream adapter and run the **exact same pinned mutual TLS 1.3 handshake**
  (`hub-id.key` ⇄ `device-id.key`, RFC 5705 exporter confirmation code for `Join`, and `daemon/remote.rs`) *inside* the
  WebSocket stream.

Because the outer HTTPS certificate for `nypi.example.com` is served by Cloudflare (or the reverse proxy), NyPi needs
no ACME / Let's Encrypt client and no privileged port binding on the VPS. Meanwhile, neither Cloudflare nor the VPS
relay can read any device traffic (`Node`, `Talk`, `Join`, standby copies, or secrets) or impersonate the hub or a
device.

### 2.6 The board at the entry, admitted like a machine

- The hub hands relayed browser traffic to the board over a dedicated local socket (`run/board-public.sock`). On that
  socket the board **never** accepts the local token: every request needs a **browser session**.
- A browser without one gets a page asking to be admitted, showing a six-digit code tied to that pending session; the
  hub sends a `join_request` with `kind: browser` (user agent, IP) to the places of §1.4. Admitted, the page continues
  and the board sets a long-lived cookie (`HttpOnly`, `Secure`, `SameSite=Strict`) whose hash is kept in the register of
  peers as a `Personal` browser. The limits of §1.3 apply. `nypi devices remove <name>` ends a session at once.
- Browsers reach the board through the entry (a phone on the same local network can still use the local board link,
  `board.local`, as today). Every answer goes through `Board::scrub`.

### 2.7 Tests for Part 2

- Over loopback with two simulated networks: a node on the hub's network links directly (found by mDNS or a candidate)
  and never through the relay; a node elsewhere links through the HTTP/WebSocket relay with inner pinned TLS 1.3, then
  moves to a direct candidate when one becomes reachable, with a call in flight finishing across the switch; a candidate
  presenting another key is refused; among two hubs with the right key the higher epoch wins.
- End-to-end inner TLS through the relay: a join over `/_nypi/link` computes the same six-digit RFC 5705 code on both
  ends, and a relay that swaps a key or inspects frames sees only ciphertext and cannot match the code.
- The relay refuses a control WebSocket signed by another key; with no hub registered it rejects `/_nypi/link` and
  answers `503` to browsers; per-IP and bandwidth caps hold; `CF-Connecting-IP` is forwarded when `trustProxy` is on.
- Public board over the relay: a browser request without a session gets only the admission page (never `/api/*` data);
  after admission the cookie works including live SSE `/events`, every answer is scrubbed, and `nypi devices remove`
  ends the session.

---

## Part 3 (Decision D95): Moving the Hub, With Exactly One Hub at a Time

### 3.1 What moves and what stays

The hub's identity is its key (`hub-id.key`); it moves with the hub, so machines keep trusting the same fingerprint and
the entry does not change. No machine joins again after a move.

The **hub state** is everything in the NyPi home except what belongs to one machine:

- carried: `settings.json`, `models.json`, `auth.json`, `AGENTS.md`, `goals.md` and its history, `agents/`,
  `specialists/`, `skills/`, `sessions/`, `chat/`, `principal/`, `stores/`, `vault/`, `audit/`, `reflections/`,
  `devices/` (peers and browser sessions), `hub-id.key`, `secrets.enc` **and the secret
  store's master key**, the Telegram pairing file, the board's token, and every file `envFiles` names. Secrets move only
  to `personal` machines and only over the pinned TLS link (decision of the user);
- not carried: `run/`, `logs/`, `bin/`, `models/` (each machine downloads its own), `node/` (its call journal),
  `device-id.key`, `instance.json`, `standby/`.

Env files outside the NyPi home (for example `~/.env`) are written on the receiving machine as
`~/.nypi/env/<file name>`, and the carried settings' `envFiles` point to those copies; files the receiving machine
already listed are kept after them. Nothing outside the NyPi home is overwritten. The master key goes into the
receiving machine's keychain when it has one, else its key file, as `nypi secrets` stores it. It never appears in a log,
a prompt, or the board.

### 3.2 Standby copies on personal machines

- The hub keeps every linked `personal` node's standby copy current: every `hub.standbyMinutes` (default 5) and after
  each decision of the Principal, at most once a minute.
- The copy goes to `~/.nypi/standby/` (in `Protected`; no tool reads it), as a difference: for append-only JSONL files
  the bytes after the length the node already has; for other files the whole file when its length or SHA-256 differs;
  deleted files are deleted. Each batch ends with a manifest and the hub's epoch; the node applies a batch only when
  every file checks out.
- Standby batches go over direct links. Over a relayed link they are sent at most every 30 minutes and never above half
  of the relay's per-link cap.
- `exec-only` nodes never receive a copy. `nypi status` on a node shows how old its copy is.

### 3.3 `nypi hub here`: a planned move

Run on a linked `personal` node:

1. The hub stops taking new work: it pauses the scheduler and idle passes, lets the Principal's active activation finish
   (up to `hub.handoverWaitSeconds`, default 60; then it is suspended and kept), and stops running tasks at their next
   tool boundary. They stay in the ledger as interrupted, and the new hub resumes them as after a restart (D22: a cut-off
   call is reported as interrupted, never run again).
2. The hub sends a final difference on top of the standby copy, ending with its epoch `N`.
3. The target checks the copy and moves it into place as its live NyPi home, keeping its own `device-id.key`,
   `instance.json`, `node/`, and `models/`.
4. The old hub writes `instance.json` (`mode: node`, `epoch: N+1`), tells every linked node `moved { epoch: N+1,
   direct: <the target's candidates> }`, closes its relay control connection, stops its daemon and board, and only then
   answers the target `released { epoch: N+1 }`. Its supervisor now runs it as a node.
5. The target writes `instance.json` (`mode: hub`, `epoch: N+1`); its supervisor starts the daemon, which registers with
   the relay at epoch `N+1` and starts announcing itself. Nodes link to it directly when they can, else through the
   entry.

If anything fails before `released`, the old hub resumes at epoch `N` and the target discards its copy: the move did
not happen.

### 3.4 `nypi hub here --force`: taking over from a hub that is gone

- Refused unless this machine is `personal`, has a standby copy, and the hub has not answered for at least
  `hub.forceAfterSeconds` (default 120).
- It says how old the standby copy is and that anything after it is lost, and asks to confirm (`--yes` skips it).
- It promotes the standby copy at epoch `known + 1`; its daemon registers with the relay at that epoch.

### 3.5 Exactly one hub

- **The relay decides.** It accepts a registration only when the signed epoch is greater than the one it holds, or
  equal to it from the same `instanceId`. A registration at a higher epoch closes the old control connection. Its small
  file keeps the decision across restarts.
- **A hub that is refused steps down.** A daemon whose registration the relay refuses because of a higher epoch, or that
  hears of a higher epoch from a node or by mDNS, does not start work (or stops at once), writes `instance.json`
  (`mode: node`, the higher epoch), moves its live state aside to `~/.nypi/diverged/<old epoch>-<time>/` so nothing is
  lost, and tells the user through the new hub's outbox where it is. There is no automatic merge.
- **Nodes check too.** Every `Welcome` and mDNS record carries the epoch; a node refuses a hub whose epoch is lower than
  the one it knows.

### 3.6 Tests for Part 3 (several homes and a relay over loopback)

- A planned move carries sessions, the vault, the ledger, goals, secrets, and the hub key; afterwards
  the old home is a node at epoch 2, the new home is the hub at epoch 2 and registered with the relay, a third node
  reaches it directly or through the entry without joining again, and only one daemon takes work at every point.
- A move that fails before `released` leaves the old hub working at epoch 1 and the target unchanged.
- A forced takeover, then the old hub coming back: the relay refuses it, it becomes a node, and its state is kept under
  `diverged/`. The same when the relay is unreachable and the old hub learns of the higher epoch from a node.
- The relay refuses a lower epoch and an equal epoch from another `instanceId`.
- Standby differences (tails appended, changed files replaced, deletions, a bad checksum not applied); an exec-only
  node never receives a copy; relayed standby batches respect their interval and rate.
- Carried env files land under `~/.nypi/env/`, nothing outside the NyPi home is written, and the master key appears in
  no log line.

---

## Part 4 (Decision D96): The Hub's Commands From Any Personal Machine

The stores (goals, memory, tasks, schedules, secrets, specialists, the scorecard, notices) are the hub's. Today these
commands read the local `~/.nypi`, which on a node is the wrong place.

- On the hub, nothing changes.
- On a `personal` node, these commands are carried over a `Talk` link as a typed `HubCommand::Admin { argv }` and run by
  the hub, in process, with the same code they run with on the hub; their output comes back as text, and their exit code
  is the hub's: `tasks`, `goals`, `memory`, `portrait`, `schedule`, `specialists`, `reflections`, `dmn`, `rate` (one
  verdict at a time; the walk-through asks on the node and sends each answer), `scorecard`, `notices`, `trace`, `eval
  --list`, `devices`.
- `nypi secrets set` on a personal node reads the value on the node (hidden input), sends it over the pinned TLS link,
  and the hub stores it; the value never appears in output, logs, or the audit record. `secrets list` and `remove` run on
  the hub like the others.
- Commands that are about this machine run locally on any machine: `status`, `node up|down|leave`, `relay up|down`,
  `hub here`, `models`, `trust`, `eval` (a benchmark run uses this machine's keys if it has any, else says it needs the
  hub's: run it there).
- On an `exec-only` node, the hub's commands are refused with a short reason. The hub refuses `Admin` from any peer that
  is not `Personal` too (`daemon/remote.rs`).
- An allowlist in `crates/nypi-hub/src/protocol.rs` names the commands `Admin` may carry; anything else is refused.

Tests: on a personal node, `goals`, `tasks`, and `secrets set` act on the hub's stores (the secret is usable by
`http_request` on the hub and appears in no log or output); on an exec-only node they are refused; a command outside the
allowlist is refused.

---

## Documents (in each part's commit)

- `docs/devices.md` and `docs/architecture.md` (§3–§4, decisions `D93`–`D96`): two instances and one supervisor, the
  default entry, joining with a confirmation code, trust and purpose instead of roles, direct candidates, mDNS, the
  HTTP/WebSocket relay with inner pinned TLS 1.3 (compatible with Cloudflare proxy and Cloudflare Tunnel) and its
  limits, browser sessions, the hub state and standby copies, moves, epochs, and the hub's commands from personal
  machines.
- `docs/usage.md`: the command table above, and one worked example: a Mac mini running `nypi hub up`, the VPS running
  `nypi relay up --hub-key …` behind Cloudflare Tunnel or reverse proxy and `nypi node up` (admitted as exec-only), a
  MacBook on the same network running `nypi node up` (admitted as the user's computer, linked directly), the MacBook
  leaving home (relayed through `wss://`, then direct when possible), the phone opening `https://nypi.example.com`, then
  a move to the MacBook and back.
- `docs/board.md`, `docs/overview.md` (Chinese summary), and `docs/roadmap.md` (a V4 table; open items: UDP hole
  punching for two machines both behind routers without port mapping, and a node-to-node path for large files).
