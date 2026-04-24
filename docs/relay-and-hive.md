# Relay and Hive: Cross-Machine Collaboration for claudectl

Status: Draft

This document specifies a relay transport, remote coordination protocol, and hive mind knowledge-sharing system that extends claudectl from a local session supervisor into a distributed collaboration plane across machines, accounts, and teams.

## Thesis

Coding agents learn in isolation. Each claudectl brain distills preferences, detects friction patterns, and builds accuracy — but that knowledge dies at the machine boundary. Two developers working on the same project, or even the same developer across a laptop and a CI runner, cannot share what their brains have learned.

This design introduces three layers:

1. **Relay** — a zero-dependency TCP transport with pre-shared key authentication
2. **Remote Coordination** — task delegation, status streaming, and handoffs across machines
3. **Hive Mind** — gossip-based knowledge propagation where every connected brain gets smarter from every other brain's corrections

Each layer is independently useful. Relay alone enables task offloading. Relay + coordination gives cross-machine orchestration. All three together create a convergent hive mind where local learning compounds across the network.

## Why This Exists

The coordination layer (see `coordination-layer.md`) solves multi-agent coordination on a single machine. It does not solve:

- offloading a task to a remote machine with spare capacity
- two developers' Claude Code instances collaborating on shared work
- sharing brain learnings across machines so corrections propagate
- bootstrapping a new machine's brain with the team's accumulated wisdom
- global cost tracking across distributed sessions

These problems require a network layer. But claudectl's constraints (zero async runtime, 7 runtime crates, <1MB binary, local-first) mean we cannot bolt on a web framework or message broker. The relay is built entirely on `std::net`.

## Design Principles

### Peer-to-peer, not hub-spoke

Both instances run a listener and can initiate connections. Either side can delegate work to the other. The relationship is symmetric by default — there is no inherent controller/worker hierarchy. Roles emerge from usage, not from topology.

### Local always wins

The deny-first principle extends to knowledge. If a peer's knowledge unit conflicts with a local preference, the local preference wins unconditionally. Peer knowledge enriches the brain's context but never overrides local decisions.

### Zero new dependencies

The relay uses `std::net::TcpListener` and `std::net::TcpStream`. Authentication uses HMAC-SHA256 implemented inline (the algorithm is simple enough to not warrant a crate). If operators need encryption, they tunnel through SSH or WireGuard — that is an infrastructure concern, not claudectl's.

### Gossip over consensus

Knowledge propagates via epidemic gossip, not distributed consensus. There is no leader election, no quorum, no total ordering. Each brain is sovereign. Knowledge units spread like rumors: high-confidence, well-validated knowledge propagates further and faster. Conflicts resolve locally by preferring local knowledge.

### Independent layers

The relay transport is useful without the hive mind. Remote task delegation is useful without knowledge sharing. Each layer adds value incrementally. This also means each layer can be implemented and shipped independently.

## Non-Goals

- Centralized server or cloud dependency
- Real-time file synchronization between machines
- Shared terminal/TUI streaming
- Cross-machine JSONL log streaming (status updates are sufficient)
- Support for non-claudectl peers (the protocol is claudectl-specific)
- Encryption at the transport layer (use SSH tunnels or VPN)
- Automatic peer discovery (peers are explicitly configured)

---

## Layer 1: Relay Transport

### Overview

The relay is a TCP socket layer that allows two claudectl instances to exchange structured JSON messages. It handles connection lifecycle, authentication, heartbeats, and reconnection.

### Module Structure

```
src/relay/
  mod.rs          — PeerId, RelayMessage, MessageType, re-exports
  protocol.rs     — NDJSON framing, HMAC-SHA256 challenge-response auth
  listener.rs     — TcpListener accept loop on dedicated thread
  peer.rs         — single peer connection: read/write/reconnect
  mesh.rs         — peer registry, broadcast, route-to-peer
```

Feature-gated behind `relay` in Cargo.toml (like `coord` is gated behind `coord`), so the default binary is unchanged.

### Data Types

```rust
/// Unique identity for a claudectl instance in the network.
/// Derived from hostname + a random suffix, generated on first run.
/// Stored in ~/.claudectl/relay/identity.
struct PeerId(String);

/// Every message over the wire.
struct RelayMessage {
    id: String,              // UUID for dedup
    msg_type: MessageType,
    from_peer: PeerId,
    timestamp: u64,          // epoch millis
    payload: serde_json::Value,
}

enum MessageType {
    // Layer 1: transport
    Handshake,
    HandshakeAck,
    Heartbeat,
    Ack,

    // Layer 2: coordination
    DelegateTask,
    TaskStatus,
    TaskHandoff,
    TaskInterrupt,

    // Layer 3: hive
    KnowledgeSync,
    KnowledgeRequest,
    KnowledgeSnapshot,
}
```

### Wire Protocol

Newline-delimited JSON (NDJSON) over TCP. Each line is a complete `RelayMessage` serialized as JSON, terminated by `\n`. No framing headers, no length prefixes, no binary encoding.

Rationale: NDJSON is trivially debuggable (`nc host port` shows readable messages), parseable with `serde_json::from_str`, and compatible with line-oriented buffered readers.

### Authentication: HMAC Challenge-Response

Pairing uses a pre-shared key (PSK) generated by `claudectl relay pair`. The PSK is a 32-byte random value, displayed as a human-friendly code (e.g., `kx7f-m2np-9a3d-w1vz`).

```
Connection handshake:

1. Connector opens TCP stream to listener
2. Listener sends:    {"type": "challenge", "nonce": "<random-32-bytes-hex>"}
3. Connector computes: HMAC-SHA256(key=PSK, message=nonce)
4. Connector sends:   {"type": "handshake", "peer_id": "...", "proof": "<hmac-hex>",
                        "version": "0.35.0"}
5. Listener verifies HMAC, checks version compatibility
6. Listener sends:    {"type": "handshake_ack", "peer_id": "...", "status": "ok"}
7. Connection established. Both sides begin message exchange.

If HMAC verification fails:
6. Listener sends:    {"type": "handshake_ack", "status": "denied"}
7. Listener closes connection.
```

The HMAC-SHA256 implementation is inlined (~60 lines of Rust for the algorithm). No external crate needed.

PSK storage: `~/.claudectl/relay/peers/<peer_id>.key` — one file per paired peer, containing the shared key. The file is chmod 600.

### Connection Lifecycle

```
States:
  Disconnected → Connecting → Authenticating → Connected → Disconnected

Heartbeats:
  Every 30 seconds, both sides send a Heartbeat message.
  If 3 consecutive heartbeats are missed (90 seconds), the connection
  is considered dead and transitions to Disconnected.

Reconnection:
  On disconnect, the initiating side (the one that called `relay connect`)
  enters a reconnect loop with exponential backoff:
    5s → 10s → 20s → 40s → 60s (cap)
  The listener side does not reconnect — it waits for the connector.

Deduplication:
  Each RelayMessage carries a unique `id`. Recipients track the last 1000
  message IDs and silently drop duplicates. This handles retransmission
  after reconnect.
```

### Threading Model

claudectl is synchronous with no async runtime. The relay uses dedicated threads:

- **Listener thread**: Runs `TcpListener::accept()` in a loop. On new connection, spawns a reader thread for that peer.
- **Reader thread** (per peer): Reads lines from the `TcpStream`, parses into `RelayMessage`, dispatches to the appropriate handler via a `crossbeam`-free channel (use `std::sync::mpsc`).
- **Writer**: Messages are sent by any thread calling `peer.send(msg)`, which locks the `TcpStream` via `Arc<Mutex<TcpStream>>`.

No new threading crate needed — `std::thread`, `std::sync::mpsc`, and `Arc<Mutex<_>>` suffice.

### Configuration

In `.claudectl.toml` or `~/.config/claudectl/config.toml`:

```toml
[relay]
enabled = true
listen_port = 9847
listen_addr = "0.0.0.0"      # or "127.0.0.1" for local-only
max_peers = 8
heartbeat_interval_secs = 30
reconnect_max_secs = 60
```

### CLI

```bash
claudectl relay serve                    # start listener (foreground, for testing)
claudectl relay pair                     # generate a new PSK, display as code
claudectl relay accept <code>            # store a PSK from a peer's pair command
claudectl relay connect <host:port>      # connect to a remote peer
claudectl relay peers                    # list connected/known peers
claudectl relay disconnect <peer_id>     # drop a connection
claudectl relay forget <peer_id>         # remove peer and its PSK
```

When the TUI or brain is running, the relay starts automatically if `relay.enabled = true` in config.

### Persistence

```
~/.claudectl/relay/
  identity              — this instance's PeerId (generated once)
  peers/
    <peer_id>.key       — PSK for each paired peer (chmod 600)
    <peer_id>.meta      — JSON: last_seen, trust_level, stats
  message_log.jsonl     — optional: last N messages for debugging
```

---

## Layer 2: Remote Coordination

### Overview

Remote coordination extends the existing orchestrator and coord store to support task delegation across machines. A task can be spawned locally or sent to a connected peer for execution. The delegating side receives structured status updates; the executing side uses its own brain and sessions.

### Extended TaskDef

The orchestrator's `TaskDef` gains an optional `peer` field:

```rust
pub struct TaskDef {
    pub name: String,
    pub prompt: String,
    pub cwd: Option<String>,
    pub depends_on: Vec<String>,
    pub resume: Option<String>,
    pub retries: Option<u32>,
    pub peer: Option<PeerId>,      // NEW: None = local, Some = remote
}
```

When `peer` is `Some`, the orchestrator serializes the task as a `DelegateTask` message and sends it over the relay instead of spawning a local process.

### Delegation Protocol

```
Controller (Machine A)                       Worker (Machine B)
    │                                             │
    ├── DelegateTask ────────────────────────────► │
    │   { task_id, prompt, cwd, context,          │
    │     git_remote, git_ref, depends_on }       │
    │                                             │
    │                                             ├── validate context
    │                                             ├── git clone/pull if needed
    │                                             ├── spawn `claude` session
    │                                             │
    │ ◄──────────────────────────── TaskStatus ────┤
    │   { task_id, state: "running",              │
    │     pid, session_id }                       │
    │                                             │
    │ ◄──────────────────────────── TaskStatus ────┤  (periodic, every 30s)
    │   { task_id, state: "running",              │
    │     tokens_used, cost_usd,                  │
    │     context_pct, files_modified }           │
    │                                             │
    │ ◄──────────────────────────── TaskHandoff ──┤  (on completion)
    │   { task_id, state: "completed",            │
    │     summary, artifacts, next_steps,         │
    │     git_ref: "branch-name",                 │
    │     total_cost_usd, total_tokens }          │
    │                                             │
    ├── TaskInterrupt ──────────────────────────► │  (optional)
    │   { task_id, interrupt_type: "nudge",       │
    │     reason: "dependency resolved" }         │
    │                                             │
    ├── Ack ◄────────────────────────────────────│
```

### Task Context Payload

The `DelegateTask` message includes a context payload that helps the remote worker understand the task without needing the full local filesystem:

```json
{
  "task_id": "t_1713960000_0",
  "prompt": "Fix the auth middleware tests in src/auth/",
  "cwd": "/path/to/project",

  "context": {
    "git_remote": "git@github.com:team/project.git",
    "git_ref": "feat/auth-rewrite",
    "git_commit": "abc123",

    "relevant_files": {
      "src/auth/middleware.rs": "<file content or snippet>",
      "tests/auth_test.rs": "<file content or snippet>"
    },

    "brain_context": {
      "project_preferences": "<distilled preferences summary>",
      "recent_insights": ["error loops in auth module", "context blowout risk"]
    },

    "dependency_graph": {
      "blocks": ["deploy-staging"],
      "blocked_by": []
    }
  }
}
```

The `relevant_files` field is optional and size-limited (max 50KB total). For larger contexts, the git remote + ref is the primary mechanism — the worker clones the repo.

### Remote Task Lifecycle

On the worker side, a delegated task follows the same lifecycle as a local orchestrator task:

1. **Receive** `DelegateTask` → validate, store in local coord DB
2. **Prepare** → `git clone`/`git pull` to the specified ref if `git_remote` provided
3. **Spawn** → launch `claude` session with the prompt and cwd
4. **Monitor** → local brain watches the session, sends periodic `TaskStatus`
5. **Complete** → send `TaskHandoff` with summary, artifacts, and final git ref
6. **Cleanup** → mark task as completed in local coord DB

The worker's brain makes its own decisions about the delegated session (approve/deny tool use, detect errors, etc.). The controller does not micro-manage — it delegates and receives results.

### Remote Task States in TUI

The controller's TUI shows delegated tasks with a peer indicator:

```
┌─ Sessions ───────────────────────────────────────────────────────────┐
│ PID    Status      Project           Cost    Tokens   Peer          │
│ 12345  working     claudectl         $0.42   18.2k    (local)       │
│ ──     working     auth-service      $0.18    7.1k    machineB ↗    │
│ ──     completed   deploy-staging    $0.05    2.3k    ci-runner ↗   │
└──────────────────────────────────────────────────────────────────────┘
```

Delegated tasks have no local PID — they show `──` and the peer name with an arrow.

### Cost Tracking

Remote task costs are tracked separately in `history.rs`:

```rust
pub struct CostEntry {
    // ... existing fields ...
    pub peer: Option<PeerId>,      // NEW: None = local, Some = remote
    pub delegated: bool,           // NEW: true if this cost was incurred on a peer
}
```

The `claudectl stats` and `claudectl history` commands show local vs delegated costs:

```
Total spend: $12.47
  Local:     $9.82 (78.7%)
  Delegated: $2.65 (21.3%)
    machineB:  $1.80
    ci-runner: $0.85
```

### Interrupt Forwarding

The controller can send interrupts to delegated tasks:

```bash
# Nudge a remote task
claudectl relay interrupt <task_id> nudge "dependency resolved, check blockers"

# Stop a remote task
claudectl relay interrupt <task_id> stop "no longer needed, scope changed"
```

These map to `TaskInterrupt` messages. The worker receives them and applies them to the local session via the existing interrupt bus.

---

## Layer 3: Hive Mind

### Overview

The hive mind is a gossip-based knowledge sharing system that allows connected claudectl brains to learn from each other's corrections, preferences, and insights. Each brain remains sovereign — local knowledge always takes precedence — but peer knowledge enriches the decision-making context.

### Module Structure

```
src/hive/
  mod.rs          — KnowledgeUnit, KnowledgeScope, KnowledgeContent, re-exports
  store.rs        — local hive knowledge store (~/.claudectl/hive/)
  distiller.rs    — convert DistilledPreferences/Insights → KnowledgeUnits
  merger.rs       — conflict resolution, trust-weighted merge
  gossip.rs       — sync protocol, snapshot generation, propagation
```

Feature-gated behind `relay` (hive requires relay transport).

### Knowledge Unit

The atom of shared learning. Maps directly to existing brain data structures.

```rust
/// A single piece of shareable knowledge.
struct KnowledgeUnit {
    /// Unique ID: `ku_{epoch}_{counter}`
    id: String,

    /// What scope this knowledge applies to.
    scope: KnowledgeScope,

    /// The type and content of the knowledge.
    content: KnowledgeContent,

    /// How many local decisions back this knowledge.
    evidence_count: u32,

    /// Distillation confidence (0.0 to 1.0).
    confidence: f64,

    /// Which peer originated this knowledge.
    source_peer: PeerId,

    /// When first created (epoch secs).
    originated_at: u64,

    /// When last validated by the originator (epoch secs).
    last_validated_at: u64,

    /// How many peers have accepted this knowledge.
    propagation_count: u32,

    /// Monotonic version — incremented when the originator updates this unit.
    version: u32,
}

/// Scope determines where knowledge applies.
enum KnowledgeScope {
    /// Applies to all projects and languages.
    Universal,
    /// Applies to a specific programming language.
    Language(String),
    /// Applies to a specific project (by slug).
    Project(String),
}

/// The actual knowledge payload.
enum KnowledgeContent {
    /// A distilled preference pattern.
    /// Maps to brain::preferences::PreferencePattern.
    Pattern {
        tool: String,
        command_pattern: Option<String>,
        preferred_action: String,
        accept_rate: f64,
        conditions: Vec<String>,  // serialized PreferenceConditions
    },

    /// Per-tool accuracy statistics.
    /// Maps to brain::preferences::ToolAccuracy.
    ToolAccuracy {
        tool: String,
        total: u32,
        correct: u32,
        confidence_threshold: f64,
    },

    /// A temporal behavior pattern.
    /// Maps to brain::preferences::TemporalPattern.
    Temporal {
        description: String,
        strength: f64,
    },

    /// A detected friction/error/cost insight.
    /// Maps to brain::insights::Insight.
    Insight {
        category: String,
        severity: String,
        summary: String,
        suggestion: Option<String>,
    },

    /// A promoted rule from coord memory.
    PromotedRule {
        rule: String,
        source_type: String,  // "workflow", "preference", "guard"
    },
}
```

### What Gets Shared vs What Stays Local

| Data | Shared | Rationale |
|------|--------|-----------|
| `PreferencePattern` | Yes | Universal tool behavior learnings |
| `ToolAccuracy` | Yes | Calibration data improves all brains |
| `TemporalPattern` | Scoped | Time-of-day patterns may be user-specific |
| `Insight` | Yes | Friction patterns help everyone |
| Promoted rules | Yes | High-confidence workflow guards |
| Raw decision logs | **No** | Too noisy, contains private context |
| Decision context snapshots | **No** | Contains cost/model/file details |
| API keys, credentials | **No** | Obviously not |
| User-specific overrides | **No** | Personal preference stays personal |
| Prompt template customizations | **No** | Local customization is intentional |

### Distillation Pipeline

The existing distillation cycle (every 10 decisions) is extended with a hive export step:

```
Local decisions accumulate
         │
         ▼ (every 10 decisions)
┌────────────────────┐
│ Distill preferences │  ← existing: produces DistilledPreferences
└────────┬───────────┘
         │
         ▼
┌────────────────────┐
│ Generate insights   │  ← existing: produces Vec<Insight>
└────────┬───────────┘
         │
         ▼
┌────────────────────────────┐
│ Export to knowledge units   │  ← NEW: hive::distiller
│                            │
│ For each PreferencePattern │
│   with evidence ≥ 5:      │
│   → KnowledgeUnit::Pattern │
│                            │
│ For each ToolAccuracy      │
│   with total ≥ 10:        │
│   → KnowledgeUnit::ToolAcc │
│                            │
│ For each Insight           │
│   severity ≥ Warning:     │
│   → KnowledgeUnit::Insight │
└────────┬───────────────────┘
         │
         ▼
┌──────────────────────┐
│ Diff against last    │  ← only changed/new units
│ sync snapshot        │
└────────┬─────────────┘
         │
         ▼
┌────────────────────┐
│ Broadcast via relay │  → KnowledgeSync message to all peers
└────────────────────┘
```

Thresholds for export (configurable):
- `PreferencePattern`: minimum 5 evidence decisions
- `ToolAccuracy`: minimum 10 total decisions for that tool
- `Insight`: minimum severity of Warning
- `TemporalPattern`: minimum strength of 0.7

These thresholds prevent noisy, low-confidence knowledge from polluting the hive.

### Gossip Protocol

#### Sync message

When a peer's distillation cycle produces changed knowledge units, it broadcasts a `KnowledgeSync` message:

```json
{
  "type": "KnowledgeSync",
  "from_peer": "machineA",
  "units": [
    {
      "id": "ku_1713960000_42",
      "scope": "language:rust",
      "content": { "type": "pattern", "tool": "Bash", "command_pattern": "cargo test",
                   "preferred_action": "approve", "accept_rate": 0.95,
                   "conditions": [] },
      "evidence_count": 23,
      "confidence": 0.95,
      "source_peer": "machineA",
      "originated_at": 1713900000,
      "last_validated_at": 1713960000,
      "propagation_count": 0,
      "version": 3
    }
  ],
  "sync_epoch": 1713960000
}
```

#### Snapshot (for new peers)

When a new peer connects, it can request a full snapshot:

```json
{"type": "KnowledgeRequest", "from_peer": "newMachine", "since_epoch": 0}
```

The responding peer sends a `KnowledgeSnapshot` containing all its accepted knowledge units (both locally originated and previously accepted from other peers).

#### Propagation

When a peer accepts a knowledge unit from another peer, it increments `propagation_count` and may re-broadcast to its own peers (excluding the source). This creates epidemic spread — a valuable insight discovered on one machine can reach all connected machines within a few sync cycles.

Propagation is bounded:
- A unit is only re-broadcast if its `propagation_count < max_propagation` (default 5)
- Units older than 30 days without re-validation are not propagated
- Each peer tracks which units it has already sent to each other peer

### Merge and Conflict Resolution

When a `KnowledgeSync` or `KnowledgeSnapshot` is received, each unit goes through the merger:

```
For each incoming KnowledgeUnit:

1. Check if a local unit with the same semantic key exists.
   Semantic key = (scope, content_type, tool, command_pattern)

2. If no local match:
   → Accept the unit, store in hive DB
   → Weight = confidence × peer_trust × evidence_factor
   → If weight > 0.3, add to brain prompt context

3. If local match exists and is locally originated:
   → Local always wins. Drop the incoming unit.
   → Log the conflict for diagnostics.

4. If local match exists and was from another peer:
   → Keep the one with higher (confidence × evidence_count).
   → On tie, keep the newer one (higher version).

5. After merge, rebuild the hive knowledge prompt section.
```

### Trust Model

```rust
struct PeerTrust {
    peer_id: PeerId,
    /// Trust level: 0.0 (ignore everything) to 1.0 (fully trusted).
    /// Default: 0.5. Configurable per peer.
    trust_level: f64,
    /// How many knowledge units accepted from this peer.
    knowledge_accepted: u32,
    /// How many units conflicted with local knowledge.
    knowledge_conflicted: u32,
    /// When this peer first connected.
    first_seen: u64,
    /// When this peer last synced knowledge.
    last_sync: u64,
}
```

Trust affects how peer knowledge enters the brain prompt:

- `trust >= 0.8`: Knowledge appears as confirmed (`[hive]` tag)
- `trust >= 0.5`: Knowledge appears as suggested (`[hive, suggested]` tag)
- `trust >= 0.2`: Knowledge appears as unverified (`[hive, unverified]` tag)
- `trust < 0.2`: Knowledge is stored but not injected into prompts

Trust can be adjusted:
- Manually: `claudectl hive trust <peer_id> 0.8`
- Automatically: when local brain agrees with peer knowledge (trust drifts up) or disagrees (trust drifts down). Drift rate: ±0.01 per concordant/discordant decision, clamped to [0.0, 1.0].

### Brain Prompt Integration

Peer knowledge is injected into the brain's advisory prompt as a separate section, clearly distinguished from local preferences:

```
## Local Preferences (from your corrections)
- [Bash, cargo test] approve (95%, 23 decisions)
- [Bash, cargo clippy] approve (92%, 18 decisions)
- [Bash, rm -rf] deny (100%, 5 decisions)

## Hive Knowledge (3 peers, 47 units)
- [hive] [Bash, cargo fmt] approve (93%, 31 decisions across 3 peers)
- [hive] [Write, *.lock] deny (88%, 12 decisions from machineB)
- [hive, suggested] [Bash, docker push] deny when not_ci_dir (82%, 8 decisions from ci-runner)
- [hive, unverified] [Bash, npm audit] approve (71%, 4 decisions from alice-mbp)
```

The brain sees both sections and can weigh them appropriately. Local preferences are authoritative; hive knowledge is advisory.

### Persistence

```
~/.claudectl/hive/
  knowledge.jsonl       — all accepted knowledge units (append-only)
  sync_state.json       — per-peer sync cursors (last_sync_epoch per peer)
  trust.json            — per-peer trust levels
  conflicts.jsonl       — log of merge conflicts (for diagnostics)
```

---

## CLI Surface

### Relay commands

```bash
# Serve: start the relay listener
claudectl relay serve [--port 9847] [--foreground]

# Pair: generate a pre-shared key
claudectl relay pair
# Output: PAIR CODE: kx7f-m2np-9a3d-w1vz
#         Share this code with the peer you want to connect.

# Accept: store a PSK from another peer
claudectl relay accept <code>

# Connect: establish connection to a remote peer
claudectl relay connect <host:port>

# Peers: list known/connected peers
claudectl relay peers [--json]

# Delegate: send a task to a remote peer
claudectl relay delegate <peer_id> "<prompt>" [--cwd /path] [--git-ref branch]

# Status: show all remote tasks
claudectl relay status [--json]

# Interrupt: send interrupt to a remote task
claudectl relay interrupt <task_id> <type> [reason]

# Disconnect/forget
claudectl relay disconnect <peer_id>
claudectl relay forget <peer_id>
```

### Hive commands

```bash
# Status: show hive network stats
claudectl hive status

# Knowledge: list shared knowledge units
claudectl hive knowledge [--scope universal|language:rust|project:foo]
claudectl hive knowledge [--from <peer_id>]
claudectl hive knowledge [--json]

# Trust: get/set peer trust
claudectl hive trust                         # show all
claudectl hive trust <peer_id>              # show one
claudectl hive trust <peer_id> <0.0-1.0>   # set

# Export/Import: offline knowledge transfer
claudectl hive export > knowledge.json
claudectl hive import knowledge.json

# Forget: remove a specific knowledge unit
claudectl hive forget <unit_id>
```

### TUI Integration

New `p` keybinding toggles a peers panel:

```
┌─ Peers ───────────────────────────────────────────────────────────────┐
│ machineB       ● connected   trust:0.8   ↑12 ↓8 knowledge units    │
│ alice-mbp      ○ reconnecting...         trust:0.5                  │
│ ci-runner      ● connected   trust:0.3   ↑42 ↓0 knowledge units    │
└───────────────────────────────────────────────────────────────────────┘
```

The detail panel for a delegated task shows peer info:

```
┌─ Task Detail ─────────────────────────────────────────────────────────┐
│ Name:    fix-auth-tests                                              │
│ Peer:    machineB (● connected)                                      │
│ Status:  working (14m elapsed)                                       │
│ Tokens:  22.4k used                                                  │
│ Cost:    $0.31                                                       │
│ Context: 42%                                                         │
│ Files:   3 modified                                                  │
│ Last:    "running cargo test, 2 failures remaining"                  │
└──────────────────────────────────────────────────────────────────────┘
```

---

## Configuration

All relay and hive settings in `.claudectl.toml`:

```toml
[relay]
enabled = false                   # opt-in
listen_port = 9847
listen_addr = "0.0.0.0"
max_peers = 8
heartbeat_interval_secs = 30
reconnect_max_secs = 60
auto_connect = []                 # list of "host:port" to connect on startup

[hive]
enabled = false                   # opt-in (requires relay)
default_trust = 0.5
auto_trust_drift = true           # adjust trust based on concordance
max_propagation = 5               # max hops for knowledge propagation
export_min_evidence = 5           # min decisions before sharing a pattern
export_min_tool_decisions = 10    # min decisions before sharing tool accuracy
export_min_insight_severity = "warning"
knowledge_ttl_days = 30           # expire unvalidated knowledge after N days
inject_unverified = true          # include low-trust knowledge in brain prompt
```

---

## Security Considerations

### Threat model

The relay operates on trusted networks between machines you control or your team controls. It is not designed for exposure to the public internet.

### Authentication

PSK-based HMAC challenge-response. The PSK never travels over the wire — only the HMAC proof does. An attacker who observes the handshake cannot derive the PSK (assuming HMAC-SHA256 is secure, which it is).

### No encryption

The wire protocol is plaintext JSON. This is a deliberate choice to avoid adding a TLS dependency. For encryption:
- Use SSH tunnels: `ssh -L 9847:localhost:9847 remote-host`
- Use WireGuard or Tailscale for the network layer
- Use a VPN

The documentation should prominently recommend one of these approaches.

### Command injection

The relay receives prompts and executes them via `claude` CLI on the worker side. This is inherently powerful — a connected peer can run arbitrary prompts. The trust model partially mitigates this (low-trust peers can be limited), but the primary protection is the pairing step: you only pair with machines you trust.

Future hardening options (not in v1):
- Allow-list of permitted prompt patterns per peer
- Require human confirmation for delegated tasks on the worker side
- Read-only mode: peer can observe but not delegate

### Knowledge poisoning

A compromised or malicious peer could send bad knowledge units to shift the brain's behavior. Mitigations:
- Local knowledge always wins (prevents overriding established preferences)
- Low-trust peers' knowledge is labeled `[unverified]` in the brain prompt
- Trust drift naturally penalizes peers whose knowledge conflicts with local decisions
- Knowledge units require minimum evidence thresholds to be shared
- The `conflicts.jsonl` log makes poisoning attempts visible

---

## Implementation Phases

### Phase 1: Relay Transport
Ship the TCP pipe with PSK auth, heartbeats, reconnect.
No coordination or hive — just the ability to connect two claudectl instances.
Test: two machines can pair, connect, exchange heartbeats, reconnect after disconnect.

### Phase 2: Remote Task Delegation
Extend orchestrator TaskDef with `peer` field.
Implement DelegateTask/TaskStatus/TaskHandoff/TaskInterrupt messages.
Worker spawns local sessions, reports status.
Test: delegate a task, watch it execute remotely, receive handoff.

### Phase 3: Hive Knowledge Store
Build the knowledge unit types and local store.
Implement the distiller (DistilledPreferences → KnowledgeUnits).
No gossip yet — just local export/import.
Test: distill preferences, export to JSON, import on another machine.

### Phase 4: Gossip Protocol
Wire the distiller to the relay.
Implement KnowledgeSync/KnowledgeRequest/KnowledgeSnapshot.
Implement the merger with conflict resolution.
Test: two connected instances share knowledge, new peer gets snapshot.

### Phase 5: Trust and Brain Integration
Implement PeerTrust with auto-drift.
Inject hive knowledge into brain prompts.
Add TUI peers panel.
Test: peer knowledge appears in brain context, trust adjusts based on concordance.

---

## Appendix: HMAC-SHA256 Implementation

The HMAC-SHA256 algorithm (RFC 2104) is simple enough to implement inline without a crate:

```
HMAC(K, m) = H((K' ⊕ opad) || H((K' ⊕ ipad) || m))

where:
  H = SHA-256
  K' = K padded/hashed to block size (64 bytes)
  ipad = 0x36 repeated 64 times
  opad = 0x5c repeated 64 times
```

SHA-256 itself is ~150 lines of Rust (pure computation, no I/O, no allocations beyond a fixed buffer). Together with the HMAC wrapper, this is ~200 lines — well within claudectl's "inline over crate" ethos.

Alternative: if the `coord` feature is enabled, `rusqlite` bundles SQLite which includes a SHA-256 implementation. We could potentially reuse it, but the inline approach is cleaner and doesn't create a cross-feature dependency.

## Appendix: Message Size Limits

To prevent memory exhaustion from malformed or malicious messages:

- Maximum line length: 1MB (messages exceeding this are dropped)
- Maximum `relevant_files` payload in DelegateTask: 50KB
- Maximum KnowledgeSnapshot size: 500KB (~5000 knowledge units)
- If a snapshot exceeds the limit, it is paginated into multiple messages
