# Security policy

## Reporting a vulnerability

Please report security problems privately, through GitHub's private vulnerability reporting on this repository
(Security, then "Report a vulnerability"). Do not open a public issue for one. Say what is affected, how to reproduce
it, and what an attacker gains. You will get an answer within a week; a fix is released, and credited if you wish,
before the details are published.

## What NyPi enforces

NyPi runs an agent that can use your files, your shell, and the network. These are the boundaries it holds in code,
not in the prompt; a way around any of them is a vulnerability.

- **Protected paths** (`nypi-core/src/protected.rs`): nothing the agent can call writes inside NyPi's home
  (`~/.nypi`), a project's `.nypi` folder, or the supervisor's service file, or reads keys, tokens, env files, the
  secret store, standby copies, or sockets. The guard and the OS sandbox (Seatbelt, bubblewrap) share one definition.
- **Secrets** (`nypi-vault/src/secrets.rs`): a secret's value never reaches the model, the session, a tool result, the
  audit log, or the board. The agent names it by handle; the harness uses it, and the `SecretScrubber` takes it out of
  whatever comes back.
- **The user's sockets**: a connection to a socket in `~/.nypi/run` speaks as the user; the sandbox denies it to the
  agent, and the hub refuses its own children.
- **Machines** (`nypi-hub/src/daemon/remote.rs`): a machine joins only when the user confirms a six-digit code made
  from both keys and the TLS session. What its links may do is decided by the trust it was admitted with: an
  `exec-only` machine carries out calls and can neither talk to the hub nor receive its state; a `personal` one can.
- **Links**: every link between machines is TLS 1.3 with pinned keys on both ends. Through the relay the same TLS runs
  inside the WebSocket: neither the relay nor Cloudflare can read a link or stand in for the hub or a machine.
- **The board**: on the local network it needs a token; through the entry, a browser session the user admitted.
  Every answer is scrubbed of secrets.
- **Releases**: every release's `manifest.json` is signed with an Ed25519 key whose public half is built into the
  program. `nypi update` checks the signature, then the archive's size and SHA-256, then that the new binary runs and
  says the version expected, before it replaces anything; the supervisor puts the previous binary back when the new
  one does not stay up. The agent cannot run `nypi update`, and nothing it calls can write `~/.nypi/bin`.
