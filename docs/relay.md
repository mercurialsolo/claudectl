# Relay & Hive Mind

Share learnings, delegate tasks, and collaborate across machines — all peer-to-peer, all local-first.

## What is it?

The relay connects two or more claudectl instances over TCP. Once connected, they can:

- **Share brain knowledge** — patterns your brain learns ("always approve `cargo test`") propagate to peers automatically
- **Delegate tasks** — offload work to a remote machine running Claude Code
- **Synchronize insights** — friction patterns, error loops, and accuracy data merge across the network

Every instance stays sovereign. Your local preferences always override peer knowledge. No cloud, no central server.

## Quick Start: Connect Two Machines

### Step 1: Enable the relay feature

Build claudectl with the relay feature:

```bash
cargo install claudectl --features relay
```

### Step 2: Generate an invite

On Machine A:

```bash
claudectl --relay invite
```

Output:

```
Your identity: laptop-a3f2

  RELAY CODE:  YEK-AGA-YHK-QAA-BM

  INVITE LINK: cctl://laptop-a3f2@192.168.1.50:9847/k/a3f29b1cd4e5f678

Share any of the above with your peer. They run:

  claudectl --relay "join YEK-AGA-YHK-QAA-BM"
  claudectl --relay "join cctl://laptop-a3f2@192.168.1.50:9847/k/a3f29b1cd4e5f678"
```

### Step 3: Join from Machine B

```bash
claudectl --relay "join YEK-AGA-YHK-QAA-BM"
```

That's it. Both machines are paired and connected.

### Step 4: Start the relay server

On Machine A (the one that generated the invite):

```bash
claudectl --relay serve
```

Machine B connects:

```bash
claudectl --relay "connect 192.168.1.50:9847"
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
claudectl --relay "invite --words"
```

```
fur-hue-ace-bid-ice-ape-cod-elk-ace
```

9 common English words. Easier to dictate than alphanumeric codes.

### Invite Link + QR Code

```bash
claudectl --relay "invite --qr"
```

```
cctl://laptop-a3f2@192.168.1.50:9847/k/a3f29b1cd4e5f678
```

Plus a scannable QR code in the terminal (requires `qrencode` installed).

The `join` command auto-detects the format:

```bash
claudectl --relay "join YEK-AGA-YHK-QAA-BM"           # relay code
claudectl --relay "join fur-hue-ace-bid-ice-..."        # word phrase
claudectl --relay "join cctl://laptop-a3f2@..."         # invite link
```

## LAN Discovery

Find nearby claudectl instances without codes:

```bash
claudectl --relay discover
```

```
Found 2 instance(s):

  IDENTITY             ADDRESS                  VERSION
  ────────────────────────────────────────────────────────
  laptop-a3f2          192.168.1.50:9847        v0.40.0
  ci-runner-9d1e       192.168.1.101:9847       v0.40.0
```

This sends a UDP broadcast and listens for 3 seconds. Peers running `claudectl --relay serve` announce themselves automatically.

## Hive Mind: Knowledge Sharing

The hive mind is the layer that makes connected brains smarter. It works automatically once peers are connected.

### How it works

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
claudectl --hive status

# List all knowledge units
claudectl --hive knowledge

# Filter by source peer
claudectl --hive "knowledge --from ci-runner"

# Filter by scope
claudectl --hive "knowledge --scope project:myapp"

# Export all knowledge as JSON
claudectl --hive export > team-knowledge.json

# Import knowledge from a file
claudectl --hive "import team-knowledge.json"

# Remove a specific unit
claudectl --hive "forget ku_1745539200_3"
```

### Manage trust

```bash
# Show all peer trust levels
claudectl --hive trust

# Show trust for one peer
claudectl --hive "trust ci-runner"

# Manually set trust
claudectl --hive "trust ci-runner 0.9"
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
claudectl --relay "delegate ci-runner 'Fix the auth tests' --cwd /project"
```

### Interrupts

```bash
# Nudge a remote task (informational)
claudectl --relay "interrupt task_123 nudge 'dependency resolved'"

# Stop a remote task
claudectl --relay "interrupt task_123 stop 'no longer needed'"
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
listen_addr = "0.0.0.0"          # bind address
max_peers = 8                     # maximum connected peers
heartbeat_interval_secs = 30      # heartbeat frequency
reconnect_max_secs = 60           # max reconnect backoff
auto_connect = []                 # list of "host:port" to auto-connect

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

## CLI Reference

### Relay commands

| Command | Description |
|---------|-------------|
| `--relay serve [--port N]` | Start the relay listener |
| `--relay invite [--qr] [--words]` | Generate invite code/link/phrase |
| `--relay "join <code>"` | Join using any invite format |
| `--relay discover` | Scan LAN for nearby instances |
| `--relay pair` | Generate a raw PSK code |
| `--relay "accept <code> <peer>"` | Accept a raw PSK from a peer |
| `--relay "connect <host:port>"` | Connect to a remote relay |
| `--relay peers [--json]` | List known peers |
| `--relay "forget <peer>"` | Remove a peer |
| `--relay identity` | Show this instance's relay identity |
| `--relay "delegate <peer> <prompt>"` | Delegate a task |
| `--relay status` | Show remote task status |
| `--relay "interrupt <task> <type>"` | Interrupt a remote task |

### Hive commands

| Command | Description |
|---------|-------------|
| `--hive status` | Show knowledge store overview |
| `--hive knowledge [--from X] [--scope Y]` | List knowledge units |
| `--hive export` | Export knowledge as JSON |
| `--hive "import <file>"` | Import knowledge from JSON |
| `--hive "forget <unit-id>"` | Remove a knowledge unit |
| `--hive trust` | Show/set peer trust levels |
| `--hive archive` | Show cold storage archive stats |
| `--hive "archive --prune 90d"` | Prune archive entries older than 90 days |
| `--hive distill` | Run distillation pipeline on archive |
| `--hive curriculum` | Show distilled curriculum |

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
- **Hive Mind**: Gossip-based knowledge sharing with conflict resolution (local always wins), trust-weighted brain injection, epidemic propagation with TTL

## Security

- All connections are authenticated via HMAC-SHA256 challenge-response
- PSK pairing requires explicit action on both sides
- No data leaves your network (peer-to-peer only)
- Auth rate limiting: 5 failed attempts = 60s cooldown per IP
- Max concurrent auth threads capped at 16
- Knowledge never overrides local preferences (deny-first)
- For encryption, tunnel through SSH or WireGuard

## FAQ

**Do I need to build with `--features relay`?**
Yes. The relay and hive modules are feature-gated to keep the default binary small. Without the feature flag, the binary is unchanged.

**Does it work across different networks?**
Yes, if the machines can reach each other over TCP (port 9847). For machines behind NAT, use a VPN like Tailscale or WireGuard, or SSH port forwarding.

**What happens if a peer goes offline?**
The connection drops, heartbeats detect it within 90 seconds, and the initiating side reconnects with exponential backoff. Knowledge already synced persists locally.

**Can a malicious peer poison my brain?**
No. Local knowledge always wins. Peer knowledge is labeled with trust tiers and never overrides your own preferences. Low-trust peers' knowledge can be excluded entirely.

**How much data is transferred?**
Knowledge units are small JSON records (100-300 bytes each). A typical sync between two peers transfers a few KB. Snapshots for new peers are paginated at 500KB.
