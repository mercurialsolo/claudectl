# Relay & Hive Mind

Share learnings, delegate tasks, and collaborate across machines — all peer-to-peer, all local-first.

## What is it?

The relay connects two or more claudectl instances over TCP. Once connected, they can:

- **Share brain knowledge** — patterns your brain learns ("always approve `cargo test`") propagate to [hive members](#what-propagation-actually-does-today), in both directions
- **Delegate tasks** — offload work to a remote machine running Claude Code
- **Synchronize insights** — friction patterns, error loops, and accuracy data merge across the network

Every instance stays sovereign. Your local preferences always override peer knowledge. No cloud, no central server.

## Quick Start: Connect Two Machines

### Step 1: Install

```bash
cargo install claudectl
```

`relay` and `hive` are both in the default feature set, so a plain install has cross-machine networking. A build with `--no-default-features --features hive` keeps hive local: knowledge is distilled, archived, and used by the brain, but never synced to peers.

### Step 2: Generate an invite

On Machine A:

```bash
claudectl relay invite
```

Output:

```
Your identity: laptop-a3f2

  RELAY CODE:  YEK-AGA-YHK-QAA-BM

  INVITE LINK: cctl://laptop-a3f2@192.168.1.50:9847/k/a3f29b1cd4e5f678

Share any of the above with your peer. They run:

  claudectl relay join YEK-AGA-YHK-QAA-BM
  claudectl relay join cctl://laptop-a3f2@192.168.1.50:9847/k/a3f29b1cd4e5f678
```

### Step 3: Join from Machine B

```bash
claudectl relay join YEK-AGA-YHK-QAA-BM
```

That's it. Both machines are paired and connected.

### Step 4: Start the relay server

On Machine A (the one that generated the invite):

```bash
claudectl relay serve
```

Machine B connects:

```bash
claudectl relay connect 192.168.1.50:9847
```

Leave both running. The relay is what carries session state between machines, so
the cluster view below is only as live as the relays behind it.

## Step 5: See the whole cluster

Once relays are up on both machines, every session on either one shows up on both:

```bash
claudectl relay fleet
```

```
Fleet: 5 session(s) across 2 machine(s)

MACHINE              PROJECT                      STATUS         COST
────────────────────────────────────────────────────────────────────────
barrys-mac-74371c…*  cabal-1                      Waiting        $137.45
barrys-mac-74371c…*  claudectl                    Processing     $31.95
barrys-mac-74371c…*  staffai                      Waiting        $17.59
mac-mini-9f2a1b      claudectl                    Processing     $4.21
mac-mini-9f2a1b      nightly-bench                Needs Input    $0.87

* = this machine
```

Remote sessions also appear in the dashboard (`claudectl`) and the plain list
(`claudectl -l`), prefixed with the machine they're running on:

```
5101    [mac-mini-9f2a1b] claudec… Processing   -        $4.2
```

They're read-only from here — the keys that send input or terminate a session
act on local sessions only. To put work on another machine, delegate it:

```bash
claudectl relay delegate mac-mini-9f2a1b "run the full test suite" --cwd ~/code/claudectl
claudectl relay status                      # how delegated tasks are doing
claudectl relay interrupt --peer mac-mini-9f2a1b task_123 stop
```

### How the cluster view works

Each relay advertises its own sessions on every heartbeat (default 30s) and
writes what its peers report to `~/.claudectl/relay/fleet.json`. The dashboard
and `relay fleet` read that file, so no port or token is needed for the local
view.

Consequences worth knowing:

- **A relay must be running on each machine.** `relay fleet` with no snapshot
  falls back to local sessions and tells you so. On macOS,
  [`relay install-agent`](#keeping-the-relay-alive) keeps one running across
  logout and reboot; otherwise `relay serve` is a foreground process that stops
  when its terminal does.
- **A peer that stops reporting disappears** after 90s rather than lingering as
  a stale row, and the whole snapshot is ignored after 120s (which is how a
  stopped relay shows up as "no snapshot" instead of a frozen cluster).
- **Remote rows carry what the heartbeat carried**: project, status, cost,
  tokens, elapsed. Live CPU and memory stay local-only.

The coordinator's HTTP API serves the same unified view to anything that can
speak HTTP. It binds `127.0.0.1` and starts only when both `--http-port` and
`--auth-token` are given:

```bash
claudectl relay serve --http-port 9876 --auth-token secret
curl -H "Authorization: Bearer secret" http://localhost:9876/api/sessions
```

Reaching that API from a dashboard on *another* machine is a second, deliberate
step, because the API is plaintext HTTP/1.1 and the bearer token crosses the
wire in the clear. Forward the loopback port instead of widening the listener —
from the machine running the dashboard:

```bash
ssh -L 9876:127.0.0.1:9876 user@machine-a
curl -H "Authorization: Bearer secret" http://localhost:9876/api/sessions
```

Cloudflare Tunnel and Tailscale Funnel do the same job without SSH. You can
bind the API to every interface with `--http-addr 0.0.0.0`; it works, it prints
a warning at startup, and [Security](#security) says why you probably want the
tunnel.

## Keeping the relay alive

`relay serve` is a foreground process. Close the terminal and the relay stops, which means the cluster view on every peer goes stale — `relay fleet` falls back to local sessions and says so. On macOS, install a launchd agent instead:

```bash
claudectl relay install-agent                    # port 9847
claudectl relay install-agent --port 9850 --http-port 9876 --auth-token secret
claudectl relay agent-status
claudectl relay uninstall-agent
```

```
Installed the relay agent.

  plist:  /Users/you/Library/LaunchAgents/io.claudectl.relay.plist
  label:  io.claudectl.relay
  port:   9847
  logs:   /Users/you/.claudectl/relay/agent.out.log
          /Users/you/.claudectl/relay/agent.err.log

It is running now, starts at login, and restarts if it dies.
```

The agent starts at login (`RunAtLoad`) and is restarted if it exits (`KeepAlive`), so a crash or a kill brings it straight back. `claudectl doctor` reports whether it is running.

Details worth knowing:

- **Re-run `install-agent` to change anything.** It replaces the plist and reloads the service, so changing a port or picking up a `brew upgrade` is one command. Running it twice is harmless.
- **Uninstall never leaves an orphan.** The plist is removed even if unloading complains — an orphaned plist that keeps resurrecting a service you thought you removed is the usual way this feature goes wrong. `agent-status` also detects the reverse case (loaded, plist missing) and tells you to run `uninstall-agent`.
- **`--auth-token` lands in the plist**, under `~/Library/LaunchAgents`, readable by your user. The install output says so when you pass one.
- **The relay is killed abruptly on unload**, not asked to shut down. That is safe: `fleet.json` and the knowledge store are both written atomically, so the worst case is losing the current one-second tick rather than a torn file.

### Linux

There is no launchd, and `install-agent` says so rather than failing silently. The `systemd --user` equivalent — write `~/.config/systemd/user/claudectl-relay.service`:

```ini
[Unit]
Description=claudectl relay

[Service]
ExecStart=%h/.cargo/bin/claudectl relay serve --port 9847
Restart=always

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now claudectl-relay
loginctl enable-linger $USER     # so it survives logout
```

## Three Ways to Share a Code

Every invite generates three formats. Pick whichever fits the situation:

### Relay Code (compact, no IP visible)

```
YEK-AGA-YHK-QAA-BM
```

15 characters. Speakable over a phone call. Encodes the IP, port, and key without exposing any of them in readable form.

### Word Phrase (memorable)

```bash
claudectl relay invite --words
```

```
fur-hue-ace-bid-ice-ape-cod-elk-ace
```

9 common English words. Easier to dictate than alphanumeric codes.

### Invite Link + QR Code

```bash
claudectl relay invite --qr
```

```
cctl://laptop-a3f2@192.168.1.50:9847/k/a3f29b1cd4e5f678
```

Plus a scannable QR code in the terminal (requires `qrencode` installed).

The `join` command auto-detects the format:

```bash
claudectl relay join YEK-AGA-YHK-QAA-BM           # relay code
claudectl relay join fur-hue-ace-bid-ice-..."        # word phrase
claudectl relay join cctl://laptop-a3f2@..."         # invite link
```

## LAN Discovery

Find nearby claudectl instances without codes:

```bash
claudectl relay discover
```

```
Found 2 instance(s):

  IDENTITY             ADDRESS                  VERSION
  ────────────────────────────────────────────────────────
  laptop-a3f2          192.168.1.50:9847        v0.40.0
  ci-runner-9d1e       192.168.1.101:9847       v0.40.0
```

Discovery is **passive**: `relay serve` broadcasts a small announcement on UDP 9848 every 5 seconds, and `relay discover` listens for 6 seconds — one second longer than the interval, so every announcing peer is heard at least once. `discover` sends nothing itself.

Turn the broadcast off with:

```toml
[relay]
lan_announce = false
```

> Before v0.66.0 nothing ever sent an announcement, so `relay discover` always reported "no instances found". If you tried it on an older build and concluded LAN discovery was broken, it was.

## Hive Identity

By default a hive has **no name**. It is whatever your relay peers happen to be — the transitive closure of who you paired with. Nothing is advertised, nothing is discoverable, and that is the state every install starts in.

Naming one is what makes it possible to advertise, find and join a hive *as such*:

```bash
claudectl hive identity                      # show it, or "unnamed"
claudectl hive identity set --name barrys-hive \
  --description "Rust CLI + Claude Code practices"
claudectl hive identity clear --yes          # back to unnamed
```

Stored at `~/.claudectl/hive/identity.json`:

```json
{
  "hive_id": "hv_3a9f21",
  "name": "barrys-hive",
  "description": "Rust CLI + Claude Code practices",
  "join_policy": "invite",
  "created_ms": 1791210482180
}
```

Renaming keeps the `hive_id` and the creation time — a rename is not a new hive, and peers who know it by id keep recognising it. The name must be usable as a capability scope qualifier (`[A-Za-z0-9._-]`), because a later phase grants `hive.read:<name>`.

### Join policy

| Policy | Meaning |
|---|---|
| `invite` | A link or code is required. The default. |
| `ask` | You approve each join request. |
| `open` | Anyone who can see your LAN broadcast may join, without approval. |

`open` is allowed, but it asks first:

```bash
claudectl hive identity set --join-policy open
```

```
WARNING: join_policy = open lets ANY machine that can see your LAN broadcast
join this hive without approval. Joining a hive means receiving your distilled
preferences and insights — the patterns the brain learned from how you work. On
a shared or untrusted network (an office, a cafe, a conference) that is everyone
on it.

  `ask` gives you the same discoverability and still lets you approve each
  request. Prefer it unless you specifically want hands-off joining on a network
  you control.

Open this hive to anyone on your LAN? [y/N]
```

Two details worth knowing:

- **Consent is recorded, not just prompted.** Confirming writes `open_acknowledged_ms` into the record. `identity.json` is an ordinary file you can edit, so a prompt alone would be theatre — hand-editing `"join_policy": "open"` leaves it **not in force**, and claudectl treats it as `invite` until you run the command and confirm. `hive identity` says so plainly when that happens.
- **No terminal means `--yes` is required**, not assumed. In a script or CI, `--join-policy open` fails without it. Defaulting to yes in a pipe is how a permissive setting gets made by accident.

Moving away from `open` drops the acknowledgement, so coming back to it asks again.

### Inviting someone into the hive

```bash
claudectl hive invite              # link + relay code
claudectl hive invite --words      # also the memorable phrase
claudectl hive invite --qr         # QR of the link
```

```
Inviting to hive "barrys-hive" (hv_3a9f21), join_policy=ask
  Each join will wait for you to approve it (claudectl hive requests).

  HIVE LINK:   cctl://hive/hv_3a9f21?a=laptop-a3f2@192.168.1.50:9847&k=cbc3179ea1a8bb70&n=barrys-hive&p=ask

  RELAY CODE:  YCU-AIG-B7L-VNU-HAS-MFS-BRA
    (the code and the phrase pair them with this machine; only
     the link names the hive — either way they end up asking to join)
```

The invite carries the same address and PSK a peer invite does, because the
holder still has to reach this machine. **Only the link carries the hive id** —
the relay code and the word phrase spend all thirteen of their bytes on the
address and the key. That costs nothing: a code pairs you with the machine, and
the machine then answers for whichever hive it runs.

> The RFC sketched this link as `cctl://hive/<hive_id>?k=<psk>&n=<name>`, which
> cannot be used — it names a hive but no machine, so a holder has nothing to
> connect to. The address is carried in `a=<identity>@<host:port>`, the same pair
> the peer link already puts before its `/k/`.

`hive invite` warns if nothing is listening on the port it is about to hand out,
because an invite minted while the relay is down cannot be redeemed.

### Joining a hive

```bash
claudectl hive join cctl://hive/hv_3a9f21?a=...     # link
claudectl hive join YCU-AIG-B7L-VNU-HAS-MFS-BRA     # relay code
claudectl hive join nut-may-aim-bud-era-cow-...     # word phrase
```

All three pair with the machine and then *ask* to join. What happens next is the
host's `join_policy` to decide — holding a link is not the same as being let in:

```
Hive "barrys-hive" (hv_3a9f21), join_policy=ask
Connecting to 192.168.1.50:9847...
Paired with laptop-a3f2 (192.168.1.50:9847)
Asking to join "barrys-hive"...
Asked to join "barrys-hive" — waiting for its owner to approve. Nothing is
shared until they do.

To start exchanging knowledge, connect to the hive:

  claudectl relay connect 192.168.1.50:9847
```

That last line matters: `hive join` records membership and exits. `relay serve`
listens but does not dial out, so the joiner is the side that has to connect for
knowledge to actually move.

`claudectl hive status` always says where you stand:

```
  Hive membership: asked to join "barrys-hive" via laptop-a3f2 — awaiting owner approval
```

A link that names a *different* hive is refused, which is what stops an invite
for someone else's hive from quietly joining yours.

### Approving who gets in

With `join_policy: ask`, each request waits for you:

```bash
claudectl hive requests                        # who is waiting, and who is in
claudectl hive requests approve laptop-a3f2
claudectl hive requests deny laptop-a3f2
```

```
Hive "barrys-hive" (hv_3a9f21), join_policy=ask

1 waiting for you:

  PEER                         REQUEST                ASKED
  ────────────────────────────────────────────────────────────────────────
  laptop-a3f2                  jr_1791349617144_0     just now
```

Approving tells the peer immediately if it is reachable; otherwise it finds out
the next time it connects or re-runs `hive join`. Denying leaves the peer
*paired* — a denial is about the hive, not the machine — but it gets nothing from
the hive, and re-asking does not undo it.

`hooks.on_hive_join_request` fires when a request is queued, so you do not have
to be watching the terminal:

```toml
[hooks.on_hive_join_request]
run = "osascript -e 'display notification \"$CLAUDECTL_HIVE_JOIN_PEER wants to join\"'"
```

### Joining as a reader

A reader meshes with the hive and receives its knowledge, and never contributes
any — the hive analogue of read-only project access. It is the one membership
tier the owner hands out explicitly, with a capability grant:

```bash
# the owner, once
claudectl access grant --scopes hive.read --project barrys-hive \
  --label "alice, read-only" --expires 30d
claudectl hive invite
```

```bash
# the reader
claudectl hive join cctl://hive/hv_3a9f21?a=... --grant cctl_gr_cf827c_9efd83d0…
```

```
Presenting a hive.read grant — asking to join as a reader.
Joined hive "read-hive" as a reader — you will receive its knowledge, and
nothing of yours is sent.
```

**Both halves are needed.** The invite is transport authentication — it pairs the
two machines. The grant is the hive-level role, and says what the peer may do
once paired. Neither alone is enough, and `--project` on `access grant` names the
*hive* here, because `hive.read:<hive-name>` is the scope.

**A grant admits directly, even on an `ask` hive.** The owner already decided when
they minted it; queueing the peer would be asking them the same question twice.
So a reader is never in the `hive requests` queue — it appears straight away in
the members list, with its role:

```
  PEER                         ROLE         HOW            ADMITTED
  barrys-mac-b0ceaa2b          reader       grant          just now

  1 of them are readers: they receive this hive's knowledge and
  contribute none of their own.
```

What a reader may and may not do:

| | Reader | Contributor |
|---|---|---|
| Receive knowledge units | yes | yes |
| Ask for a snapshot | yes | yes |
| Contribute units | **no** | yes |
| Appear in `hive requests` queue | no — the grant is the approval | only under `ask` |

A reader's attempted contribution is **refused on the wire**, not silently
dropped. The host answers with a rejection naming the reason and how many units
it discarded, and the reader prints it:

```
[2026-10-07T09:17:28Z] barrys-mac-fc78cb5c refused our knowledge:
  this hive admitted you as a reader — readers receive knowledge but do not
  contribute it (2 unit(s) dropped)
```

Silently ignoring it would be indistinguishable, at the reader, from a network
fault.

Readers need nothing from `hive trust`: `TrustTier` weighs how much a peer's
claims count for, and a reader never makes any, so merging, drift detection and
concordance checking are unchanged by this tier.

Three limits worth knowing before you hand out a reader grant:

- **The roster is authoritative after admission.** Revoking the grant does
  **not** demote an existing reader — it only stops *new* admissions. The grant
  id is recorded on the member file so you can see the connection. Removing a
  member is `rm ~/.claudectl/hive/members/<peer>.json` for now; making revocation
  reach the roster is a follow-up.
- **Renaming the hive invalidates every outstanding reader grant,** because the
  hive name is the scope qualifier. `hive identity set --name` warns and lists
  the grants that will stop working.
- **A reader is still a paired peer.** Read-only is about the hive, not about the
  relay: the two machines can still exchange heartbeats and fleet data.

### What membership actually gates

Knowledge is exchanged only with hive **members**, and in both directions. A peer
that is merely pending receives no units *and* cannot contribute any — gating only
what you send would let an unapproved peer push into the hive while getting
nothing back.

Three things worth knowing:

- **An unnamed hive gates nothing.** Membership only exists once a hive has a
  name, so nothing changes for anyone who has not named one.
- **Naming a hive admits everyone you had already paired with.** They were
  trusted before the hive had a name, and naming it does not withdraw that —
  otherwise gossip would stop dead until every peer re-joined. The command says
  who it admitted. A peer you have denied is not admitted this way.
- **A reader receives but never contributes**, which is the one case where the
  two directions differ. See *Joining as a reader* above.
- **`invite` and `open` are the same rule at the host today.** The PSK is
  per-host rather than per-invite, so the host genuinely cannot tell which link a
  peer used — pairing *is* the credential. The two policies differ in your intent
  and in what gets advertised on the LAN, not yet in what the host enforces. Only
  `ask` currently changes who gets in. Per-invite tokens are the follow-up that
  would make `invite` enforce what its name says.

Membership is stored as one create-only file per peer under
`~/.claudectl/hive/members/`, so the gossip gate is a single `stat` and
`relay serve` admitting a peer cannot collide with `hive requests approve`
running in another process. A joiner's own standing is in
`~/.claudectl/hive/membership.json` — one hive per machine.

### Finding hives on the LAN

Once a hive has a name, `relay serve` advertises it alongside the machine's own announcement, and `hive discover` lists hives rather than machines:

```bash
claudectl hive discover
```

```
Found 1 hive(s):

  HIVE                 POLICY     PEERS   UNITS   MACHINES
  ──────────────────────────────────────────────────────────────────
  barrys-hive          invite     3       412     1
                       hv_3a9f21
                         laptop-a3f2@192.168.1.50:9847
```

Rows are grouped by hive id, so several machines in one hive collect under it. `relay discover` also grew a HIVE column, showing `—` for a machine whose hive is unnamed.

Three things to know:

- **An unnamed hive advertises nothing.** No `hive` key is added to the datagram at all, so an unnamed machine sends exactly what it always sent and does not appear in `hive discover` — only in `relay discover`.
- **The policy advertised is the effective one.** A hand-edited `open` that was never confirmed goes on the wire as `invite`. The consent check lives in the stored record, not in the CLI, precisely so this path cannot leak it.
- **Renaming takes a relay restart.** The identity is read once at startup, like the index in `query serve`. If you run the launchd agent, `relay install-agent` again (or `launchctl kickstart -k`) picks up the new name.

Peer and unit counts are advisory — they are snapshots from whenever the announcer last ticked, at most one interval old.

> Joining a hive by link is a later phase; this makes one findable.

## Hive Mind: Knowledge Sharing

The hive mind is the layer that makes connected brains smarter.

### What propagation actually does today

Both ends of a connection offer each other whatever the other has not seen,
every 12 seconds, plus immediately when the serving side distills something
new. It does not matter which machine dialled: `relay serve` and `relay join`
run the same gossip code.

The offer is incremental — each side records what it has already sent a given
peer — so a tick with nothing new costs nothing, and one plain interval covers
every case that would otherwise need its own trigger: connecting, reconnecting,
being approved after a spell in the pending queue, and a unit distilled while
the link was down.

Membership gates it at both ends. The host sends only to peers on its roster
and merges only from contributors; a peer that joined as a
[reader](#joining-as-a-reader) does not push its own units upstream. A refusal
is sent back on the wire rather than silently dropped, and the refused batch is
re-offered once the gate opens, so approving a queued request does not lose the
knowledge that was turned away before it.

Two limits worth knowing:

- **`relay serve` does not dial out.** It redials a peer it has *lost*, but it
  does not connect to known peers at startup. Somebody has to dial.
- **A second connection from the same machine displaces the first.**
  `claudectl hive join` opens its own short-lived connection, so running it
  while `relay join` holds a durable one from the same machine leaves the host
  sending into the closed socket until the durable connection is
  re-established. Run `hive join` first, or restart `relay join` afterwards.
  [#459](https://github.com/mercurialsolo/claudectl/issues/459) tracks it.

### How it works once it does fire

1. Your brain distills patterns every 10 decisions (e.g., "approve `cargo test` at 95% confidence")
2. These patterns become **knowledge units** stored in `~/.claudectl/hive/knowledge.jsonl`
3. When connected to peers, knowledge units sync via **gossip protocol** — new units are sent to all peers
4. Incoming knowledge is **merged** using conflict resolution — your local preferences always win
5. Peer knowledge appears in the brain prompt with trust labels

### Trust tiers

Each peer has a trust level (0.0 to 1.0) that determines how their knowledge appears in the brain prompt:

| Trust | Tier | Label in prompt | Meaning |
|-------|------|-----------------|---------|
| >= 0.8 | Confirmed | `[hive]` | High confidence, treated as reliable |
| >= 0.5 | Suggested | `[hive, suggested]` | Default for new peers |
| >= 0.2 | Unverified | `[hive, unverified]` | Low confidence, informational only |
| < 0.2 | Ignored | Not shown | Knowledge excluded from prompts |

Trust adjusts automatically: when your brain makes a decision that agrees with hive knowledge, the source peer's trust drifts up (+0.01). Disagree, it drifts down (-0.01).

### View and manage knowledge

```bash
# Overview
claudectl hive status

# List all knowledge units
claudectl hive knowledge

# Filter by source peer
claudectl hive knowledge --from ci-runner

# Filter by scope
claudectl hive knowledge --scope project:myapp

# Export all knowledge as JSON
claudectl hive export > team-knowledge.json

# Import knowledge from a file
claudectl hive import team-knowledge.json

# Remove a specific unit
claudectl hive forget ku_1745539200_3
```

### Manage trust

```bash
# Show all peer trust levels
claudectl hive trust

# Show trust for one peer
claudectl hive trust ci-runner

# Manually set trust
claudectl hive trust ci-runner 0.9
```

## Remote Task Delegation

Delegate orchestrator tasks to connected peers. The remote machine spawns its own Claude Code session and reports status back.

### Task file with peer routing

```json
{
  "tasks": [
    {
      "name": "fix-tests",
      "prompt": "Fix the failing auth tests",
      "cwd": "/path/to/project",
      "peer": "ci-runner-9d1e"
    },
    {
      "name": "update-docs",
      "prompt": "Update the API docs",
      "cwd": "/path/to/project"
    }
  ]
}
```

Tasks with `"peer"` are delegated to the remote machine. Tasks without `"peer"` run locally. Dependencies work across local and remote tasks.

### Manual delegation

```bash
claudectl relay delegate ci-runner 'Fix the auth tests' --cwd /project
```

### Interrupts

```bash
# Nudge a remote task (informational)
claudectl relay interrupt task_123 nudge 'dependency resolved'

# Stop a remote task
claudectl relay interrupt task_123 stop 'no longer needed'
```

## TUI Integration

### Peers panel

Press `p` in the TUI to toggle the peers panel:

```
┌─ Peers (2) ──────────────────────────────────────────────┐
│ ● laptop-a3f2      connected     trust:0.8  ↑12 ↓8 kb   │
│ ● ci-runner-9d1e   connected     trust:0.5  ↑42 ↓0 kb   │
└──────────────────────────────────────────────────────────┘
```

### Brain prompt integration

When the brain evaluates a session, hive knowledge appears as a separate section:

```
## Hive Knowledge (2 peers, 15 units)
- [hive] [Bash, cargo test] approve (95%) — 20 decisions from laptop-a3f2
- [hive, suggested] [Write, *.lock] deny (88%) — 12 decisions from ci-runner
```

## Configuration

Add to `.claudectl.toml` or `~/.config/claudectl/config.toml`:

```toml
[relay]
enabled = true                    # start relay with TUI/brain
listen_port = 9847                # TCP port for peer connections
listen_addr = "0.0.0.0"           # bind address — peer transport only, not the HTTP API
max_peers = 8                     # maximum connected peers
heartbeat_interval_secs = 30      # heartbeat frequency
reconnect_max_secs = 60           # max reconnect backoff
auto_connect = []                 # list of "host:port" to auto-connect
http_addr = "127.0.0.1"           # bind address for the coordinator HTTP API.
                                  # Deliberately loopback, and deliberately
                                  # separate from listen_addr.
# http_port = 9876                # no default; unset means no HTTP API at all
# auth_token = "…"                # no default; the API's bearer token

[hive]
enabled = true                    # enable knowledge sharing
default_trust = 0.5               # trust level for new peers
auto_trust_drift = true           # adjust trust based on concordance
max_propagation = 5               # max gossip hops for knowledge units
export_min_evidence = 5           # min decisions before sharing a pattern
export_min_tool_decisions = 10    # min decisions before sharing accuracy
knowledge_ttl_days = 30           # expire unvalidated knowledge after N days
inject_unverified = true          # include low-trust knowledge in brain prompt
max_units = 500                   # hard cap on stored knowledge units
max_prompt_units = 20             # cap on units injected into brain prompt
stale_peer_days = 90              # prune knowledge from peers gone this long
share_categories = []             # empty = share all (or: ["best_practice", "technique"])
exclude_tools = []                # tools to never share (e.g., ["Write"])
exclude_commands = []             # command patterns to never share
```

`http_port` and `auth_token` have no defaults, and the HTTP API starts only
when both resolve — leave either unset and there is no listener. The bind
address is resolved `--http-addr` first, then `http_addr`, then `127.0.0.1`.

## CLI Reference

### Relay commands

| Command | Description |
|---------|-------------|
| `relay serve [--port N]` | Start the relay listener |
| `relay serve --http-port N --auth-token T [--http-addr ADDR]` | Also start the coordinator HTTP API. `--http-addr` defaults to `127.0.0.1` |
| `relay install-agent [--port N] [--http-port N] [--http-addr ADDR] [--auth-token T]` | macOS: keep the relay alive across logout and reboot |
| `relay agent-status` | Whether the launchd agent is installed and running |
| `relay uninstall-agent` | Remove the launchd agent |
| `relay invite [--qr] [--words]` | Generate invite code/link/phrase |
| `relay join <code>` | Join using any invite format |
| `relay discover` | Scan LAN for nearby instances |
| `hive discover` | Scan LAN for named hives, grouped by hive |
| `relay pair` | Generate a raw PSK code |
| `relay accept <code> <peer>` | Accept a raw PSK from a peer |
| `relay connect <host:port>` | Connect to a remote relay |
| `relay peers` | List known peers |
| `relay forget <peer>` | Remove a peer |
| `relay identity` | Show this instance's relay identity |
| `relay delegate <peer> <prompt>` | Delegate a task |
| `relay status` | Show remote task status |
| `relay interrupt <task> <type>` | Interrupt a remote task |

### Hive commands

| Command | Description |
|---------|-------------|
| `hive status` | Show knowledge store overview |
| `hive knowledge [--from X] [--scope Y]` | List knowledge units |
| `hive export` | Export knowledge as JSON |
| `hive import <file>` | Import knowledge from JSON |
| `hive forget <unit-id>` | Remove a knowledge unit |
| `hive trust [<peer> [<level>]]` | Show/set peer trust levels |
| `hive archive [--prune Nd]` | Show cold storage archive stats |
| `hive distill` | Run distillation pipeline on archive |
| `hive curriculum` | Show distilled curriculum |

## Architecture

```
┌──────────────────────────────────────────────┐
│                   HIVE MIND                  │
│  distill → knowledge units → gossip → merge  │
├──────────────────────────────────────────────┤
│              REMOTE DELEGATION               │
│  delegate → remote spawn → status → handoff  │
├──────────────────────────────────────────────┤
│                    RELAY                     │
│  TCP + PSK auth + NDJSON + heartbeats        │
└──────────────────────────────────────────────┘
```

- **Relay**: TCP transport with HMAC-SHA256 pre-shared key authentication, NDJSON wire protocol, heartbeats with exponential backoff reconnect
- **Delegation**: Remote task execution with periodic status updates, handoffs, and interrupt support
- **Hive Mind**: Gossip-based knowledge sharing with conflict resolution (local always wins), trust-weighted brain injection, epidemic propagation with TTL. See [what propagation actually does today](#what-propagation-actually-does-today) for the sync interval and the two limits that remain

## Security

The relay runs two listeners whose authentication and defaults have nothing in
common, so each claim below is scoped to one of them.

### Peer transport (TCP, `listen_addr`:`listen_port`, default `0.0.0.0:9847`)

- Every connection is authenticated via HMAC-SHA256 challenge-response, and the
  handshake proof is compared in constant time (`relay::crypto::ct_eq`, #426)
- PSK pairing requires explicit action on both sides
- Auth rate limiting: 5 failed attempts = 60s cooldown per IP
- Max concurrent auth threads capped at 16
- Session state and knowledge go only to paired peers (LAN discovery still
  broadcasts identity, port and version to the local segment)
- For encryption, tunnel through SSH or WireGuard
- Knowledge never overrides local preferences (deny-first)

Non-loopback is this listener's intended deployment, which is why it still
defaults to `0.0.0.0`.

### Coordinator HTTP API (`http_addr`:`http_port`, default loopback)

The bullets above describe the peer transport and do not hold here. This
listener authenticates with the single `--auth-token` bearer token, compared in
constant time as of #426. It has none of the rate limiting or connection caps
the peer listener has, and the token carries no scope and no revocation short
of restarting with a different one. A leaked token is full access to the API —
every session on the cluster, plus the `POST /api/heartbeat` write — until the
relay restarts.

### Transport

#426 settles [Q3 in the open-cluster RFC](open-cluster.md#q3-transport): the
HTTP API **binds `127.0.0.1` by default** and off-machine access is the
operator's tunnel to arrange — Cloudflare Tunnel, Tailscale Funnel, or
`ssh -R`. There is no TLS anywhere in this path. The API speaks plaintext
HTTP/1.1, so on any hop that is not tunnelled the bearer token and the session
data both travel in the clear.

`rustls` was the alternative, and it was rejected on the dependency rule: the
sync core runs on 7 runtime crates and `Cargo.toml` carries no TLS crate at
all. A tunnel covers the same boundary without one. The cost is operator setup,
and fronting the API does move session data off your network — that is what a
dashboard on another machine means, tunnel or not.

Before #426 the HTTP API inherited `relay.listen_addr`, so
`claudectl relay serve --http-port 9876 --auth-token secret` put the plaintext
API on every interface. It binds loopback now. Pass `--http-addr 0.0.0.0` (or
set `http_addr`) for the old behavior; `listen_addr` no longer governs the HTTP
API either way.

Both HTTP listeners warn at startup when the address they bind is not loopback
— `0.0.0.0`, `::`, or a specific LAN IP. That covers the coordinator API and
`claudectl supervisor metrics`, whose `/metrics` endpoint has no authentication
at all and defaults to `127.0.0.1:9464`.

## FAQ

**Do I need to build with `--features relay`?**
No. `relay` has been in the default feature set for a while — `cargo install claudectl` and the Homebrew bottle both ship it. The feature still exists for the minimal sync-only build (`--no-default-features --features hive`), which drops the networking and leaves hive local.

**Does it work across different networks?**
Yes, if the machines can reach each other over TCP (port 9847). For machines behind NAT, use a VPN like Tailscale or WireGuard, or SSH port forwarding.

**What happens if a peer goes offline?**
The connection drops, heartbeats detect it within 90 seconds, and the initiating side reconnects with exponential backoff. Knowledge already synced persists locally.

**Can a malicious peer poison my brain?**
No. Local knowledge always wins. Peer knowledge is labeled with trust tiers and never overrides your own preferences. Low-trust peers' knowledge can be excluded entirely.

**How much data is transferred?**
Knowledge units are small JSON records (100-300 bytes each). A typical sync between two peers transfers a few KB. Snapshots for new peers are paginated at 500KB.
