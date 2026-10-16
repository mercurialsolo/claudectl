# Relay Wire Protocol — Reference

**Protocol version 1** (#559). This is what two claudectl relays say to each other, message by message. The design reasoning lives in [Relay & Hive](relay-and-hive.md) and the agent protocol epic (#558); this page is the reference. A test round-trips every message type (`protocol::tests::every_message_type_round_trips`), so a type added to the code without being added here fails to compile there first.

## Framing

The challenge, handshake and ack are always NDJSON. After the ack, a link stays on NDJSON unless both ends agreed `frame-bin`, in which case every frame after the ack is binary.

**NDJSON**

- TCP, one JSON object per line, `\n`-terminated, at most 1 MiB per line.
- A blank line reads as end of stream.

**Binary frames (`frame-bin`, #562)**

```
┌──────────────┬───────────┬──────────────────────────┐
│ len: u32 BE  │ flags: u8 │ body: len - 1 bytes      │
└──────────────┴───────────┴──────────────────────────┘
flags bit 0: body is raw_len (u32 LE) followed by an LZ4 block of the JSON
flags bits 1-7: reserved, must be 0
```

- `len` is 1 to 1 MiB.
- A sender compresses only when the JSON is at least **1024 bytes**, and only when that makes it smaller.
- A receiver rejects any of these as a protocol error, and the link closes:
  - a compressed frame whose `raw_len` is below 1024 or above **4 MiB**
  - a block that doesn't decompress to exactly `raw_len`
  - any reserved bit set
- The 1024-byte threshold and the 4 MiB cap are part of what `frame-bin` means. Changing either needs a new feature string.
- No base64: it would cost back a third of what LZ4 saves.
- A read timeout before a frame's first byte is an idle link. After the first byte, the reader keeps reading until the frame is whole, because stopping mid-frame would leave every later read misaligned.

Every frame, in either framing, is a `RelayMessage`:

| Key | Type | Meaning |
| --- | --- | --- |
| `id` | string | Unique per sender (`gen_msg_id`) |
| `msg_type` | string | One of the types below, `snake_case` |
| `from_peer` | string | Sender's relay identity (empty in `challenge`) |
| `timestamp` | u64 | Epoch ms at send |
| `payload` | object | Per type, below |

## Compatibility rules

1. **`proto` changes only for a break.** Everything else is a feature string, negotiated per link in the handshake.
2. **A feature is on only when both ends advertised it.** A new message type, or a new key whose absence an older peer would misread, is sent only on a link that agreed to the feature that introduced it.
3. **Unknown `msg_type`: skip, don't fail.** A reader that sees one logs it once per peer and type and reads the next frame. Peers from before #559 instead close the reader, which is why rule 2 exists.
4. **Optional keys are omitted, not sent empty,** where "didn't say" and "has none" mean different things (`capabilities`, `listen_addrs`).
5. **Peer-supplied strings are bounded and filtered** before they're kept, logged or used: feature names, versions, addresses.

### Features

| Feature | Since | Meaning |
| --- | --- | --- |
| `unknown-ok` | proto 1 (#559) | This peer skips unknown message types instead of dropping the link |
| `frame-bin` | proto 1 (#562) | After the ack, length-prefixed binary frames, LZ4 at or above 1024 bytes (see Framing) |

## Connection setup

```
dialler                                  listener
   │ ───────────── TCP connect ───────────▶ │
   │ ◀──────────── challenge ────────────── │
   │ ───────────── handshake ─────────────▶ │  verify HMAC, intersect features
   │ ◀──────────── handshake_ack ────────── │
   │ ═════════ link (or one-shot) ═════════ │
```

### `challenge` (listener → dialler, first frame)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `nonce` | string, 64 hex | yes | Fresh per connection |

### `handshake` (dialler → listener)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `proof` | string, hex | yes | `HMAC-SHA256(PSK, nonce)`; compared in constant time |
| `version` | string | no | Crate version. Logged, never compared |
| `proto` | u64 | no (#559) | Absent: a peer from before #559, treated as proto 0 with no features |
| `features` | string[] | no (#559) | Names match `[a-z0-9-]{1,32}`; at most 64 kept |
| `listen_port` | u16 | no (#484) | Where the dialler accepts connections. Absent for one-shots; 0 is ignored |
| `transient` | bool | no (#487) | Only `true` counts: a one-shot delivery, never registered as the link |

### `handshake_ack` (listener → dialler)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `status` | string | yes | `ok` or `denied`; anything else is treated as denied |
| `proto` | u64 | no (#559) | `min(theirs, ours)`. Sent only if the handshake had `proto` |
| `features` | string[] | no (#559) | The agreed set. The dialler keeps only names it also has |

A dialler from before #559 gets exactly `{"status": "ok"}`.

## Link messages

### `heartbeat` (both directions, every 30 s by default)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `worker_id` | string | no | Defaults to `from_peer` |
| `timestamp` | u64 | no | Not read |
| `sessions` | object[] | no | `ClaudeSession::to_json_value` per local session. Worker state is stored only when present |
| `capabilities` | object | no (#536) | `{claudectl_version, os, arch, tools: {name: {state: "present", version?, credential?} \| {state: "missing"}}}`. Omitted, never empty |
| `listen_addrs` | string[] | no (#541) | `ip:port` literals the peer can be reached at; unparseable entries dropped. Omitted when empty |

A liveness-only heartbeat has payload `{}`.

### `ack`

Defined, never sent or handled. Reserved.

## Delegation

### `delegate_task` (delegator → worker, one-shot)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `task_id` | string | yes | `task_<epoch>_<seq>` |
| `prompt` | string | yes | |
| `cwd` | string \| null | no | Always sent; `null` when unset |
| `context` | object | no | `DelegationContext`: `git_remote`, `git_ref`, `git_commit` (each optional), `relevant_files` (`{path: content}`, ≤ 50 KB), `brain_context`, `dependency_graph {blocks[], blocked_by[]}`. Defaulted if absent or unparseable |

### `task_status` (worker → delegator, over the link)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `task_id` | string | yes | Dropped without it |
| `state` | string | no | Defaults to `running` |
| `stats` | object | no | `{tokens_used, cost_usd, context_pct, files_modified[], elapsed_secs?, last_activity?}` |

### `task_handoff` (worker → delegator, final)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `task_id` | string | yes | |
| `state` | string | no | `completed` (default when absent) or `failed`; any other value reads as failed |
| `summary` | string | no | |
| `artifacts` | string[] | no | Always `[]` today |
| `total_cost_usd` | f64 | no (#493) | |
| `total_tokens` | u64 | no (#493) | |
| `model` | string \| null | no (#493) | |
| `usage` | object | no (#493) | The agent's usage object, passed through |
| `git_ref` | string | no | Read by the delegator; no sender sets it yet |

### `task_interrupt` (delegator → worker, one-shot)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `task_id` | string | yes | |
| `interrupt_type` | string | yes | `stop` (kill, then a failed handoff) or `nudge` (reply with status); others ignored |
| `reason` | string | no | |

## Hive

### `knowledge_sync` (peer → peer, periodic and on propagation)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `units` | KnowledgeUnit[] | in effect | `hive/mod.rs` `KnowledgeUnit`. Missing or unparseable reads as none |
| `sync_epoch` | u64 | no | Not read |

### `knowledge_request`

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `since_epoch` | u64 | no | 0 or absent: full snapshot |

Handled on receipt; nothing in production sends it yet.

### `knowledge_snapshot` (reply to `knowledge_request`)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `units` | KnowledgeUnit[] | in effect | |
| `page`, `total_pages` | usize | no | 1-based paging. Read only in tests |

### `hive_join_request` (joiner → host, #434)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `hive_id` | string \| null | no | Refused if present and wrong |
| `label` | string \| null | no | Advisory name for the owner's queue |
| `grant` | string \| null | no (#435) | `hive.read:<hive>` token; admits as a reader |

Sent as explicit `null`s rather than omitted.

### `hive_join_result` (host → joiner, on the link or later as a one-shot)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `state` | string | yes | `member`, `pending` or `refused`; anything else is an error |
| `hive_id` | string \| null | for member and pending | |
| `name` | string \| null | no | |
| `reason` | string \| null | no | |
| `role` | string \| null | no (#435) | Absent: contributor. Unrecognised: reader |

### `knowledge_rejected` (host → sender of refused knowledge, #435)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `reason` | string | no | |
| `units` | usize | no | How many were dropped |

## Conversation transfer (#478)

### `session_transfer` (sender → receiver, one-shot, one chunk per frame)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `session_id` | string | yes | Validated |
| `cwd` | string | yes | Sender's cwd; the placement directory |
| `seq` | u32 | yes | 0-based, `< total` |
| `total` | u32 | yes | |
| `digest` | string | yes | Hex SHA-256 of the whole transcript |
| `data` | string | yes | Base64 of up to 512 KiB |

A frame missing any key is dropped.

### `session_received` (receiver → sender, reply on the same connection, #511)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `session_id` | string | yes | |
| `path` | string | yes | Where the transcript was placed |
| `cwd` | string | no | Not read |
| `resume` | string | yes | Command to resume it on the receiver |
