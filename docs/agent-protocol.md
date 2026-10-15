# claudectl Agent Protocol — Design Specification

**Status:** Proposed, epic #558. Nothing here is built. Written against the code as of `edaed703`.
**Scope:** Let any coding agent (Claude Code, Codex CLI, Gemini CLI, OpenCode, Factory Droid, an agent hosted in Zed, and agents nobody has named yet) be observed by claudectl, reach the bus and the hive, and take delegated work. Define the relay wire protocol formally, version it, and make it faster to exchange.

**Relationship to the cross-agent hive (#521, `docs/cross-agent-hive.md`).** That spec makes Codex a full member: canonical tool vocabulary (#522, shipped), provenance (#523), Codex discovery (#524), the adapter registry (#533), the hive MCP surface (#525), Codex write surfaces (#526), and the bus's agent and machine dimensions (#527) and relay bridge (#528). This spec builds on it and doesn't repeat it. It covers agents beyond Codex, delivering into them, running work on them, and the relay wire protocol all of it rides on.

## Implementation status

| Phase (§11) | Issue | Status | Module / artifact |
| --- | --- | --- | --- |
| 1. Wire protocol v1: handshake negotiation, unknown-type tolerance | #559 | In review | `src/relay/{features,protocol,peer,listener}.rs`, `docs/relay-protocol.md` |
| 2. Agent manifests (other agents appear in the TUI and fleet) | #563 | Not started, after #533 | `crates/claudectl-core/src/agent/`, `process.rs` |
| 3. Per-agent delivery into a running agent | #560 | Not started | `src/bus/`, hook installers per agent |
| 4. ACP runner for delegated work | #561 | Not started | `src/relay/worker.rs`, new `src/acp/` |
| 5. Binary frames + LZ4 | #562 (answers #503) | Not started | `src/relay/protocol.rs`, `transfer.rs` |
| 6. Delta heartbeats | #564 | Not started | `src/relay/{advertise,protocol}.rs` |
| — Bus across machines | #528 (cross-agent hive) | Not started, needs #559 | see §8 |

---

## 1. Motivation

claudectl's name says Claude, and so does most of its code. People run more than one agent, though, often on the same repo. A Codex session reviewing what a Claude session wrote, OpenCode on the mini, a Droid in CI. Today those agents are invisible to the dashboard, can't be delegated to, can't receive bus mail without polling for it, and can't contribute to or benefit from the hive.

The fix is not a Claude-shaped hole with other agents squeezed in. It's a contract that any agent satisfies at whatever level it can, plus a wire protocol written down well enough that the next agent doesn't need a code change in four modules.

## 2. Three boundaries, two contracts

"Any agent joins the hive" mixes two different boundaries. The code already separates them, and this spec follows the code.

```
 machine A                                        machine B
┌──────────────────────────────┐                ┌──────────────────────────────┐
│ agent ⇄ claudectl            │   relay wire   │ claudectl ⇄ agent            │
│  (adapter contract, §3–§5)   │ ◀────────────▶ │  (adapter contract, §3–§5)   │
│                              │   protocol     │                              │
│  observe · reach · deliver   │   (§6–§8)      │  observe · reach · deliver   │
└──────────────────────────────┘                └──────────────────────────────┘
```

- **Peer ↔ peer** is claudectl ↔ claudectl over the relay. Machines join hives, not agents. §6–§8 define that wire format.
- **Agent ↔ claudectl** is local to one machine and has three directions:
  - **Observe:** claudectl discovers the agent's sessions and infers status, cost and context (§3).
  - **Reach:** the agent calls into the bus and the hive. MCP already does this for every agent (§4).
  - **Deliver:** claudectl pushes into the agent: bus mail, an interrupt, a delegated task (§5).

So the compatibility promise for phase 1 is **any agent on a claudectl node**. A non-claudectl implementation of the relay protocol is possible once §6 is published, but `docs/relay-and-hive.md` lists non-claudectl peers as a non-goal and this spec does not change that by itself (open question 1).

## 3. Observe: the agent manifest

### What exists today

- `trait AgentAdapter` with `AgentFamily {ClaudeCode, Codex}` (`src/coord/adapter.rs:93,119`). Only `claudectl coord adapters` calls it (`src/coord/cli.rs:1061`); #533 routes the rest through it.
- Codex discovery and monitoring is #524 (PR #532), as a second hand-written family in `claudectl-core/src/agent/codex.rs`. The tool vocabulary that maps each family's tool names is #522, shipped in `agent/vocab.rs`.
- Before #532, everything else is hard-coded to Claude:
  - discovery reads `~/.claude/sessions/*.json` and `~/.claude/projects/<slug>/*.jsonl` (`crates/claudectl-core/src/discovery.rs:7,17`)
  - the pid filter drops any process whose command line lacks `claude` (`process.rs:97`)
  - the transcript parser expects Anthropic's message schema (`transcript.rs:75-142`)
  - launchers spawn `claude` (`terminals/mod.rs:517`)

### Proposal: agents are data, not an enum

"Or whatever else" rules out an enum that needs a release per agent. Claude and Codex keep their hand-written families. Every agent after them is described by a manifest, and a manifest can also override a built-in. claudectl ships built-ins and reads overrides and additions from `~/.claudectl/agents/<name>.toml`.

```toml
# ~/.claudectl/agents/opencode.toml
name        = "opencode"
display     = "OpenCode"
binaries    = ["opencode"]          # pid filter and the `--self` ancestor walk

[observe]
sessions    = "none"                # "claude-sessions-json" | "glob" | "none"
transcript  = "none"                # parser tag: "anthropic-jsonl" | "codex-jsonl" | "none"
# glob      = "~/.local/share/opencode/sessions/*.json"

[run]
acp         = ["opencode", "acp"]   # §5.3: preferred headless path
headless    = []                    # fallback argv template; {prompt} is substituted
outcome     = "exit-code"           # parser tag for headless output

[deliver]
mode        = "pull"                # "claude-stop-hook" | "plugin-event" | "terminal" | "pull"

[install]
mcp_config  = "~/.config/opencode/opencode.json"  # where `init` registers the bus server
skills_dir  = ""                    # empty: hive skill artifacts are not installed for this agent
```

Rules:

- **Parser tags name code in claudectl.** A manifest picks a parser; it never supplies one. A new transcript format needs a release, and a new agent that reuses a known format doesn't.
- **Status degrades, it doesn't guess.** An agent with `transcript = "none"` shows its process, cwd, CPU and elapsed time. Status is `Unknown` and cost is `n/a`. `Unknown` is already the supervisor's no-actuation backstop.
- **Manifest strings never reach argv unchecked.** The same rule PR #557 applied to `os_user`: a value is matched against a closed character set before it's used in a command.

### Core seam

#533 decides where the adapter registry lives. If it lands in `claudectl-core` next to `SessionSource` (core can't reach up into `src/coord`), a manifest is one more registry entry built from data. `ClaudeSession` already gains `agent_family: String` in #532; for a manifest agent it holds the manifest `name`.

`bus`'s `list_agents` (`src/bus/mcp.rs:307`) and the `--self` ancestor walk (`src/bus/roles.rs:116`) read the manifest set instead of matching `"claude"`.

### Built-in set (to verify per agent before phase 2 ships)

| Agent | Binary | Observe | MCP config | ACP | Delivery hook |
| --- | --- | --- | --- | --- | --- |
| Claude Code | `claude` | sessions JSON + JSONL (today) | plugin `.mcp.json` | via adapter | `Stop` hook (today) |
| Codex CLI | `codex` | rollout JSONL (#524) | `~/.codex/config.toml` | via adapter | TBD |
| Gemini CLI | `gemini` | TBD | `~/.gemini/settings.json` | native | TBD |
| OpenCode | `opencode` | TBD | `opencode.json` | native | plugin `event` hook |
| Factory Droid | `droid` | TBD | `~/.factory/mcp.json` | registry | hooks (experimental since 1.10) |

"TBD" means not verified against the agent's own docs. Each row gets checked against primary sources, and the agent run end to end, before it's marked supported. Zed is a host, not an agent: agents running inside Zed are the same binaries, launched by Zed over ACP. They show up through their own row once Zed's child processes are visible to the pid filter.

## 4. Reach: MCP is the universal join path

Every agent above supports MCP. `claudectl bus stdio` already exposes `whoami`, `list_agents`, `publish`, `read_inbox`, `submit_task`, `list_tasks` and `task_status` to any MCP client (`src/bus/mcp.rs:540`). That's the strongest part of this design and it already ships. Registering it is the per-agent part: `claudectl init` writes the server into each detected agent's `install.mcp_config`, the same way it installs the Claude plugin today.

Role resolution is already agent-neutral after binding (`roles.rs:57-80`: `--role`, `CLAUDECTL_BUS_ROLE`, pid chain, cwd). Only `bind --self` needs the manifest's `binaries` list.

## 5. Deliver: getting into a running agent

### 5.1 Today

A Claude session gets bus mail when its `Stop` hook runs `claudectl bus stop-hook`, which answers with Claude's `{decision: "block", hookSpecificOutput: {...}}` (`src/bus/stop_hook.rs:22-60`). Every other agent is **pull-only**: it has to decide to call `read_inbox`. That pull is what makes the bus slow. A message sits until the agent happens to look.

### 5.2 Delivery modes, fastest first

| Mode | How | Latency | Agents |
| --- | --- | --- | --- |
| `claude-stop-hook` | hook response at end of turn | end of current turn | Claude Code (today) |
| `plugin-event` | the agent's plugin/hook system calls `claudectl bus drain --agent <name>` and injects the result | end of current turn | OpenCode, Droid, others with hooks (to verify) |
| `acp` | claudectl owns the session (§5.3) and sends a `session/prompt` | next turn boundary | any session claudectl started over ACP |
| `terminal` | `send_input` types a nudge into the agent's terminal | immediate | anything in tmux/Kitty/WezTerm |
| `pull` | agent calls `read_inbox` | whenever it looks | everything (floor) |

`terminal` is the fallback for agents without hooks. It's the least safe mode because it types into a live prompt, so it's opt-in per role and goes through the leading-`/` neutralization already in `bus/policy.rs`.

### 5.3 Running work: ACP instead of per-agent `--print` parsing

Today the worker runs `claude --print --output-format json <prompt>` and parses Claude's JSON (`src/relay/worker.rs:103`, `outcome.rs`). Doing that per agent means a new output parser for every agent.

The Agent Client Protocol (ACP) replaces that with one client. ACP is JSON-RPC 2.0 over stdio. Stable version 1; the version is bumped only for breaking changes, and everything else is negotiated through capabilities. A claudectl ACP client needs five methods:

| Method | Direction | Use |
| --- | --- | --- |
| `initialize` | client → agent | negotiate `protocolVersion`, exchange capabilities |
| `session/new` | client → agent | start a session with `cwd` and **`mcpServers`** |
| `session/prompt` | client → agent | send the task; the response carries `stopReason` (`end_turn`, `cancelled`, …) |
| `session/update` | agent → client | streamed progress (messages, plans, tool calls) |
| `session/cancel` | client → agent | `TaskInterrupt` maps here |

`session/new` takes a list of MCP servers. A delegated task started over ACP gets `claudectl bus stdio` attached at birth, whichever agent runs it. That's how a Droid on the mini receives bus mail without any Droid-specific install.

`DelegateTask` gains an `agent` field (the manifest name, default `claude` for compatibility). The worker picks the runner: the manifest's `run.acp` argv if present, else `run.headless`. `TaskStatus` reports `stopReason` mapped onto the existing task states.

## 6. Relay wire protocol v1

### 6.1 What exists today

- **Framing:** NDJSON over TCP. One `RelayMessage {id, msg_type, from_peer, timestamp, payload}` per line, 1 MiB line cap (`src/relay/protocol.rs:10-60`).
- **Auth:** the server sends a nonce; the client answers `HMAC-SHA256(PSK, nonce)`, compared in constant time (`protocol.rs:69-107`). Only the client is challenged.
- **Message types:** 17 variants (`src/relay/mod.rs:65-101`).
- **Versioning: none that works.** The handshake sends `version: CARGO_PKG_VERSION` (`protocol.rs:185`), and `verify_handshake` never reads it. `docs/relay-and-hive.md` says it does.
- **The hazard that gates everything else:** an unknown `msg_type` fails serde, `read_message` returns `Err`, and the reader loop breaks (`src/relay/peer.rs:366-371`). **Adding any new message type disconnects every older peer.** So every change below sits behind phase 1.

### 6.2 Handshake negotiation

The handshake payload gains two keys. Older peers don't send them, and that absence is meaningful.

```json
{ "proto": 1, "features": ["unknown-ok", "frame-bin", "lz4", "hb-delta", "agents", "bus-forward"] }
```

- `proto` is bumped only for a change an older peer can't survive. Every other addition is a feature string.
- A feature is **on for a link only when both sides advertise it**. No `proto` key means proto 0 with no features: today's behaviour, byte for byte.
- `HandshakeAck` echoes the agreed set so both sides log the same answer.

### 6.3 Unknown types

`MessageType` gets `#[serde(other)] Unknown`. An unknown frame is logged once per type per peer and dropped. It isn't fatal and it isn't acked. A peer only sends a new type to a peer whose agreed feature set includes the feature that introduced it, so `Unknown` is a safety net, not a channel.

### 6.4 Binary frames (feature `frame-bin`)

After `HandshakeAck`, if both sides agreed `frame-bin`, the link switches from NDJSON to length-prefixed frames:

```
┌──────────────┬───────────┬────────────────────────────┐
│ len: u32 BE  │ flags: u8 │ body: len-1 bytes          │
└──────────────┴───────────┴────────────────────────────┘
flags bit 0 = body is LZ4 block-compressed (raw size prepended as u32 LE)
flags bits 1-7 reserved, must be 0; a frame with a reserved bit set is a protocol error
body (after decompression) = one RelayMessage as JSON
```

- The challenge, handshake and ack stay NDJSON. The upgrade happens after them, so an old peer never sees a binary byte.
- `len` is capped at 1 MiB, matching today's line cap. **The decompressed size is capped at 4 MiB** and checked against the prepended size before allocating, so a small frame can't expand into a memory exhaustion.
- No base64 in frames. Base64 costs 33%, which would cancel most of what compression saves.

### 6.5 Message catalogue

v1 publishes the payload of every existing type in its own reference, `docs/relay-protocol.md` (#559): required keys, optional keys, and the issue that added each optional one. A test round-trips every message type and matches the enum exhaustively, so a type added to the code fails to compile there until it's listed.

### 6.6 Capability manifest

The heartbeat's `capabilities` (#536, `src/relay/capabilities.rs:54`) probes a fixed list of tools (`claude`, `git`, `cargo`, `python3`, `node`) and checks credentials for `claude` only. Under feature `agents` it reports one entry per installed agent manifest: name, binary version, whether `run.acp` or `run.headless` is available, and whether a credential is present. That's what a delegating peer uses to pick a machine for `agent = "opencode"`. The shape borrows from the A2A agent card (name, version, capabilities, skills) without adopting A2A's transport.

## 7. Speed

### 7.1 What the bytes actually are

Measured on this cluster, `cec14e59`:

| Payload | Size | Notes |
| --- | --- | --- |
| Bus message body | 29 B average, 119 B max (24 messages in `bus.db`) | compression makes these **bigger** |
| Heartbeat, 5 sessions | 7.5 KB | sent every 30 s per peer |
| `SessionTransfer` chunk | up to 512 KiB, 64 MB cap | base64 today (`transfer.rs:39-48`) |

### 7.2 Codec benchmark

Same three payloads. Pure-Rust crates where possible; size is the stripped release-binary delta over a no-compression baseline.

| Codec (crate) | Bus msg | Heartbeat | Transcript 512 KiB | Decode 512 KiB | Binary delta |
| --- | --- | --- | --- | --- | --- |
| LZ4 (`lz4_flex`, pure Rust) | 0.95× | 2.05× | 3.53× | 57 µs | **+17 KB** |
| Deflate-1 (`miniz_oxide`, pure Rust) | 1.11× | 2.60× | 2.79× | 1.17 ms | +50 KB |
| Deflate-6 (`miniz_oxide`) | 1.11× | 3.07× | 4.17× | 0.74 ms | +50 KB |
| Zstd-1 (`zstd`, C library) | 0.99× | 2.87× | 5.16× | 139 µs | +463 KB |
| Zstd-3 (`zstd`) | 0.89× | 2.95× | 5.69× | 158 µs | +463 KB |

### 7.3 Decisions

- **LZ4 via `lz4_flex`** is the one codec. Pure Rust, +17 KB on a 6.3 MB binary, decodes 512 KiB in 57 µs. Zstd compresses transcripts 1.5× better but costs 27× the binary size and a C build. The 64 MB transfer cap over a tailnet is the only case where the ratio would matter, and that can be revisited with a measurement of transfer time on the slow link.
- **Compress a frame only when the body is ≥ 1 KiB.** Bus messages and acks never qualify; heartbeats and transfers always do.
- **Heartbeats go delta (feature `hb-delta`).** A heartbeat carries `{seq, upsert: [...], remove: [pids]}` relative to the last one the peer acked, with a full snapshot every 10th beat or on request. Most beats change a few numbers in one or two sessions. No crate needed, and it beats any codec on the common case.
- **`SessionTransfer` drops base64 under `frame-bin`.** Chunks travel as LZ4 frames: roughly 4.7× less on the wire than base64 today (3.53× from LZ4 on top of 1.33× from base64).
- **"Fast" for the bus is latency, not bytes.** §5.2's delivery modes are what make mail arrive in a turn rather than whenever the agent polls. §8 is what lets it cross machines at all.

## 8. Bus across machines (feature `bus-forward`)

This is #528's to design (`docs/cross-agent-hive.md` §7.3), and the addressing is #527's. What this spec adds is the wire constraint: the bridge's message type goes only to peers that agreed `bus-forward` (§6.2), so it can't disconnect an older peer. The sketch below is one forwarding design #528 can take or replace.

The bus is a SQLite file per machine (`~/.claudectl/bus/bus.db`). A role on the mini can't be addressed from here. Rather than a second transport, a new relay type carries bus messages:

```json
{ "msg_type": "BusForward",
  "payload": { "to": "reviewer@baradas-mac-mini-508f9c0e", "from": "impl@barrys-mac-74371ce5",
               "subject": "review.request", "type": "request", "body": "…", "hop": 1, "msg_id": "…" } }
```

- **Addressing:** `role@peer`. A bare `role` stays local; nothing that works today changes meaning.
- **Directory:** with `bus-forward` agreed, the heartbeat carries the peer's bound roles (names only), so `list_agents` can show remote roles.
- **Policy runs on both ends.** The sending peer applies `bus/policy.rs` before forwarding and the receiving peer applies it again before inserting. Hop count carries across, so the existing cap of 8 holds cluster-wide.
- **Delivery:** the receiving claudectl inserts into its own store, and §5.2 delivers from there. A forwarded message is `Ack`ed after insert, so the sender can retry an unacked forward after reconnect without duplicates (`msg_id` is the dedupe key).

## 9. Hive content

The cross-agent hive covers the content model:
- #522 (shipped) normalises tool names to one canonical vocabulary.
- #523 records each unit's origin agents and adds `ArtifactRequires::agents`.

Manifests extend both:

- **Each manifest gets its own tool-name mapping.** A manifest agent has no hand-written mapping in `agent/vocab.rs`. Its manifest names which canonical tool each native tool is, under the same rule `docs/cross-agent-hive.md` §3.2 sets: map only a genuinely identical operation. A native tool with no listed mapping passes through unnormalised.
- **Install paths come from `install.skills_dir`.** An empty value means "don't install", not "install to `~/.claude`" (`hive/share.rs:490` today).

## 10. Security

- **What a peer can make claudectl run grows.** Under `agents`, a peer can pick which agent runs a delegated task. A peer's `agent` value must name an installed manifest; it's never a path or an argv fragment.
- **Peer and manifest strings never reach argv unchecked** (§3, and PR #557's `os_user` rule).
- **Decompression is bounded** (§6.4).
- **Auth stays as strong as today, and no stronger.** One-way HMAC, no mutual proof, no session key, and the PSK authenticates without encrypting. A published protocol makes that more visible, not weaker. Mutual challenge and an encrypted transport are their own change. Until then the guidance stays "run the relay over a private network (LAN, tailnet)".
- **A `bus-forward` peer can put mail in your agents' inboxes.** That's a delegation-level trust decision. It should follow the open-cluster trust tiers (`docs/open-cluster.md`), not be on for every paired peer by default.

## 11. Suggested build order

Each phase is independently shippable and has to be run on two machines before it's called done.

1. **Wire protocol v1 (#559).** `proto` + `features` in the handshake, `Unknown` tolerated, the catalogue in §6.5 with fixture tests. Tested by running a new peer against a 0.80 peer and confirming the old one stays connected.
2. **Agent manifests (#563, after #533).** Other agents appear in the TUI, `relay fleet` and `list_agents`, at observe-only depth. Proven on one agent beyond Codex first (open question 3).
3. **Delivery modes (#560).** `plugin-event` for that agent, `terminal` as opt-in fallback.
4. **ACP runner (#561).** `agent` on `DelegateTask`, `session/new` with the bus attached, `session/cancel` for interrupts.
5. **Binary frames + LZ4 (#562).** `frame-bin`, the 1 KiB threshold and 4 MiB bound, binary transfer chunks.
6. **Delta heartbeats (#564).**

#528 (`bus-forward`) can start any time after #559.

## 12. Open questions

1. **Non-claudectl relay implementations: yes or no?** §6 makes one possible. Supporting one means freezing the catalogue and treating changes as public API. Recommendation: publish the spec, keep the non-goal for now, and revisit when someone asks to implement it.
2. **One dependency (`lz4_flex`), or keep hand-rolling?** The crypto and base64 are hand-rolled to keep the dependency count down. A hand-rolled LZ4 decoder is small, but it parses bytes from the network. The crate is the safer choice; this is the maintainer's call (#503, #562).
3. **Which agent proves the manifest first?** Codex is covered by #524 with hand-written code. The first manifest-only agent should be one with native ACP and documented hooks; OpenCode fits both.
4. **Does `claudectl` keep the name?** Not for this spec to decide, but phase 2 is when the name stops describing the product.
