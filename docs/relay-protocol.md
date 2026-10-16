# Relay Wire Protocol — Reference

**Protocol version 1** (#559). This is what two claudectl relays say to each other, message by message. The design reasoning lives in [Relay & Hive](relay-and-hive.md) and the agent protocol epic (#558); this page is the reference. A test round-trips every message type (`protocol::tests::every_message_type_round_trips`), so a type added to the code without being added here fails to compile there first.

## Framing

- TCP, one JSON object per line, `\n`-terminated, at most 1 MiB per line.
- A blank line reads as end of stream.
- Every frame is a `RelayMessage`:

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
| `lease-fleet` | proto 1 (#569) | This peer can ask a lease authority for fleet-wide leases, or be one (see Fleet leases) |

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

## Fleet leases (#569)

Sent only on a link that agreed `lease-fleet`; a requester that finds the authority without it refuses before sending.

**The authority.**
- One peer, named by `[relay] lease_authority` on every machine, keeps the fleet's leases in its own coord store.
- A fleet lease's owner is `<session>@<peer>`. The peer is the id the relay authenticated, never one read from the payload.
- The authority computes `expires_at` on its own clock.

**Requests and answers.** A requester sends one of the three requests on a one-shot dial, and the authority answers each with a `lease_result` on the same connection.

**Renewal sends no message.** On every session collection (about 5 s), the authority renews each fleet lease whose session is in that peer's latest heartbeat `sessions[].session_id`, from a heartbeat no older than 90 s. A session that ends, or a peer that stops heartbeating, stops renewing, and the lease expires at its TTL.

### `lease_claim` (requester → authority)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `request_id` | string | yes | Echoed in the result. Printable, no `@`, ≤ 96 bytes |
| `lease_id` | string | yes | Chosen by the requester and reused across retries, so a repeat of a granted claim is answered `granted` instead of creating a second lease |
| `session` | string | yes | ≤ 128 bytes, no `@` |
| `kind`, `value` | string | yes | `kind`: 1–32 of `a-z 0-9 _ -`. `value`: 1–512 printable. `path_glob` overlaps by prefix; other kinds match exactly |
| `mode` | string | yes | `exclusive` or `advisory` |
| `ttl_secs` | u64 | yes | 1 to 7 days |
| `reason` | string | no | Control characters dropped, ≤ 256 |
| `wait` | bool | no | Queue instead of failing when held |
| `waiter_id` | string | no | Present on every poll after the first `queued` |

### `lease_release` (requester → authority)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `request_id` | string | yes | |
| `lease_id` | string | yes | |
| `session` | string | yes | Must match the holder: only `<session>@<requesting peer>` may release |

### `lease_list` (requester → authority)

| Key | Type | Req | Meaning |
| --- | --- | --- | --- |
| `request_id` | string | yes | |

### `lease_result` (authority → requester)

| Key | Type | Meaning |
| --- | --- | --- |
| `request_id` | string | From the request |
| `outcome` | string | `granted`, `conflict`, `queued`, `waiter_gone`, `released`, `not_owner`, `not_found`, `not_active`, `list` or `error` |
| `lease` | object | With `granted`: the lease, including the authority-computed `expires_at` |
| `holder`, `holder_lease`, `expires_at` | string | With `conflict`/`queued` when a lease is in the way |
| `ahead` | u64 | With `conflict`/`queued` when only the queue is in the way |
| `waiter_id` | string | With `queued`: present it on the next poll |
| `leases` | object[] | With `list`: the active fleet leases |
| `message` | string | With `error` |
