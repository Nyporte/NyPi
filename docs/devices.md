# NyPi across devices: one hub, a node on every device

Status: **accepted** as decision D66 (2026-11-17). Built: the hub (the hub and its supervisor, D64) with this
computer's node in its process; Telegram and the inbound webhook as channels of the hub (D66, D81, D91); terminals,
`nypi -p`, the full-screen UI, and the board as interfaces (D67); one conversation with a focus per message (D79);
`nypi node` on other computers, paired over the user's network with TLS and a role fixed per key (D68, D80); a
journal on each node so a call outlives a dropped link (D74); a device folder's context files, skills, native
specialists (D80, D83), and safe settings (D91) for the work done there; and devices on the board (D74, D80).
Since D93 a machine is the hub or a node as `instance.json` says, one supervisor runs either, and machines join with
`nypi node up` and a six-digit code the user confirms (section 4). Since D94 a node links to the hub directly when it
can and through a relay behind the entry otherwise, and a browser reaches the board through the entry (section 4).
Since D95 the hub can move to any personal machine, which keeps a standby copy of its state, with exactly one hub at a
time by its epoch (step 7). Since D96 the hub's commands (`nypi tasks`, `goals`, `secrets`, and the others that work
on its stores) run on the hub when typed on a personal node. Since D98 every machine updates from the public
repository's signed releases, and `nypi update --all` on the hub updates the nodes one by one and the hub last, each
node checking the release itself. Not built: the nodes on phones (section 9; steps 4 and 5 of section 14) and specialists as
jobs on a node. Nothing has yet run between two real machines: the tests pair and link over loopback.
Serves principles A5 (priority response), A6 (parallelism), B4 (time leverage), B6 (the action chain never breaks
silently), and requirement 5 (the agent cannot modify the Inspector).

## 1. What must be true

The user uses NyPi from several terminals and several devices at once, phones included, and it is one Lucia
everywhere.

1. **One self.** Her memory, her picture of the user, her own lessons, and her goals have one current version.
2. **One account of the work.** One ledger knows every task, wherever it runs, so nothing stops unseen.
3. **Acts once.** A reminder fires once, a message gets one reply, an approval is asked once.
4. **Works where the thing is.** A task about a folder on the laptop changes that folder on the laptop; a task
   about an app on the phone operates that app on the phone.
5. **Reachable from anywhere.** Any terminal, the phone, and the board talk to the same Lucia and see the same work.
6. **Each device defends itself.** What may be touched on a device is decided on that device.

Points 1 to 3 need a single place that decides. Point 4 needs a presence on every device. That is the whole shape.

## 2. The parts

| Part | Count | Process | What it is |
|---|---|---|---|
| **Hub** | one | `nypi hub run` (kept by the supervisor after `nypi hub up`) | Lucia: every model call, every conversation, all state, the schedules, the outbox, the message channels |
| **Node** | one per device | `nypi node`, or the NyPi app on a phone | Carries out the hub's calls on that device, under that device's own Inspector |
| **Interface** | any number | `nypi`, `nypi attach`, `nypi send`, the phone app, the board's page | Where the user talks, watches, and answers |
| **Channel** | any number, inside the hub | part of the hub | A message service the user already uses, such as Telegram |
| **Board** | one | `nypi board` | Watches the hub and every node; stays up when the hub is down |
| **Relay** | at most one, behind the entry | `nypi relay run` (kept by the supervisor after `nypi relay up`) | Passes relayed links and browsers to the hub; reads neither |
| **Supervisor** | one per machine that runs a service | `nypi supervisor` | Keeps the local hub, node, relay, and board alive |

On the machine the hub runs on, the hub has a node of its own, in the same process: one machine needs no second
process.

A terminal on a laptop is an interface and uses that laptop's node: the user types there, and the calls of that
conversation run there.

```
                 phone (app, Telegram)         browser
                         │                        │
                         │                  ┌─────┴─────┐
                         │                  │   board   │◄──── states from the hub and every node
                         ▼                  └───────────┘
  terminal ─────► ┌──────────────────────────────┐ ◄───── terminal
  (laptop)        │              HUB             │        (desktop)
                  │ agent loop · threads · memory│
                  │ ledger · schedules · outbox  │────► model providers
                  │ channels (Telegram, ...)     │
                  └──────┬──────────┬─────────┬──┘
                   calls │          │         │ calls
                         ▼          ▼         ▼
                  ┌──────────┐ ┌──────────┐ ┌──────────┐
                  │  node:   │ │  node:   │ │  node:   │
                  │  laptop  │ │  desktop │ │  phone   │
                  │ files,   │ │ files,   │ │ apps,    │
                  │ shell    │ │ shell    │ │ sensors  │
                  └──────────┘ └──────────┘ └──────────┘

  Terminals, the phone app, and the board's page are interfaces; Telegram is a channel.
```

## 3. What lives where

| Kept by the hub | Kept by each node |
|---|---|
| The session of the one conversation (section 5), and those of jobs | The device's keys for its links to the hub |
| Memory, portrait, self-prompt, goals | Its Inspector's rules: what is protected here, the sandbox, calls this device always refuses |
| The vault, the asset registry, the secret store (D77): a secret is used by the hub, never sent to a node | Nothing of these |
| Task ledger, schedules, outbox | Its job journal: calls it ran or is running, and results not yet acknowledged |
| The task pool (D78): every task's runtime, its brief, its workspace in the vault, the observer, and the review of its report. A task started beside another works at the same place as its conversation | Nothing of these: a node sees only the calls a task makes |
| The audit log | A local audit journal of what was done on this device |
| Model keys and the model list | The trust decisions for folders on this device (D61) |
| Global settings; the register of paired devices and what each last reported (section 8) | Nothing of Lucia's self, and no model keys |

The hub is the only writer of everything in the left column. That is what makes the append-only files of today
(ledger, memory, outbox, audit) stay correct without any merging, and it is why a lost device leaks no model keys.

A folder's own configuration (`.nypi/settings.json`, skills, specialists) stays in that folder on that device. The
node offers it to the hub when a thread starts there, and only for folders the user has trusted on that device.

## 4. Links and who may do what

Three kinds of connection reach the hub. They carry different authority, and a credential for one is useless for
another.

| Link | From | May | May not |
|---|---|---|---|
| **Interface** | A terminal or the app on a paired device, a paired Telegram chat, the board's page | Give tasks, steer, stop, answer approvals, rate results | Run anything by itself |
| **Node** | `nypi node` or the phone app on a paired device | Answer the hub's calls, report job states, offer a trusted folder's configuration | Give tasks, answer approvals, read the hub's state |
| **Board** | `nypi board` | Read states and stories; pass on the user's answers made on its page | Anything an interface may not |

- **Joining (built, decision D93).** A machine joins once, with `nypi node up`. It finds the hub on its network by
  mDNS, or at the entry the user names (there is none by default, decision D97), links
  with any key, and shows a six-digit code made from both keys and the TLS session; the hub records a join request
  and asks the user wherever approvals go (a terminal attached to the hub, Telegram, the board), showing the same
  code. The user admits it as `personal` (the user's own machine: it carries out calls and talks as the user) or
  `exec-only` (it carries out calls only), or refuses it. A machine has one key, and one entry in the register,
  whatever it does. `nypi devices invite` gives a one-time code for a machine nobody watches; `nypi devices remove
  <name>` forgets one. Requests are limited, expire after ten minutes, and never reach the conversation.
- **Transport (built, decision D94).** Machines dial out, so a machine behind a home router or on a mobile network
  needs no open port. Every link is TLS 1.3, authenticated on both ends with the pinned keys, and carries the same
  purposes whichever way it went:
  - **Direct**: to `hub.listen` (port 7681). A node tries, all at once, the hub that mDNS hears on its network with
    the pinned fingerprint and the hub's direct addresses it was told (each interface's private address, a port the
    user forwarded, or one the router mapped); the first handshake that shows the pinned key wins, the highest epoch
    among them, within 1.5 seconds.
  - **Relayed**: through the entry, a relay behind Cloudflare (a Tunnel or its proxy) or a reverse proxy. The relay is
    plain HTTP and WebSocket; the hub keeps a control connection to it signed with the hub key; a machine's link is a
    WebSocket the relay joins to one the hub opens, frames passed unread, and the pinned TLS runs inside it end to end,
    so neither Cloudflare nor the relay can read a link or stand in for the hub or a machine. The VPS that runs the
    relay needs no public port and its address can stay out of DNS.
  - A node linked through the relay tries the direct ways every minute and moves to one as soon as it works; calls
    under way go on, and their results reach the hub on the new link.
  - Nodes never talk to each other: "direct" means a node reaches the hub without the relay.
- **Browsers (built, decision D94).** A phone opens the entry, the relay passes it to the hub, and the hub hands it to
  the board's public socket, where only a browser session the user admitted is taken: a new browser shows a six-digit
  code and is asked about like a machine, and `nypi devices remove` ends its session at once. On the local network
  the board's own link (`board.local`) works as before.
- **Who in the hub reaches a device (decision D84).** Chat only reads there (`read`, `grep`, `find`, `ls` at the
  user's focus, refused at once when it would need an approval, stopped after 1.5 seconds), and the Principal
  never reaches a device at all: everything that acts on a device is a worker's task, under that node's rules.
  A schedule set at a device's folder keeps that focus when it fires and wakes the Principal (decision D85), so
  the task it starts for it works at that device again.
- **The board shows where each step ran (decision D86).** Chat's turns name the device and folder the user was at,
  a task's calls made on another device carry that device's name, and the System map shows chat's reads reaching
  the devices.
- **What a link may carry is decided in one place** (`daemon/remote.rs`), by the trust the machine was admitted with
  and the purpose the link names: a `node` link is read as the node protocol and nothing else, so nothing a node sends
  can be a task or an approval's answer; a `talk` link reaches the conversation, for a personal machine only.
- **Removing ends what is open.** `nypi devices remove` tells the hub, which closes the machine's live links at once
  (decision D83); a register changed by hand is noticed within seconds.
- **Programs a node starts cannot speak as the user.** On every machine its key and sockets are in what the node's
  Inspector protects, as the hub's socket is (D64).
- **The hub's own machine** keeps the user-only Unix socket of D64 for its local interfaces.

## 5. One Lucia, one conversation, and where its work is done

Lucia is one, and so is the conversation the user has with her (decision D79; before it, there was a thread per
folder). The user talks to her from several places at once, as one would with a person by phone and in a room, and
it is the same conversation everywhere.

- The hub holds **one conversation**, with one session (`sessions/primary`), open as long as the hub runs.
- An interface brings its **ambient focus**: the folder its user has open, and the device that folder is on when
  it is not the hub's computer. A terminal opened in a folder says that folder; `--on mini --at /srv/app` says a
  folder on a device; Telegram and anything that names nothing are at the hub's own folder.
- What a message asks for is done at its focus: the run's file and shell calls go to the node of that device, in
  that folder. The focus comes with every message, so two terminals in two folders talk in one conversation and
  each has its work done where it is.
- Every interface sees every record of the conversation (D64), whoever asked and from where.
- One call may go to another device: the file and shell tools take a `device`, and `on_device` calls a tool only
  some device has (a phone reading a code from a message for a task on the laptop). The prompt's `<devices>`
  section lists the devices and what each offers.
- A call that goes elsewhere is judged for where it goes: by the hub's Inspector with the target's folder and home,
  then by that device's own node (section 4), which has the last word.
- A folder the device's owner trusted lends its skills and native specialists to the work done there (decision D83):
  a specialist delegated to there works at that folder on that device. A specialist that is a program runs only
  where it was written, so one declared on another device is not offered, and one of the hub's is not run for a
  folder on another device.
- Work beyond the run in front of the user (a task beside it, a job a schedule started) is listed in the prompt's
  `<active_tasks>` section, from the ledger; up to `tasks.maxConcurrent` tasks work at once (D78).
- A task's record says where it worked, so a task taken up again and a schedule that fires go back to that folder
  on that device.

## 6. A task from start to end

1. An interface or a channel sends a message into a thread. The hub writes the task in the ledger, with its thread and
   place, before anything runs.
2. The hub builds the prompt: the layers, recalled memories, the elsewhere section, the folder's configuration the
   node offered, and the tools the place's node says it has (section 8).
3. The agent loop runs in the hub. Model calls go from the hub to the provider.
4. Each call goes through the hub's Inspector first, which asks the user when the user's rules say so, on every
   interface attached to the thread and through the channels. The first answer counts.
5. The call goes to the node. The node's Inspector checks it against that device's rules and either carries it out
   or refuses, and a refusal is a result the model reads.
6. Output streams back; the result goes into the session and the ledger as today.
7. The run ends with its status line. Attached interfaces see it; with none attached it goes to the outbox.

The hub asks the user; a node never does. A node may only refuse. That keeps approvals in one place and still lets
each device have the last word about itself.

## 7. Calls on a node

- **Calls.** `call` (id, task, tool, arguments, folder, limit) from the hub; `started`, `output` (as it is
  produced), `finished` (result) or `refused` (why) from the node; `cancel` from the hub.
- **At most once.** A call's id is new for every call. A node writes the id to its journal before it carries out
  the call and never carries out an id twice.
- **Durable jobs.** A call that has started keeps running when the link drops. The node buffers its output and
  keeps the result until the hub acknowledges it.
- **Reattach.** After a reconnect the hub asks about the calls it still waits for; the node answers running,
  finished with the result, or unknown.
- **Own protocol, not MCP.** NyPi can already call MCP servers, and a node could be one. It is not, because MCP has
  no durable jobs, no reattach, and no pairing, and those are what make a sleeping laptop or a phone in a pocket
  harmless.

**Delegated specialists.** A long piece of autonomous work is not driven call by call. Lucia delegates it to a
specialist on that device (Codex, Claude Code), which the node starts as a durable job; its loop runs beside the
files, its progress streams back as output, and its report is the result. A native specialist is a NyPi loop, so it
runs in the hub and uses the place's node like any thread.

## 8. Nodes differ: what each reports, and what the hub does with it

A laptop has a shell and a file system. A phone has apps, a screen, and sensors. So a node does not implement a fixed
list of tools: when it connects it **declares its tools**, each with a name, a description, and its arguments, and
the hub offers the model exactly those for a thread at that place.

- A computer's node declares the tools of today: `read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`, the command
  tools from settings, and the specialists installed there.
- A phone's node declares what that phone allows (section 9).
- The tools that are Lucia's own (memory, schedule, goals, delegate, her self-prompt) stay in the hub, on every thread.
- The hub's approval rules name tools by pattern, as today, so "always ask before anything on the phone that sends
  or pays" is one rule.

**The hub keeps a register of its nodes.** Tools are one part of what a node reports. For every node the hub
knows:

| What | For example | When the node sends it |
|---|---|---|
| Identity | The device's name, its kind (computer, Android phone, iPhone), its system and NyPi version | At pairing and on every connect |
| Presence | Connected since when, or last seen when; how good the link is | With every heartbeat |
| Resources | On mains or on battery and how full, free disk and memory, how busy it is, how many calls it takes at once | On connect, and when one changes by much |
| Environment | The folders it serves and which of them are trusted, its shell, the programs installed that matter (compilers, Codex, Claude Code); on a phone, whether it is locked and which apps it may operate | On connect, and when it changes |
| Tools | Every tool it declares, with its arguments | On connect, and when the list changes |
| Work | The calls it is running, and the finished ones the hub has not acknowledged | As it happens |

A node that is away keeps its last reported state in the register, with the time it was reported.

What the hub does with the register:

- **Chooses where work runs** when the user has not said: the device the interface is on; else a connected node
  that has the tool the task needs; and not, for heavy work, a phone on a low battery.
- **Tells Lucia.** Every thread's prompt carries a short devices section beside the elsewhere section: which devices
  there are, which are connected, and what each can do. So she can say "the laptop is away; shall I wait, or do
  this on the desktop?" and not find out by failing.
- **Shows the user**: `nypi devices`, and the devices region of the board's map.
- **Keeps the chain unbroken**: a presence that lapses starts the rules of section 12.

What the hub may do to a node, and what it may not:

- It may send calls, cancel them, ask for their state, send less than the node would take, and end the pairing.
- It may not change the node's Inspector rules, raise its limits, read anything outside a call, or have it carry
  out what it refused. Those are changed on the device, by the user. The hub manages the work; each device keeps
  the last word about itself (point 6 of section 1).

This also sets how the code is cut. The node is a small library with no model, no session, and no provider code, so
it builds for phones: wire types in a crate of their own, as the board's are, and a node crate holding the link,
pairing, the journal, and the table of tools. The desktop tools register into it from where they are today; a phone
app is a thin native shell around the same Rust library.

## 9. Nodes on phones

The node runs natively on a phone, as an app that is also an interface (talk, watch, approve, get notices). What it can
do there is set by the phone's system, and the two systems differ a great deal.

| | Android | iPhone |
|---|---|---|
| Operate other apps: read the screen, tap, type, scroll | Yes, once the user turns on the app's accessibility service | No. Apple lets no app do this. An iPhone's other apps are operated from a Mac's node through developer tooling, or through the Shortcuts actions those apps offer |
| See the screen | Yes: the screen's element tree, and screenshots on Android 11 and later | Its own screens only |
| Stay connected with the app closed | Yes, as a foreground service with a standing notification, at a cost in battery; some makers' power saving has to be told to leave it alone | No. Connected while the app is open; otherwise woken for a short time by a push from the hub |
| What it offers of its own | Notifications in and out, clipboard, sharing into and out of apps, files it is given, location, camera on request | Running a Shortcut, calendar, reminders, contacts, photos, files it is given, location, notifications |
| Getting it onto the phone | An installable package, no store needed | A build signed with an Apple developer account; pushes go through Apple's service |

What follows from this:

- **A phone is often away.** A call to a phone waits for the phone, for a limited time, then the task ends Blocked
  and the user is told: the same rule as a sleeping laptop (section 12), reached far more often.
- **The scenario "operate this app on my phone"** runs natively on Android. On an iPhone it runs natively only as
  far as Shortcuts reach; beyond that the phone is operated from a Mac, and a simulator always is.
- **The phone's Inspector is stricter by default.** Apps the node never touches (banking, password managers, the
  system's settings), nothing while the phone is locked, and a visible mark on the screen while Lucia is operating it.
- **Never past a check meant for a person**: a CAPTCHA, a biometric prompt, a payment confirmation. The node stops
  and the task ends Blocked.
- The phone app is written as a thin native shell (Kotlin on Android, Swift on iPhone) around the Rust node library.

## 10. Channels and notices

- A **channel** is a message service inside the hub: it turns what comes in into messages for the conversation,
  sends replies, and carries approval questions and notices (one trait, decision D81). Telegram is the first, and
  two-way. The **inbound webhook** is the second, and one-way: other programs of the user's hand in notes, links,
  and files, which are saved to the vault's inbox and filed by a task. It authenticates callers with a secret from
  the secret store, and what it takes in is material, never the user's words.
- The **outbox** stays as it is (D57): written first, sent until something takes it. What can take a notice grows:
  an attached interface, a channel, and a node, which can show a notification on the device the user is at.

## 11. The board

The board is a service of its own, kept alive by the supervisor on the hub's machine.

- It must outlive the hub, because it is what shows that the hub is down.
- It listens on the network, so it holds no model keys and no tools.
- Each process of the hub reports to it on one link, every runtime on a channel of its own (decision D80); the
  board also reads the hub's heartbeat, its ledger, and its registers itself, so a silent hub shows as one.
- The home screen shows the whole hub: what needs the user, what runs, the tree of tasks, the devices (connected
  or away, what each offers and is paired as), and what last changed in the vault.

## 12. When something fails

| What happens | What NyPi does | What the user is told |
|---|---|---|
| A node goes away during a call (lid closed, phone in a pocket, network gone) | The job keeps running on the device if the device is up. The hub waits a grace period for the link | Nothing yet |
| It stays away past the grace period | The call is answered as interrupted, as a cut-off call is today; the run ends Blocked, naming the device and the step | A notice: the laptop is away, the task stopped at this step |
| The node comes back | The hub collects finished results and offers to go on from where it stopped | The thread shows it; a notice if nobody is attached |
| A task arrives for a device that is not connected | The task waits for the device, for a limited time, then ends Blocked | A notice at once that it is waiting, and why |
| The hub stops or hangs | Its supervisor restarts it (D64). Nodes finish what they are running and hold the results. Interfaces say at once that the hub is unreachable | The board shows the hub down; the supervisor's notice |
| The hub's machine is gone for good | The user starts the hub on another machine from the latest copy of its state, with the same keys, so devices need no new pairing | This is a manual step |
| Two interfaces answer the same approval | The first answer counts; the other is told it was already answered | As today |
| A device is lost | `nypi devices revoke` ends its links; it held no model keys and none of Lucia's state | |

A task belongs to the hub's process, as tasks belong to a process today, so the ledger's liveness rule stays. What is
new is a lease per call on a node, renewed by the node's heartbeat.

## 13. What changes in earlier decisions

| Decision | Today | Becomes |
|---|---|---|
| D22 task liveness | The owning process's pid | The hub's pid for the task, plus a lease for each call on a node |
| D23, D36 rounds behind file locks in every front end | Any long-lived process may do the watchdog's and scheduler's rounds | Only the hub does them; the locks go once terminals no longer hold runtimes. The supervisor's rounds stay as the fallback while the hub is down |
| D36 fired prompts as separate processes | `nypi schedule fire` | A message from the scheduler into a thread of the hub (done, D67 part 3) |
| D56 board link | Every runtime links over a Unix socket | The hub links, and reports for its nodes |
| D61 project trust | One store on one machine | One store per device, read by that device's node |
| D62 what tools may not touch | One `Protected` | One per device, which also covers that device's keys and sockets |
| D64 the hub's socket | One conversation, local interfaces | One conversation with a focus per message (D79); a network listener for paired devices beside the local socket (D80) |
| The tool list | Fixed in the runtime | The hub's own tools plus what the place's node declares |

## 14. Order of building

Each step is useful by itself, and the first two need no networking.

1. **The message service moves into the hub** (done). Telegram becomes a channel of the hub. The supervisor starts and
   keeps the board. After this, the supervisor (`nypi hub up`) is the only thing to install.
2. **Terminals become interfaces** (done; the threads per folder became one conversation in D79). Plain `nypi` and `nypi -p` attach to the hub when one is
   running, and `--standalone` keeps today's behaviour. Fired schedules, goal reviews, and idle passes become
   messages to the hub. The rounds run only in the hub. After this there is one Lucia on this machine.
3. **Nodes on computers** (done). The node library and its wire types; tools declared by the node, with the local node in
   the hub's process; then `nypi node`, pairing, the network listener, durable jobs, and reattach. After this a
   second computer works.

   Built (decisions D68, D74, D80, D93, D94): `nypi node up` joins with the user's approval and `nypi node run` links
   over TLS to the hub directly when it can and through the relay at the entry otherwise, moving to a direct way when
   one works; the hub's own node is on its Unix socket for nodes
   (`~/.nypi/run/nodes.sock`). A message's focus can be `(device, folder)`: the hub keeps the conversation, and its file and shell tools
   are the ones that device's node declared. The node applies its own
   machine's sandbox (Seatbelt on a Mac, bubblewrap on Linux, decision D76), protected paths, and guard, and
   refuses by them whatever the hub asked; what it runs starts without the keys in that machine's environment. A call whose node
   goes away is waited for up to a lease (120 s) while the node keeps it running and journals its result, which it
   hands over on its next link (decision D74).
4. **A node on Android.** The app as node and interface: the accessibility service, the screen's tree and
   screenshots, taps and text, notifications, and the stricter Inspector. After this "operate this app on my phone"
   works on Android.
5. **A node on iPhone.** The app as interface first (talk, approve, notices), then the node within what Apple allows:
   Shortcuts, calendar, reminders, files, location. Pushes from the hub to wake it.
6. **The rest of a device.** Specialists as jobs on a node, notices shown on a device, devices on the board's map
   (the last is done, D74).
7. **A hub that can move** (done, decision D95). Every personal node keeps a standby copy of the hub's state, kept
   current by differences (every five minutes over a direct link, and soon after a decision of the Principal; every
   half hour over the relay, at half its rate); exec-only nodes never get one. `nypi hub here` on a personal node is a
   planned move: the hub stops its work, sends the last difference, becomes a node of the new hub at the next epoch,
   and only then lets it start; a move that fails changes nothing. `nypi hub here --force` takes over from the copy
   when the hub has been gone a while. The hub key moves with the hub, so no machine joins again, and the epoch
   decides which is the hub: the relay takes only the highest, nodes refuse a lower one, and a hub that hears of a
   higher one (from the relay, a node, or mDNS) steps down, keeping its state under `diverged/`.

## 15. What is not decided

These are the user's to settle; none blocks steps 1 and 2.

- **Where the hub lives in the long run**: this Mac, or a small machine that is always on.
- **What may be stored on the hub.** Everything Lucia reads on a device passes through the hub and stays in its
  sessions. For a work laptop, or for a phone's screens, that may need a rule (a device whose content is never
  kept, or kept only as summaries).
- **Working without the hub.** A device cannot use Lucia when the hub is unreachable. A detached mode (a separate,
  clearly marked session on the device whose notes go to the hub later) can be added; a device that keeps full
  capability offline would need model keys and is the design of equal peers this decision turns down.
- **Devices the user does not fully control** (a shared server): whether a node may run there at all.
- **The iPhone app**: whether an Apple developer account and pushes through Apple's service are acceptable, or the
  iPhone node works only while the app is open.

## 16. Why not equal peers

Every device being a full NyPi that can think and act was the alternative. It was turned down for one reason:
acting once needs one decider. With several, two peers can answer the same message or fire the same reminder, and the
cure is to elect a leader, which is this design plus the machinery for elections and for merging what each peer wrote
meanwhile. Memory would merge (it is a log of events); the self-prompt, a running conversation, and a promise would
not. Several equal peers would be several Lucias sharing a diary. Revisit when the user needs full capability with
the hub unreachable, has no machine that can stay on, or shares NyPi with other people.
