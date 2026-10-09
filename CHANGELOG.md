# Changelog

All notable changes to claudectl are documented here.

## [Unreleased]

### Fixed

- **A delegated task's cost and token count were always reported as zero**,
  on a tool whose premise is cost tracking. The worker's counters were
  initialised to zero and only ever read, so a task running a real Claude
  session on another machine looked free, and 0.75.0's ledger faithfully
  recorded that.

  The worker now runs the session with `--output-format json` and reports what
  Claude Code itself accounts for: the cost, the token breakdown, and which
  model ran. That is preferred over re-deriving the figures locally, so the two
  cannot disagree and there is no second pricing table to keep current.

  A completed task's summary is now what the session actually produced rather
  than the fixed phrase "Task completed successfully".

  Verified across a laptop and a Mac mini: a delegated question came back
  `completed: 42 ($0.1994)`, with 36,639 tokens and the model recorded.

- **A delegated task that could not start reported nothing**, leaving the
  delegating host waiting indefinitely while the reason sat on the other
  machine's console. It is now sent back like every other outcome. A missing
  working directory also said "spawn claude: No such file or directory", which
  pointed at a Claude Code install that was in fact fine; it now names the
  directory.

- **A failed delegation reaching the far host now explains itself.** A worker
  that is not logged in reports "Not logged in · Please run /login" to
  whoever delegated the task, rather than failing silently.

- Failures to deliver a worker's reply are no longer discarded. A finished
  task whose reply went nowhere previously produced no sign of it anywhere.

## [0.76.0] - 2026-10-09

### Added

- **`relay status` reports the tasks you have actually delegated.** It used to
  print a hardcoded "no active delegated tasks" and a note saying live status
  needed `relay serve` — a constant, and so wrong in every state.

  A delegated task now gets a record on disk the moment it is sent, which the
  serve loop updates as the far host reports progress and completion, so
  `relay status` answers from the ledger and needs no live connection. It has
  to work this way: `relay delegate` exits within a second while the reply
  arrives minutes later at a different process, so there is nothing in memory
  that could connect the two.

  Completion and failure are final, so a report redelivered after a reconnect
  cannot reopen a finished task; a report for a task this host never delegated
  is ignored rather than invented.

  Verified across a laptop and a Mac mini: records appear at send time, a real
  Claude session on the far host moves one to `completed` with its summary, a
  failing task settles as `failed` with the reason, and `relay status` in a
  fresh process reads all of it back.

### Known limitations

- A delegated task's cost and token count are reported as zero. The worker
  never measures them, so the ledger records what it is told. Tracked
  separately.
- A delegated task that fails to *start* on the far host — an unreadable
  working directory, say — reports nothing back, leaving its record open.
  The error is printed on the worker's own console instead. Tracked
  separately.

## [0.75.0] - 2026-10-09

### Fixed

- **`relay delegate` reported success and did nothing whenever the two peers
  already held a live connection.** A regression in 0.72.0, and the normal
  case rather than an edge one since 0.73.0 made a live link ordinary for any
  pair that has connected once.

  `relay delegate` is a one-shot process: it dials, sends, and exits. That
  connection authenticated as the same peer as the lasting link, so the
  accepting side saw two connections for one peer and applied 0.72.0's
  collision rule, closing the new one before its message was read. The rule is
  right for two serving peers and wrong here — it was being asked a question
  about the wrong kind of connection.

  The handshake now says which kind it is, and the accepting side delivers a
  one-shot's messages without registering it as a peer link, so it can neither
  displace the link nor be displaced by it. Older peers omit the field and are
  treated as peer links exactly as before.

  Two things that kept this invisible are fixed with it: sending treated a
  successful write as delivery and now reports an error when the peer closes
  without accepting the message, so this failure exits non-zero instead of
  printing "delegated"; and it consulted only the newest stored address rather
  than all of them, so sending to a peer that had moved could fail with a
  working address on record.

  Verified on two machines in both directions, since the broken behaviour
  depended on how the two peer ids sort: the delegation reaches the worker,
  the task runs, status and completion both come back, and neither side
  reports a disconnect. With the far side stopped, the command exits non-zero
  and names the error.

  This also makes delegated tasks report their completion back to the host for
  the first time, which had never worked.

## [0.74.0] - 2026-10-09

### Fixed

- **A peer that had only ever dialled in was undialable, and peer records
  held one address that every write overwrote.** `save_peer_meta` is called
  from the four dialling paths and never from the listener, so the host of an
  invite held a pairing key and nothing to connect to; and because each write
  rewrote the whole record, a laptop that paired on a LAN and later moved
  networks had no other address to try.

  The handshake now carries the dialler's listening port, and the acceptor
  records the peer at the IP it observed plus that port — after authentication
  succeeds, so an unauthenticated connection cannot write to the address book.
  The port has to come from the peer and the address from us, since neither
  side knows both. It is sent only when bound to a wildcard address, because a
  port bound to one specific address is not reachable where the peer sees us
  from. Older peers send no such field, which means "nothing learned" rather
  than an error.

  Peer records now hold an ordered list of addresses, most recent first,
  deduplicated and capped. Both dial paths try them in turn and stop at the
  first that answers as the expected peer, so losing one address no longer
  costs the connection. Records written by earlier versions read as a
  single-address list.

  Verified on two machines: as invite host, the Mac mini recorded the
  laptop's listening address rather than an ephemeral port; both then dialled
  on restart and held with no disconnects; and with a dead address ahead of
  the mini's Tailscale address, the laptop logged the dead one once and
  connected on the next.

## [0.73.0] - 2026-10-08

### Fixed

- **Two machines that both ran only `relay serve` never connected.** Each
  listened and neither dialled: the registry's reconnect event needs a peer
  already present and already marked disconnected, and a fresh process starts
  empty. `relay serve` now dials, at startup, every peer it holds both a
  pairing key and a stored address for — on its own thread, since a connect
  carries a 10s timeout and dialling inline would hold the banner for 10s per
  powered-off peer. A stale address is one quiet log line naming it as the
  peer's address *as of pairing*, so it can be told apart from a host that is
  simply off.

  Verified by pairing two servers the real way and restarting both with
  nothing running `join`, on one machine and then across two.

- **Simultaneous dials killed both connections.** Letting both ends dial made
  a new case reachable: each end holds two authenticated sockets for the one
  peer, an inbound and an outbound. Both dial threads saw the peer already
  present — installed by their own listener — and discarded the socket they
  had just opened, so each was left holding the connection the other had just
  closed. Both died, and neither reconnected, because an inbound connection
  carries no address to re-dial.

  Only an asymmetric rule lets two ends agree, so the registry now keeps the
  connection opened by the lower peer id. A dead connection is still always
  displaced, whatever the ids say, or a peer that crashed and reconnected
  could never replace the zombie the other side holds.

  Verified on two physical machines: the lower id kept the socket it opened,
  the higher id deferred to it, and the link stood for 464 seconds — more
  than fifteen heartbeat intervals — with zero disconnects on either side.

## [0.72.0] - 2026-10-08

### Fixed

- **On a system whose `ps` cannot answer `-o`/`-p`, every session read as
  dead.** busybox `ps` — Alpine and other minimal images, a normal place to run
  an agent — rejects those flags but still *runs*, so the call returns with a
  non-zero status and nothing on stdout rather than failing outright. The parse
  then found no rows and marked every session `Finished`. That was survivable
  until 0.70.0 made the verdict sticky, at which point a whole machine reported
  dead. A non-zero exit is ambiguous, since procps means "no such process", so
  `ps -o pid= -p 1` now distinguishes a crippled `ps` from a genuinely absent
  process, and only then does liveness fall back to `kill(pid, 0)`. `EPERM` is
  also no longer read as death — a process owned by another user exists.

  Verified by running the shipped Linux musl artifact in Debian and Alpine
  containers on an arm64 Linux VM: Debian reported a live session as `Idle` and
  its cost as $7.50 throughout, while Alpine previously could not see processes
  at all.

## [0.71.0] - 2026-10-08

## [0.70.0] - 2026-10-08

### Fixed

- **A session whose process had exited was reported as `Idle`, not `Finished`.**
  `process::fetch_and_enrich` reads liveness from `ps` and marks an exited
  session `Finished` — then `monitor::update_tokens` ran afterwards and
  `infer_status` reassigned on every path, the last being `Idle`. The verdict
  was computed correctly and thrown away two steps later, so a dead session was
  indistinguishable from a live idle one until its pointer file aged out 24
  hours later. `infer_status` now preserves it, and `fetch_and_enrich` clears a
  stale `Finished` when `ps` says the pid is alive, so liveness is re-decided
  each tick in both directions.
- **The budget hook judged the wrong session.** `budget-check.sh` pulled the
  first `cost_usd` out of `claudectl --json` and compared *that* to the budget,
  whichever session it belonged to; `PROJECT_DIR="$PWD"` was assigned and never
  read. With six sessions open, a project at $3.58 was denied because an
  unrelated one sat at $263.52, and an over-budget session passed whenever a
  cheap one came first. Matching a hook process to a session means walking the
  process tree, which `sed` cannot do, so it now lives in
  `claudectl --budget-check <pid>` and the hook is a thin shim over it. The
  pid's ancestors are searched too, since a hook may run under an intermediate
  shell.

The TUI's own budget enforcement was already per-session and is unchanged.

## [0.69.0] - 2026-10-08

### Fixed

- **Session cost was overstated 6x.** Two independent faults, both measured
  against a live session recomputed from its own transcript ($1463.67 reported,
  $248.79 actual):
  - **A turn's tokens were counted once per content block (1.87x).** Claude Code
    writes one assistant turn as several JSONL lines — `thinking`, `text`,
    `tool_use` — each repeating the turn's entire `usage`. Totals are now counted
    once per `message.id`. The subagent rollup had the same fault.
  - **The price table was a model generation behind (3.3x).** `shorten_model`
    collapsed any unrecognised id to its bare family name, so `claude-opus-5`
    matched the `"opus"` arm and billed at retired Opus 4.1 rates, $15/MTok
    against a real $5. Versions are now extracted from the id, rates come from
    `docs/pricing-source.md`, and an unversioned family name resolves to a
    labelled fallback instead of inheriting a sibling's prices.
- **1-hour cache writes are priced at 2x base input**, not the 5-minute 1.25x.
  Every cache write in a long session is 1-hour TTL, so that line was understated
  by a third. Transcripts with no TTL breakdown still price at the 5-minute rate,
  which is the documented default.
- **`context_max` for Sonnet 4.6 and Sonnet 5 was 200k**, understating context
  saturation fivefold. Claude 4.6 and later carry the full 1M window.

Status and context reporting were checked against the same live sessions and
were already correct; `context_pct` is unchanged by this work.

**Known limitation:** `history.rs` persists computed dollars and token counts
without the cache-read/cache-write split, so rows recorded before this release
cannot be recomputed. Past figures in `claudectl history` and `stats` stay as
they were recorded.

## [0.68.0] - 2026-10-07

### Fixed

- **A second connection from the same peer no longer silently kills the first
  (#459).** `PeerRegistry::add_peer` replaces an existing connection, which is
  right for a reconnect — but it only *forgot* the old one. Its socket is shared
  with its reader thread through an `Arc`, so dropping the registry entry closed
  nothing: the socket stayed open, still delivering inbound messages, with
  nothing able to send on it. Since `claudectl hive join` dials its own
  short-lived connection, running it beside a `relay join` from the same machine
  left the host reading from a peer it could no longer answer, and hive
  knowledge stopped flowing one way with nothing logged. The displaced
  connection is now closed, so its reader exits, the other end sees the close
  and redials, and gossip resumes in about ten seconds instead of never.
- **An abruptly closed connection is noticed at once rather than after 90
  seconds.** The reader thread already exited on EOF; nothing asked it, so
  liveness rested entirely on three missed 30-second heartbeats. `check_alive`
  now also treats a finished reader thread as the connection being gone.

## [0.67.0] - 2026-10-07

### Fixed

- **Hive knowledge now actually propagates (#455).** Both ends of a connection
  sync every 12 seconds, incrementally, in addition to the existing
  on-distillation push. Previously a peer that dialled out had no gossip code
  at all — `relay join` built a connection that could not carry knowledge in
  either direction — and the serving side synced only at the instant it
  distilled something, with no catch-up, so a peer that connected a minute
  later never saw that unit.
- **A refused knowledge batch is re-offered once the gate opens (#455).** The
  gossip engine records what it has sent a peer when it *builds* the batch,
  since there is no acknowledgement to wait for. A #434 membership refusal
  therefore left it believing in a delivery that never happened — persisted to
  disk, so even a restart kept the false belief, and the peer was never offered
  those units again. `GossipEngine::forget_peer` now resets that when our own
  standing in the hive changes, which is the event that means "try again"; the
  refusal itself is not, since nothing changed on our side and a peer the host
  will always refuse would otherwise re-offer its whole store every tick.
- **A read-only member no longer pushes its own knowledge upstream (#455).**
  #435 was enforced only at the receiving end; the reader's own side now
  declines to send, and a peer whose join request is still pending does not
  send either.
- Outbound `KnowledgeSync` is logged. Every inbound hive message was logged and
  no send ever was, which is part of why the above went unnoticed.

### Changed

- The gossip half of both relay loops lives in one module, `relay::hivesync`,
  rather than inline in `cmd_serve`. The duplication between the serve and
  connect loops is what let them diverge far enough for the connect loop to end
  up with no gossip arms at all.

## [0.66.0] - 2026-10-07

### Fixed — a relay code of only letters could not be redeemed

- `relay join` and `hive join` picked the invite format with a guess — "every
  dash-separated segment is short and alphabetic" meant a word phrase. Base32
  codes contain only `A`–`Z` and `2`–`7`, so a code that happened to draw no
  digits satisfied that too, was handed to the phrase decoder, and failed with
  `invalid word phrase` — blaming the wrong format. About **1 in 78** codes, and
  about 1 in 18 before the payload widened.
- Both formats are fixed-length and the lengths differ (seven groups versus
  thirteen words), so the choice is now exact rather than guessed, and the
  predicate lives next to the codec that defines the lengths.

### Added — read-only hive membership (#435)

- **`hive.read:<hive-name>` grants are issuable, and admit a reader**: a peer that
  meshes and receives the hive's knowledge and can never contribute any — §7.5's
  "participation without symmetry". `claudectl access grant --scopes hive.read
  --project <hive-name>` mints one; `claudectl hive join <invite> --grant <token>`
  redeems it.
- Both halves are needed: the invite is transport authentication, the grant is
  the hive-level role. A grant admits directly even on an `ask` hive, because the
  owner already decided when they minted it.
- **A reader's attempted contribution is refused on the wire**, not silently
  dropped — the host answers with the reason and how many units it discarded, so
  a reader cannot mistake refusal for a network fault.
- `hive requests` grew a ROLE column and names how many members are readers.
  `hive status` says which this machine is. Member records carry the grant id
  they were admitted on.
- `hive identity set --name` now warns when renaming would orphan live
  `hive.read` grants, because the hive name is the scope qualifier.
- Readers need nothing from `hive trust`: `TrustTier` weighs a peer's claims and
  a reader makes none, so merging, drift detection and concordance checking are
  untouched.

### Known limitations
- **The roster is authoritative after admission.** Revoking a `hive.read` grant
  stops new admissions but does not demote an existing reader; remove
  `~/.claudectl/hive/members/<peer>.json` to do that. Making revocation reach the
  roster is a follow-up.
- **Hive gossip is more inert than it looks, and #435 did not change it.** A peer
  that dials with `relay connect` neither sends nor merges knowledge —
  `run_connect_loop` has no gossip at all — and `relay serve` only syncs when the
  brain distills something new, and never dials out. So knowledge moves only from
  a serving host to peers connected to it at the moment of a distillation. The
  membership gate is unit-tested on the exact function the serve loop calls; it
  could not be socket-tested, because in this architecture a dialing peer cannot
  send a `KnowledgeSync` at all.

### Fixed — peer pairing never worked on macOS
- **`claudectl relay join` could not pair with anything on macOS.** The accept
  loop puts the *listening* socket in non-blocking mode so it can poll for
  shutdown; on macOS and the BSDs an accepted socket inherits `O_NONBLOCK` from
  its listener (Linux does not), and a non-blocking socket ignores
  `set_read_timeout`. So the host's first read of the handshake returned
  `EAGAIN` immediately instead of waiting, every time. The host logged
  `handshake read failed … Resource temporarily unavailable (os error 35)` and
  the joiner reported `auth failed: connection closed before ack`. The accepted
  connection is now put back into blocking mode before the handshake, which is
  what the timeouts around it always assumed.

### Fixed — relay codes and word phrases could never be redeemed
- **Only the `cctl://` link could authenticate; the code and the phrase never
  could.** The link carries 8 PSK bytes and both sides rebuild the key with
  `crypto::parse_psk`. The relay code and word phrase packed only **4** bytes
  and then derived their own key with `sha256(seed4)` — a different 32-byte key
  than the link produces, and a different one than the inviter stores in
  `_pending.key`. A joiner using a code or phrase therefore presented a key the
  host could not match, and got `handshake denied`. All three formats now pack
  the same 8 bytes and go through the same single derivation, so the bug class
  cannot recur. The round-trip tests did not catch this because each codec was
  only ever checked against itself; the new tests assert the decoded key equals
  the canonical key an inviter actually stores.
- A relay code is now 13 bytes — seven groups of three characters instead of
  five — and a word phrase is 13 words instead of 9. Codes and phrases minted by
  v0.65.0 or earlier are refused with an explanation rather than decoded into a
  key nothing matches. Nothing is lost by this: they could not be redeemed.

### Fixed — `relay invite --json` minted invites that could not be redeemed
- `cmd_invite` returned from inside its `--json` branch *before* storing the
  pending PSK, so a scripted invite printed a perfectly valid-looking link that
  the serve side had no key for. The pending key is now claimed before either
  output path.

### Added — hive invite links and `join_policy` enforcement (#434)
- `claudectl hive invite` mints an invite to *this machine's named hive*, as a
  link, a relay code, a word phrase or a QR code. The link is
  `cctl://hive/<hive_id>?a=<identity>@<host:port>&k=<psk>&n=<name>&p=<policy>`.
  The spec wrote this as `cctl://hive/<id>?k=&n=`, which cannot be used — it
  names a hive but no machine, so a holder has nothing to connect to; the
  address rides in `a=`. Codes and phrases have no room for a hive id, so they
  pair with the machine and ask to join second.
- `claudectl hive join <link|code|phrase>` pairs and asks to join. It warns if
  nothing is listening on the port it is about to hand out.
- **`join_policy` is now enforced, by the host.** `ask` queues the request for
  its owner instead of admitting on possession of a link;
  `claudectl hive requests` lists who is waiting, with `approve` and `deny`.
  `invite` and `open` admit a paired peer that asks. A link naming a different
  hive is refused.
- Knowledge is only exchanged with hive **members**, in both directions — a
  peer that is merely pending neither receives units nor can contribute any.
- Naming a hive admits every already-paired peer, so gossip does not silently
  stop for anyone who named a hive after pairing. A peer the owner has denied is
  not admitted this way. A hive with no name gates nothing and behaves exactly
  as before.
- `claudectl hive status` shows where this machine stands and how many peers are
  waiting to join. New hook: `hooks.on_hive_join_request`.
- Membership is one create-only file per peer, so the gate is a single `stat` and
  two processes deciding at once cannot lose an update — the same
  monotonic-fact-as-create-only-file pattern as the escalation verdicts in #446.

### Known limitation
- At the host, `invite` and `open` are the same rule today, because the PSK is
  per-host rather than per-invite: pairing is the credential, so the host cannot
  tell which link a peer used. The two differ in owner intent and in what is
  advertised over LAN. Per-invite tokens are the follow-up that would make
  `invite` enforce what its name says.
- Knowledge still syncs only when the host distills something new while a peer
  is connected; there is no catch-up on connect. A peer that joins after a
  distillation has to wait for the next one. Pre-existing, and not changed here.

### Fixed — LAN discovery never worked at all
- **`relay discover` has returned "No claudectl instances found" since it
  shipped**, even with a relay running on the same network. The scanner was
  fine; nothing was broadcasting. `start_announcer` and `send_announcement` had
  zero callers anywhere in the tree, and `#![allow(dead_code)]` on
  `relay/mod.rs` is why that never surfaced as a warning. `docs/relay.md` said
  "Peers running `claudectl relay serve` announce themselves automatically",
  which was untrue, and the empty-state message told you to start the thing that
  was already running.
- Two bugs, the second only visible after fixing the first: `cmd_serve` never
  started the announcer, and `start_announcer`'s flag is a *shutdown* flag while
  `cmd_serve`'s is a *running* flag — so passing it straight through stopped the
  thread on its first check, printing "announcing every 5s" above a process
  sending nothing. Measured: zero bytes on UDP 9848 over six seconds before, a
  datagram every five seconds after.
- A send error was discarded with `let _ =`; the first failure is now logged and
  warned about once, since a silently failing broadcast is indistinguishable
  from a working one.
- Announce interval and scan duration are now paired constants — the old
  hardcoded 3-second scan would have missed peers against a 5-second announcer.
  `[relay] lan_announce` (default true) turns the broadcast off.

### Added — named hives are advertised and discoverable (#433)
- **`claudectl hive discover`** lists hives rather than machines: one row per
  distinct hive id, with join policy and the peer and knowledge-unit counts each
  advertises, grouped so several machines in one hive collect together.
  `relay discover` grew a HIVE column.
- The hive block rides the **existing** UDP datagram as additive fields, so a
  peer on an older build reads the three keys it knows and ignores the rest. The
  pre-#433 extraction is kept as a test fixture and run against a new payload,
  so "an older build is unaffected" is checked rather than asserted.
- **An unnamed hive adds no `hive` key at all**, so a machine that never ran
  `hive identity set` sends exactly what it sent before and appears only in
  `relay discover`.
- **The advertised policy is the effective one.** A hand-edited `open` that was
  never confirmed goes on the wire as `invite` — #432 put that gate on the
  stored record rather than on its CLI precisely so this path could not leak it,
  and there is now an end-to-end check that it does not.
- `description` is deliberately not advertised (200 bytes against a 1 KB receive
  buffer), and a test asserts the worst-case payload — longest name, longest
  identity, `u32::MAX` counts — still fits.
- Counts are read from atomics the serve loop updates each tick, so the
  announcer never shares a lock with the thread doing a blocking `send_to`.

### Fixed — a flaky test introduced in #438
- `brain::decisions` mutates `HOME` process-wide with a comment claiming cargo
  runs tests sequentially; it does not. #438's agent tests read `HOME`, so the
  two raced and the suite failed intermittently. The agent paths now take an
  explicit home, and no test in that module reads the environment. The
  underlying `brain` test is untouched and still a latent hazard for anything
  else that reads `HOME`.


### Added — hive identity: name, description, join policy (#432)
- **A hive can be named.** `claudectl hive identity` shows it, `identity set
  --name X [--description Y] [--join-policy P]` sets it, `identity clear --yes`
  removes it. Stored at `~/.claudectl/hive/identity.json` (0600, atomic
  temp+rename), and it is the prerequisite for advertising, discovering or
  joining a hive *as such* — you cannot advertise what has no name.
- **Absent is the default and changes nothing.** `identity::load()` returns
  `Option`, so every future consumer writes `if let Some(id) = load()?` and the
  unnamed path is the code that already shipped. A user who never names their
  hive sees no behavioural difference. A *malformed* file is an error rather than
  a silent "unnamed", because reverting to unnamed would stop advertising without
  saying so.
- **Renaming is not re-creating.** `hive_id` and `created_ms` survive a rename,
  so peers who know a hive by its id keep recognising it.
- **Names are validated to the capability-scope grammar** (`[A-Za-z0-9._-]`)
  now, not when #435 needs it — otherwise a hive called `barry's hive` could be
  named and then never granted `hive.read:` against.

### Decided — RFC Q5: `join_policy: open` is allowed, but warranted
- `open` keeps its plain meaning rather than being quietly redefined as `ask`: a
  setting that does not do what it says is worse than one that asks. Choosing it
  prints what it exposes, points at `ask` as the alternative that keeps
  discoverability, and requires confirmation — a `y/N` prompt interactively, and
  `--yes` where there is no terminal. In a pipe the flag is **required**, never
  assumed.
- **The consent is recorded, which is what makes it more than a prompt.**
  `identity.json` is an ordinary editable file, so a hand-written `"open"` has
  had no confirmation: `effective_join_policy()` returns `invite` for an
  unacknowledged `open`, and `hive identity` says plainly that the stored policy
  is not in force. Fail closed — a policy nobody consented to is not the
  permissive one. Leaving `open` drops the acknowledgement, so returning to it
  asks again.

### Changed
- `fmt_ms_at` (relative timestamps like `5m ago`) moved into
  `claudectl-core::helpers`. Two feature-gated callers need it now — `access`
  behind `relay`, `hive` not — and the sync-only
  `--no-default-features --features hive` build has a hive but no grants. The
  hive name grammar is spelled out in `hive::identity` for the same reason, with
  a `relay`-gated test asserting it agrees with `access::scope` where both exist.

### Added — the relay survives logout: launchd agent (#438)
- **`claudectl relay install-agent`**, plus `uninstall-agent` and
  `agent-status`. `relay serve` was a foreground "Press Ctrl+C to stop" process,
  so closing a terminal stopped the relay and every peer's cluster view went
  stale — `relay fleet` quietly fell back to local sessions. The agent starts at
  login (`RunAtLoad`) and is restarted if it exits (`KeepAlive`).
- **Managed from the CLI, not from an app bundle.** This is phase 3 of the macOS
  app epic (#425), but nothing about it is app-specific, so terminal users get a
  durable relay from the same change. `docs/relay.md` previously told people to
  "run it under `launchd`, `tmux`, or whatever you already use" — that advice is
  now a command.
- **Uninstall cannot leave an orphan.** The plist is removed even when unloading
  complains, because an orphaned plist that keeps resurrecting a service you
  thought you removed is how this feature usually goes wrong. `agent-status`
  detects the reverse too (loaded in launchd, plist missing) and says how to
  clear it.
- **`claudectl doctor` reports it**, with "not installed" as `Skipped` rather
  than a failure — running a relay in a terminal is a legitimate choice, and
  nagging about an optional daemon is noise. "Installed but not loaded" *is* a
  `Fail`, because that is the state that silently produces a stale cluster view.
- **The binary path is recorded un-canonicalized**, so a Homebrew install keeps
  pointing at the stable `/opt/homebrew/bin/claudectl` symlink rather than a
  versioned Cellar path the next `brew upgrade` would invalidate.
- Not macOS prints the `systemd --user` recipe instead of failing silently.
  `--auth-token` is stored in the plist (readable by your user) and the install
  output says so.


### Added — acting on an escalation: verdict, caller poll, expiry (#446)
- **The queue is drainable.** #430 queued a middle-band question and said
  plainly that acting on one was out of scope. `claudectl access escalations
  approve <id>` / `deny <id>` now record a verdict with an optional note, the
  listing grew a STATE column and a count of what is awaiting you, and the
  caller polls `GET /api/v1/project/<project>/escalation/<id>` with the token
  they already hold.
- **State lives beside the append-only queue, not in it.** A verdict is one
  `create_new` file under `escalation-verdicts/`, which keeps `escalations.jsonl`
  append-only and makes "decided twice" an `EEXIST` from the filesystem rather
  than a lost write — across processes and with no lock, since `approve` runs in
  the owner's shell while `query serve` answers the poll. Same reasoning as the
  grant revocation tombstone #431 introduced.
- **Approving marks the question answerable; it does not freeze an answer.**
  The poll retrieves deterministically from the live index (the #429 path, never
  the classifier) and reports both the fingerprint the question was classified
  against and the index's fingerprint now, so a caller can see the project moved
  underneath them. It also means `approve` needs no index, which is what lets an
  owner decide from any directory rather than only from inside the repo.
- **Expiry is derived, never swept.** A pending row stops being answerable after
  seven days, computed from its timestamp, so there is no sweeper process and a
  caller's poll never mutates the owner's queue. A recorded verdict always
  outranks the clock: an owner who decided on the last day decided it.
- **Polling does not spend the caller's daily budget** — it is the tail of a
  question already charged, and charging again would let a caller exhaust their
  own day waiting for an answer the owner had not yet given. The rate limit still
  applies, so they cannot spin on it for free.
- **Owner notification is a hook, not a new subsystem.** `[hooks.on_escalation]`
  fires with `CLAUDECTL_ESCALATION_ID`, `_GRANT`, `_PROJECT` and a truncated
  `_QUESTION`. It is the first hook with no session behind it, so it passes
  context as environment variables and expands no `{placeholder}` — and it is
  spawned without being waited on, so a slow notifier cannot hold the caller's
  response open.
- Every refusal on the poll path stays the one opaque `404`: unknown id,
  malformed id, and another grant's escalation are indistinguishable, so the
  48-bit id space is not an oracle for enumerating other holders' questions.
- Rows queued before this change still parse. `fingerprint` is
  `#[serde(default)]`, because without it every pre-existing line would fail to
  parse and the queue would silently empty — losing exactly the questions this
  closes the loop on.


## [0.65.0] - 2026-10-06

### Added — Jev query classification, confidence-gated routing, escalation queue (#430)
- **Classification gates the query surface, and it is off unless you turn it
  on.** Every `ask` is classified before retrieval by a single
  `POST https://api.typesafe.ai/v1/systemone` carrying five independent
  questions — intent, answerable-from-docs, seeks-sensitive, injection-attempt,
  scope-match — which Jev evaluates in parallel. `seeks_sensitive > 0.15` or
  `injection_attempt > 0.15` deny; `scope_match < 0.5` or a confident
  `out_of_scope` decline with a pointer; `answerable_from_docs > 0.7` with a
  confident intent answers; anything else escalates to the owner.
- **Jev is the router; code is the boundary.** Classification runs *after* the
  grant's scopes, rate limit and daily budget are checked, so it never sees a
  query the caller was not already entitled to ask and never decides whether a
  caller is authorized. Answers still come only from the pre-built index, so no
  classification outcome and no prompt injection can widen what is readable.
  Remove the classifier and the surface is blunter, not less safe.
- **Only the question and one paragraph of `CLAUDE.md` leave the machine.** The
  first exception to "brain decisions are local-only", and a deliberate one:
  opt-in via `TYPESAFE_API_KEY`, hard off via `[query] jev_enabled = false`,
  and both `query serve` and `query stdio` print the reminder to tell your grant
  holders. A test over a real index captures the request body and asserts it
  carries the question and the summary and nothing else — not `CLAUDE.md`
  beyond that one paragraph, not `docs/`, not the module map, not the skill
  list.
- **The credential never reaches `argv`.** `brain/client.rs` passes everything
  as `curl` arguments because a local Ollama endpoint has no auth; an
  `Authorization: Bearer` there is readable by any local user through `ps`. The
  header goes on stdin via `-H @-` instead, and `curl_args` does not take the
  key as a parameter so that is a property of its signature. The endpoint is a
  constant, not a config field.
- **Two ways to have no classifier, and they behave differently.** No key (or
  `jev_enabled = false`) is exactly the surface #429 shipped — unchanged, not a
  degraded mode. A *configured but unavailable* classifier answers strictly: at
  most 3 spans, and only spans scoring 2 or better. An owner who enabled a gate
  should never get a more permissive surface when the gate is down; an owner who
  never enabled one should not get a quiet regression.
- `jev.unreachable`, `jev.unauthorized`, `jev.rate_limited`, `jev.malformed`
  and `jev.spend_ceiling` are audited separately, because "you forgot the key"
  and "the API is down" call for different responses. A missing `usage` block
  is treated as malformed rather than as a free call — a schema change must not
  uncap the spend the ceiling exists to bound.
- **An escalation queue.** `~/.claudectl/access/escalations.jsonl`,
  append-only and 0600, read by `claudectl access escalations`. HTTP answers
  `202 Accepted` with `{"status":"pending_review","escalation_id":"esc_…"}`. The
  record keeps the **full** question where `audit.jsonl` truncates at 512 bytes,
  because an escalation is read one at a time by a person deciding on it.
- **The monthly spend ceiling**, completing #431's deferred §4.8 row.
  `jev-spend.json`, keyed by UTC month, default `$5.00` via
  `[query] jev_monthly_usd`. Checked before the call because a request cannot be
  un-sent, charged after it from `usage.input_tokens` because that is the only
  authoritative count. On breach the surface degrades instead of billing on.
- **An injection attempt flags the grant without revoking it.** `flagged_ms` and
  `flag_reason` are unsigned fields on the grant, so flagging changes what the
  owner is told rather than what the grant can do; `access list` shows
  `flagged`, which outranks `active` precisely because a flagged grant still
  works. First flag wins, so a flood cannot overwrite the evidence of the one
  that started it. The threshold is admittedly paranoid, and a false positive
  that killed a real holder's grant would be worse than one denied query.
- **Fixed a lost-write window #431 left open.** `record_query_use` is load →
  bump `use_count` → write, which was never under the lock `charge_daily_budget`
  takes. On its own that could lose a budget charge between two concurrent
  requests; with #430's `flag_grant` writing the same file it could also
  *erase a flag* — "B loads, A flags and writes, B writes its stale copy" —
  which would quietly undo the one thing `access list` is meant to surface. All
  three recording paths now take `budget_lock`.
- A new `classification` field on audit lines — one greppable line with all
  five numbers. Absent when nothing was classified, so an owner who never opted
  in sees exactly the log shape #431 shipped. `access audit` prints it under its
  row rather than as a column, and `event` gained a third value, `escalated`.
- `[query]` config section: `jev_enabled`, `jev_model`, `jev_monthly_usd`. TOML
  only, no CLI flags — these are properties of a long-running server. The API
  key is not among them; a secret in `.claudectl.toml` is a secret in the repo.
- **Fixed in review:** concurrent appends to `escalations.jsonl` and
  `audit.jsonl` could merge two records onto one line and lose **both** —
  `writeln!` on an unbuffered file is two `write(2)` calls, and the surface is
  thread-per-connection. Measured: 167 of 1000 escalations lost under four
  concurrent writers. Now one write including the newline, which `O_APPEND`
  places atomically.
- **Fixed in review:** an escalation the queue could not accept charged the
  budget and left no audit line anywhere — the same `access list` /
  `access audit` disagreement the `get_doc` miss had. It now audits
  `queue_unwritable` with the classification attached.
- **Fixed in review:** a spend-ledger write failure is now marked on the audit
  line of *every* route rather than only an answer, and no longer discards the
  five probabilities. An unwritable ledger reads as zero spend, so the monthly
  ceiling stops biting — and an adversarial holder produces denies and
  declines, exactly where an answer-only marker would never appear.
- **Fixed in review:** `query stdio` now makes §4.6's third-party disclosure on
  stderr. Only `serve` printed it, so MCP mode with a key already in the
  environment was a silent opt-in.
- **Not shipped, and said rather than implied:** Jev reranking of candidate
  spans (§4.5 step 2, which the RFC itself calls separable); approve/deny and
  the resume path for an escalation (#446); and any verification against the live API.
  No request has been sent to `api.typesafe.ai` from this codebase. The client
  follows the published contract, the routing table is fixture-tested against
  24 adversarial cases, both degrade paths are tested, and the real `curl`
  invocation is tested against a local listener — but whether Jev accepts this
  request body is unverified. Set `TYPESAFE_API_KEY` to opt in, and file an
  issue with the first real response if the schema has drifted.

### Fixed — review follow-ups on the query guardrails (#431)

Eight findings from a review of #444. All eight were real.

- **A revocation could be silently undone by a concurrent write.** The budget
  charge, the use-count bump and `revoke` all rewrite the *whole* grant record,
  and `access revoke` runs in the CLI while the other two run in
  `query serve` — a different process, which a process-local mutex cannot
  order. So `serve` could load a grant, the owner could revoke it, and `serve`
  could write its stale copy back: a token the owner believed was dead kept
  answering. Revocation is now backed by a create-only marker file at
  `grants/<grant_id>.revoked`, which a writer that never writes it cannot
  clobber; `load` and `list` both fold it in, so `access list` and `verify` can
  never disagree. **Revocation is one-way now** — setting `revoked: false` back
  in the JSON no longer restores the grant. Nothing documented un-revoke as a
  capability, and #431's "a cross-process clobber is the benign direction" was
  true of the counters and false of this.
- **The budget mutex didn't close the window it was for.** `record_query_use`
  is a second read-modify-write on the grant file and never took the lock, so
  an unsynchronised read followed by a locked charge followed by a stale write
  still lost the charge. All three recording paths take it now.
- **A `get_doc` miss charged the budget and audited nothing.** A holder
  enumerating doc paths drained `budget_used` while `access audit` stayed
  empty, so the two commands disagreed and neither could be reconciled with the
  other — exactly the probing #431 argued the QUESTION column exists to
  surface. The miss now writes a `not_indexed` line carrying the path. The
  caller still gets the same opaque `404`.
- **A question could forge an audit row or drive the operator's terminal.**
  `question` is third-party text printed straight to stdout; a newline inside
  the first 44 characters survived the truncation and forged a plausible extra
  row, and an ANSI escape could clear the screen. Control characters are now
  replaced with a visible marker **at render** — never on the way into
  `audit.jsonl`, because scrubbing on write would destroy the evidence the log
  exists to keep. `--json` was already safe.
- **The `missing_scope` denial bypassed the rate limit entirely.** A holder
  with a wrong-scope token was unthrottled, each request costing an HMAC, a
  grant read, a grant write and two audit appends at wire speed. It has
  presented a valid MAC, which is the only thing the bucket-map argument
  requires, so it is throttled on its own grant's limit. The refusal stays the
  opaque `404` — a `429` there would confirm this project recognises the token.
  The throttle bounds the grant-file writes, not the appends: `verify_detailed`
  has already written its line by then.
- **A backwards clock step handed out a second daily allowance.** The rollover
  compared `!=`, so an NTP correction back across a day boundary read as a new
  day and reset the counter. It compares `>` now and only ever rolls forward.
- **`daily_query_budget: 0` was a dead grant.** `rate_limit_per_min: 0` is
  deliberately read as `1`; the budget read `0` as deny-everything. These
  fields have no CLI flag and `docs/access.md` tells owners to edit the file, so
  both now read `0` as `1` — one convention is worth more than either reading
  alone. Read-time only; the file keeps what was written.
- **A `429` could ship without the `Retry-After` the docs promise.** The
  acquire and the hint read two different `Instant`s, so a refill between them
  returned `None`. One `Instant`, used for both.

### Added — query guardrails: rate limit, daily budget, audited questions (#431)
- **The per-grant limits #427 persisted are now enforced.** `rate_limit_per_min`
  is a token bucket, `daily_query_budget` a UTC-day counter on the grant file.
  Both answer `429` with a `Retry-After` and distinguishable bodies — the
  caller has already proved they hold a valid in-scope token, so naming their
  own limit reveals nothing §3.3 protects, and a working grant that goes
  silently quiet is hostile.
- **Only a `MissingScope` denial charges the budget.** §4.8 says "denied
  queries count, so probing is self-limiting", and that is the one denial
  proving the caller holds a *valid* token and is probing other verbs. Read
  literally — charging every denial — anyone who guessed a 24-bit grant id
  could drain the real holder's allowance, turning a defence against probing
  into a denial-of-service against the person it protects.
- **A throttled request never reaches the budget**, because it was never
  evaluated; and the rate limit is checked *after* verification, so the bucket
  map is only ever keyed on an id that has presented a valid MAC. Keyed on
  claimed ids instead, walking 24-bit ids could pin 16M buckets.
- **`audit.jsonl` now records what was asked and what came back** — `question`
  (truncated to 512 bytes on a char boundary) and `cited` (the distinct paths
  that answered, sorted). This is the issue's load-bearing claim made real:
  caps stop the abuse you anticipated, and the log is the only way the abuse
  nobody anticipated becomes visible. `access audit` grows a QUESTION column;
  `cited` stays in `--json`.
- **`src/bus/rate_limit.rs` moved to `src/rate_limit.rs`** (gated on
  `any(bus, relay)`), because the query surface is `relay`-gated and the
  alternative was forcing rmcp, Tokio and SQLite onto an HTTP surface with no
  use for them. `try_acquire_with_capacity` is new, since a grant brings its
  own limit where a bus role uses one global default.

### Fixed — a budget charge that failed open (#431)
- **A transient store failure returned `500` with the charge never applied**,
  so anyone able to induce one got unmetered requests. `charge_daily_budget`
  distinguished "budget exhausted" from "could not read the grant" by
  comparing error *strings*; it now returns a typed `ChargeError`, and a store
  failure is the same opaque denial as everything else — a surface that cannot
  meter does not answer. The failure is audited so the operator sees it.

### Added — read-only query surface, deterministic (#429)
- **`claudectl query serve` and `claudectl query stdio`.** Phase 3 of the
  open-cluster RFC (#423): grant holders ask natural-language questions about
  one project and get **verbatim spans with citations** back — over HTTP for a
  human or a script, over MCP for their Claude. There is no generation step, so
  selection cannot invent a fact the index does not contain. Deterministic term
  matching stands where Jev will go (#430), which is deliberate: a surface that
  works is what a classifier can then be measured against.
- **One server, one project.** The RFC routes on
  `/api/v1/project/<project>/…`, which reads as though a name resolves to a
  directory. Nothing in claudectl can do that — `session.project_name` is a cwd
  basename, so the mapping is many-to-one, and `~/.claude/projects/<slug>` is a
  lossy `/`→`-` substitution that cannot be inverted. A process serves the
  repository it was started in; the `<project>` segment is *compared* against
  that one name and never resolved, joined to a path, or passed to `verify`.
  `--project` names it when the directory name cannot be a scope qualifier,
  which is every worktree with a `+` in its name.
- **The required scope comes from the served project, not the request.** Built
  from the request's `<project>` segment instead, a token minted for
  `project.query:other` presented at `/project/other/query` would satisfy
  `verify` on a server serving something else. `project.query` covers `ask` and
  `topics`; `project.docs` covers `get_doc`, the verb that returns verbatim
  bodies.
- **Every refusal is the same opaque `404`.** Wrong project, missing scope,
  unknown route and bad MAC are one answer, byte for byte, with the real
  `DenyReason` going to `audit.jsonl` and nowhere else (RFC §3.3). A missing
  bearer header is the one `401`, decided before anything project-specific so
  it says nothing about what exists. No route mutates the project; the only
  write anywhere is the grant's own `use_count` and audit line.
- **`get_doc` is a lookup, not a read.** A caller's path is matched against
  paths already in the index — never joined, canonicalized or opened. It is the
  one input that looks like a path, and treating it as a key is what keeps the
  resemblance harmless.

### Fixed — citations, found by serving this repo (#429)
- **A fenced code block's `#` comment was published as a heading.** `#` starts
  a comment in TOML, shell, Python and YAML, so `docs/configuration.md` was
  publishing a section titled ``# `escalation_model`. Unset = no routing.`` and
  splitting a code block in half to do it. #428 skipped fences as "harmless for
  a publishable-text decision"; sections are also the citation unit, which is
  what made it matter. `docs::sections` now tracks fences, including `~~~` and
  longer-backtick nesting.
- **Ancestor headings counted at full weight.** `docs/AGENT_BUS.md` is titled
  "claudectl Agent Bus — Design Specification", so all of its subsections
  inherited both words of "agent bus" and tied — handing the top of the results
  to whichever was shortest. An ancestor now scores 1 against its own heading's
  3.
- **A module item cited only its `impl` block**, so three different functions
  in `impl Config` came back as three identical citations. The signature is now
  part of the heading path.

### Changed
- **`src/context` is gated on `relay`**, matching its only consumer. It was
  ungated in #428 because nothing consumed it; left that way, the minimal
  `--no-default-features --features hive` build carried the whole module as
  dead code.
- **`src/access` and `src/context` no longer carry `#![allow(dead_code)]`** —
  the lid came off as part of wiring their first consumer, which is the real
  check on whether the surface is complete. `Scope::is_issuable` turned out to
  be a second, uncalled copy of `unissuable_reason`'s list and was removed.

### Added — project context index: the query surface's boundary (#428)
- **Tracked symlinks are refused, not followed.** A link is the one tracked
  path whose contents live where no rule in the module reaches: a tracked
  `docs/notes.md -> ../../.env` classifies as documentation, and both
  `Path::is_file()` and `fs::read` follow it. `symlink_metadata` now gates
  every read. Declined rather than resolved — deciding whether a target is
  "inside" the repo is a canonicalization problem with `..`, mount points and
  TOCTOU in it.
- **Per-category exposure fails closed on a corrupt file.** A missing file
  means nothing was decided and the mode decides; a file that will not parse
  hides everything. `auto` is the default mode, so discarding the map would
  have republished whatever the operator had hidden, and hand-editing is
  currently the only way to write it.
- **The skills channel's `.claude/skills/` carve-out is now explicit.**
  `deny.rs` excludes `.claude` at any depth and project skills live inside it,
  so the code and the RFC table disagreed. The channel requires a tracked path
  whose first two segments are exactly `.claude/skills/`;
  `.claude/settings.json` and `.claude/agents/` stay unreachable by every
  channel.
- **`const` and `static` initializers are cut at the `=`.** A one-line
  `pub const TOKEN: &str = "sk-live-…"` is a body by another name, which is
  what the no-bodies rule exists to prevent. The name and type survive.
- **The module-map extractor works on logical statements, not physical lines.**
  rustfmt wraps signatures at 100 columns and puts a generic `impl`'s `where`
  clause on its own line; reading either line-at-a-time lost the whole block —
  every method of a `where`-bounded impl, in that case. Block comments are also
  stripped with state carried across lines, since one unbalanced `{` inside
  `/* … */` desynced the brace counter and silently dropped a file's entire
  public surface.
- `ContextIndex::root` is no longer serialized: it is an absolute local path,
  and this struct is "everything a query may be answered from". Leaving it out
  also makes `fingerprint()` depend on content alone, so the same tree checked
  out at two paths agrees.
- Building an index no longer mints a peer identity. `local_identity` reads
  `~/.claudectl/relay/identity` rather than calling
  `relay::load_or_create_identity`, which *writes* when absent; with no
  identity, no hive units publish, since none could be attributed.
- A nonexistent root reports `NotARepo` rather than `GitUnavailable` —
  `Command::output` fails identically for both, and "git is not available"
  sends the operator somewhere useless.
- New ungated **`src/context/`** — the substrate a read-only project query may
  be answered from, phase 2 of the open-cluster RFC (#423).
  `ContextIndex::build(&Path)` / `build_with(root, &IndexExposure, ShareMode)`
  build it out of CLAUDE.md, README.md, `docs/**` markdown, a Rust module map,
  project skills and exposed hive units. There is deliberately **no CLI and no caller**:
  retrieval and the query surface are #429, caps and budgets #431.
- It takes a **path, not a project name**. Project names are many-to-one onto
  directories — every worktree of a repo shares a basename — so resolving a
  grant's `project.query:<name>` to a directory is #429's problem.
- **`git ls-files -z --cached` is the only source of paths.** No `read_dir` and
  no directory walk anywhere in the module, which is what makes "published
  implies tracked and not excluded" true rather than aspirational. `--cached`
  is the git index, so a staged file counts and a modified file's untracked
  sibling does not.
- **No git is an error, never a fallback** — `IndexError::{GitUnavailable,
  NotARepo, GitFailed}`, an enum so #429 can match on it. `coord::resume`
  degrades to an mtime hash when git is missing because a stale tree hash is
  tolerable; here a fallback would mean indexing whatever is on disk, `.env`
  included.
- **`deny.rs` is the "excluded" half**, because tracked is not the same as
  publishable. Deny-first on the relative path before any read: directory
  segments at any depth (`.claude`, `.claudectl`, `.git`, `.ssh`, `target`,
  `node_modules`, …), whole names and prefixes (`.env*`, `.netrc`, the `id_*`
  ssh keys, `credentials`, `secrets.yml`, `secrets.yaml`, `.npmrc`, `.pypirc`),
  and extensions (`jsonl`, keys, certs, `sqlite`/`db`). Denying `jsonl` covers
  two of the three never-published content classes in one rule — session
  transcripts and the brain decision log are both JSONL. `.claude`/`.claudectl` as *segments* stop a repo that commits its own
  agent state from publishing it.
- **The module map emits `//!` headers, `///` docs and public signatures, never
  bodies.** Two separate depth counters do it: `skip_depth` for item bodies
  (emit nothing) and `container_depth` for `impl` / inline-`mod` blocks (descend,
  their contents are more items). Nothing inside a brace is emitted, so struct
  fields and enum variants are out too. `pub(crate)` is not public API, and
  private items are dropped entirely, name included.
- **Per-category exposure** over six categories — `claude_md`, `readme`, `docs`,
  `module_map`, `skills`, `hive_units` — at `~/.claudectl/access/index-exposure.json`.
  Mirrors `hive::exposure::ExposureStore` semantics exactly (explicit entry wins;
  missing entry follows the mode; auto exposes, manual hides) with the types
  redefined locally and identical `"expose"`/`"hide"` wire values, because
  `src/context/` is ungated and hive is not. The mode is the existing
  `Config.hive.share_mode` — no new config field. Manual mode publishes nothing
  until a category is opted in.
- **Two holes the tests found and closed.** `skills::discover` reads the
  filesystem directly, sweeping `~/.claude/skills`, every installed plugin and
  the project, so publishing its output as-is drove a hole through the
  tracked-only rule: an *untracked* project skill would publish, and so would
  the operator's personal global skills. Intersecting with the tracked set
  closes both, since a skill outside the repo is not in that set. Separately,
  hive units were not filtered by origin, so a unit that arrived by gossip could
  be republished to a third party who was never part of that exchange — now
  locally-originated only (`source_peer` equals this machine's identity), with
  categories matched on the `KnowledgeCategory` enum rather than a string, which
  sidesteps `WorkflowPattern` serializing as `workflow_pattern` while its
  `label()` returns `workflow`.
- **Deterministic by construction**: tracked list sorted and deduped, units and
  skills sorted, so two builds of the same tree are byte-identical.
  `fingerprint()` is FNV-1a (`"fnv1a:<hex>"`) rather than SHA-256 because
  `relay::crypto` is feature-gated and this module is not — same reasoning as
  `coord::resume`'s tree hash, and it is a cache key, not an auth primitive.
- Acceptance test is one fixture project that *contains* every forbidden thing
  — committed `.env`, committed `decisions.jsonl` and `transcript.jsonl`, a
  tracked `.claudectl/` file, an untracked file, a gitignored file and a
  function body — asserting through a single serialized-blob check that no
  sentinel appears, and that the things that should be there are. Plus a
  self-index test against claudectl itself, the one fixture that cannot drift
  from reality. That run reports **0 denied**, which is correct: this repo's
  `.env` is untracked, so `ls-files` never returns it — meaning the tracked gate
  and the denylist are each covered by a different test.
- First `git init` in the repo's test suite. Fixtures pass `-c user.name`/
  `user.email`/`commit.gpgsign=false` so they do not depend on CI's global git
  config, and skip rather than fail when git is absent.

### Added — capability grants, scopes and `claudectl access` (#427)
- **New `src/access/` module**: scoped, expiring, revocable read-only access
  for a third party — phase 1 of the open-cluster RFC (#423). Everything else
  in claudectl assumes one trust level (you, on your machines): a relay PSK is
  symmetric, and the coordinator's bearer token has no identity, no scope and
  no per-grant revocation.
- **`claudectl access grant --project <p> --label <l> [--scopes …] [--expires …]`**
  issues a grant and prints its token once. `--scopes` defaults to
  `project.query`, `--expires` to `30d`. Plus **`access list`** (state, use
  count, last use), **`access audit <grant_id>`** (what a grant actually asked
  for) and **`access revoke <grant_id>`**. All four honour `--json`, which is a
  global flag — `claudectl --json access list`.
- **Token format `cctl_<grant_id>_<mac>`** — e.g.
  `cctl_gr_cacccf_54814e5e550529c7cf02f3805f61d8eb`. The MAC is HMAC-SHA256
  over a canonical payload of `grant_id` + sorted scopes + `expires_ms`,
  truncated to 128 bits and hex-encoded, via the inline primitives already in
  `relay::crypto` — no JWT library, no asymmetric crypto, no new dependency.
- **The signature covers `scopes` and `expires_ms`; it does not cover
  `revoked`, `last_used_ms` or `use_count`.** Revoking is a one-field write
  that takes effect immediately with nothing restarted, while any edit to a
  grant's scopes or expiry invalidates the issued token and forces a re-grant.
- **Every denial is opaque to the caller.** One `AccessError::Denied` with no
  detail: unknown grant, bad MAC, revoked, expired and missing scope are
  indistinguishable from outside. The specific reason goes to
  `~/.claudectl/access/audit.jsonl` only — the RFC's 404-not-403 rule applied
  one layer down, since telling "no such grant" from "revoked" would leak which
  grant ids exist. Denied attempts are audited too, so probing is visible.
- **Scopes:** all five RFC verbs parse, so a grant file written by a later
  version still loads, but only `project.query` and `project.docs` can be
  issued today. `fleet.read` is refused citing open question Q8 ("defined,
  issued to nobody"); `hive.read` and `hive.join` are refused pending #424.
  There is no write verb in the grammar.
- **The HMAC key at `~/.claudectl/access/secret` fails closed.** It uses a new
  `crypto::try_generate_psk`, which errors rather than falling back to
  `generate_psk`'s timestamp/pid/thread-id hash when `/dev/urandom` cannot be
  read — fine for a LAN pairing code, not for
  the root key behind third-party auth. A corrupt or wrong-length secret is an
  error, never a silent re-mint, because re-minting would invalidate every live
  grant without saying so.
- **`secret`, `grants/*.json` and `audit.jsonl` are all `0600`**, with the mode
  set in the `open(2)` call rather than chmodded afterwards — a chmod cannot
  revoke a descriptor another process opened during the window at
  `0666 & ~umask` (`save_peer_psk` still has that window). The audit log's mode
  is re-asserted on every append, so a log left wider by an earlier version is
  repaired instead of staying that way. Grant files are owner-only because each
  carries every field the MAC covers — at a default umask, `secret` would be the
  only barrier between an unprivileged local user and every token on the machine.
- **`HOME` must be set.** Every other store in the codebase falls back to
  `/tmp` when it is not; this one refuses. `/tmp` is world-writable and the
  secret is read back with `read_to_string`, which follows symlinks — someone
  who pre-places `/tmp/.claudectl/access/secret` would be supplying the key
  every grant MAC derives from.
- `claudectl access audit` with no grant id prints the whole log. Denials
  against a token too malformed to name a grant are filed under a sentinel id
  that is not a valid grant id, so the no-id form is the only way to see the
  probing the log exists to surface.
- A qualifier written into `--scopes` must agree with `--project` rather than
  silently overriding it: `--project internal-api --scopes project.query:secrets`
  is now an error, where before it issued a grant scoped to `secrets`.
- **No network surface in this phase.** `access grant` opens no port. #429 adds
  the read-only query surface; #431 adds enforcement of the
  `rate_limit_per_min` (20) and `daily_query_budget` (500) fields the grant
  file already carries.
- Gated behind the `relay` feature, since the MAC comes from `relay::crypto`.
  The minimal `--no-default-features --features hive` build has no access
  surface. Written up in the new `docs/access.md`; `docs/open-cluster.md` §3.2,
  §3.3, §5 and §10 are updated to what shipped — including its verify order,
  which as specified put the MAC check before the grant-file load and could not
  work, because the MAC covers fields that live only in the file.

### Changed — coordinator HTTP API binds loopback by default (#426)
- **`RelayConfig` gains `http_addr`, defaulting to `127.0.0.1`**, plus a
  `claudectl relay serve --http-addr <ADDR>` flag. Resolution order is
  `--http-addr` > `http_addr` in config > `127.0.0.1`.
- **This breaks reaching the API from another machine.** The HTTP listener used
  to inherit `relay.listen_addr`, which defaults to `0.0.0.0`, so
  `claudectl relay serve --http-port 9876 --auth-token secret` put the
  plaintext API on every interface. It now binds loopback. Pass
  **`--http-addr 0.0.0.0`** (or set `http_addr` in `[relay]`) to opt back in.
- `listen_addr` no longer governs the HTTP API. It still governs the PSK peer
  transport and still defaults to `0.0.0.0`, which is that listener's intended
  deployment — it is HMAC-authenticated and rate-limited per IP.
- Phase 0 of the open-cluster RFC (#423) settles its open question Q3:
  loopback by default, and an operator-provided tunnel (Cloudflare Tunnel,
  Tailscale Funnel, `ssh -R`) for off-machine access. No `rustls`, no new
  dependency — the sync core runs on 7 runtime crates and `Cargo.toml` has no
  TLS crate, and a tunnel covers the same boundary. The API remains plaintext
  HTTP/1.1. Written up in `docs/relay.md` §Security, which previously claimed
  the peer transport's HMAC auth and rate limiting for the HTTP API as well.

### Added — constant-time compare + non-loopback startup warnings (#426)
- New **`relay::crypto::ct_eq(&[u8], &[u8]) -> bool`**: folds over every byte so
  neither the time taken nor the result reveals where the first mismatch was. A
  length mismatch returns `false` early; length is not treated as secret.
- It replaced the two places that compared a secret with `!=`: the coordinator
  API bearer token (`src/relay/http.rs`) and the HMAC-SHA256 handshake proof
  (`src/relay/protocol.rs`).
- **Startup warning on a non-loopback HTTP bind** — `0.0.0.0`, `::`, or a
  specific LAN IP — for the relay coordinator API and for
  `claudectl supervisor metrics`, whose `/metrics` endpoint has no
  authentication at all. The metrics warning is an adjacent fix, not part of
  #426; its bind still defaults to `127.0.0.1:9464`, so the warning fires only
  when an operator asks for a wider one.

### Added — cluster session view: every machine's sessions in one place
- **`claudectl relay fleet`** lists every session running across paired
  machines — local and remote — with project, status and cost per machine.
- Remote sessions now appear in the dashboard and `claudectl -l` as
  `[worker-id] project`, folded into the total cost. The plumbing for this
  existed on both ends but was never connected: heartbeats were sent with
  `None` for sessions, so `App.remote_sessions` stayed empty outside demo mode
  and `GET /api/sessions` returned only peers' sessions, never the
  coordinator's own.
- New `claudectl-core::fleet`: the snapshot `relay serve` publishes to
  `~/.claudectl/relay/fleet.json`, and a side-effect-free local session
  collector (no hooks, notifications or history writes, since the relay runs
  alongside the TUI). New `relay::advertise` owns the write side.
- A relay must be running on each machine for the cluster view; `relay fleet`
  says so and falls back to local sessions when no snapshot is found. Peers
  that stop reporting drop out after 90s; a snapshot older than 120s is
  ignored rather than shown frozen.

### Fixed
- **`claudectl_core::history::parse_duration` no longer panics on an absurd
  duration.** It multiplied the numeric part by its unit without a guard, so
  `--expires 99999999999999999999w` aborted in a debug build instead of being
  read as invalid input. It now uses `checked_mul` and returns `None` on
  overflow, like any other unparseable string. Reached through
  `claudectl access grant --expires`, and shared with `--since` (the window for
  `--summary`, `--history` and `--stats`).
- `claudectl -l` no longer misaligns every column after a project name longer
  than the column width; the PROJECT column widened to fit `[worker-id] project`,
  and cells clamp via the new `helpers::truncate_cell`.

### Notes
- `LocalSessionCollector` is stateful on purpose: it keeps each session's JSONL
  offset and token totals across collections, so the relay re-reads only new
  bytes. Measured on 4 live sessions (one 54M-token, 10-hour transcript): 188ms
  for the first collection, 40ms for each one after, vs 158ms every time for a
  stateless collector. Honours the "never rereads full files" design rule.
- `claudectl-core` gained a public `fleet` module, `helpers::truncate_cell` and
  `helpers::is_exposed_bind`, and `history::parse_duration` changed behaviour
  (overflow is now `None` rather than a debug panic), so it needs a version
  bump (and a matching path-dep `version` in the binary's `Cargo.toml`) at
  release time.
- `is_exposed_bind` lives in core rather than in `relay` because both callers
  need it and they sit behind different features — `relay` for the coordinator
  API, `coord` for the metrics exporter.

Workspace crates bumped: `claudectl-core` → 0.59.0 (new `fleet` module, `helpers` additions), `claudectl-tui` → 0.60.0 (`app` split into a module tree with new public surfaces).

## [0.64.0] - 2026-07-04

### Added — guided-tour orientation tips in demo mode (#373)
- `claudectl --demo` now shows a rotating **orientation tip** in the dashboard title — a short tour of the key surfaces (status colors, the brain, health flags, `T`/`K`/`M` panels, per-session cost) so a first-time viewer understands what they're looking at, alongside the existing scripted activity narration in the status bar. Complements the modal `claudectl demo` guided tour shipped in 0.63.0.

Workspace crate bumped: `claudectl-tui` → 0.59.0 (new public `demo_tour_tip`). `claudectl-core` unchanged at 0.58.0.

## [0.63.0] - 2026-07-01

### Added — brain "why" + decision audit log (#372, closes the issue)
- Every brain decision now carries an auditable **"why"**: a `DecisionCause` (source · rule · few-shot ids) rides on each suggestion, captured at decision time and persisted as three backwards-compatible `DecisionRecord` fields. `DecisionRecord::why()` renders a one-line cause (`via llm · 92% confidence · 2 past example(s)`), surfaced inline in `--brain-query` JSON and the Brain Review detail panel (via the `DecisionSummary` DTO).
- **One-key correct-and-learn** (`[c]` in `--brain-review`): pick the right answer and it's recorded as a canonical example, so the next similar decision improves.
- New **`--brain-export [md|json]`** with `--project`/`--pid` filters — a readable decision timeline to paste into a PR or hand to a teammate. Backed by the new `brain::audit` module.

### Added — `claudectl demo` guided tour (#373, closes the issue)
- New **`claudectl demo`** subcommand launches the dashboard in demo mode with a 7-step narrated overlay over the existing fixtures: stall detection, budget/cost warnings, conflict alerts, the brain's "why", and verified tasks. `space`/`→` advance, `←` back, `Esc` drops into the live demo; the scene is pinned per step so the moment stays on screen. No live Claude sessions required.

### Added — PR auto-post on task DONE (#369, closes the issue)
- With `CLAUDECTL_PR_AUTO_POST=1` set in the supervisor daemon's environment, the reconciler now runs the `supervisor pr` flow automatically when a task reaches DONE — posting the summary comment + `claudectl/verifier` commit status to its branch's PR. Opt-in (off by default), and the post runs on a **detached thread** so git/gh latency never blocks the reconciler tick. The transition is committed before the post fires, so a failed post is logged, never propagated. This completes #369.

With #372/#373 this closes the **developer-effectiveness epic (#367)**. Workspace crates bumped: `claudectl-core` and `claudectl-tui` → 0.58.0 (new `DecisionSummary` "why" fields; public `DemoTour` + `ui::demo_tour`).

## [0.62.0] - 2026-06-29

### Added — sessions↔tasks linkage in the dashboard (#368, closes the issue)
- Sessions in the main dashboard table that are **supervisor task attempts** now carry a **`T`** badge (alongside the existing `L`/`H`/`I` coordination badges), so an operator can tell at a glance which live Claude sessions belong to a tracked task. With the panel write actions + verdict/cost columns already shipped, this completes #368.

### Added — supervisor panel approve action (#368)
- The Supervisor panel gains **`a`** to **approve** a `NEEDS_HUMAN` task — accept the work as-is and move it to DONE, an operator override of the verifier that escalated it. Routes through the new `Actions::approve_task`. With cancel (`c`), retry (`R`), and drain (`d`), the panel's operator action set is complete.

### Added — supervisor panel write actions (#368, increment 2b)
- The Supervisor panel (`T`) is now operational: **`c`** cancels the selected task (double-tap to confirm — moves it to CANCELLED), **`R`** re-queues a failed/cancelled task (back to PENDING so the reconciler re-assigns it), and **`d`** toggles the supervisor **drain** marker (reconciler stops issuing new assignments while running tasks finish). All route through new `Actions::cancel_task` / `Actions::retry_task` / `Actions::set_supervisor_drain` methods; the footer + help reflect the keys. One-key approve for NEEDS_HUMAN tasks is a later slice.

### Added — verifier-as-check (#369, increment 2)
- `claudectl supervisor pr <task_id>` now also sets a **`claudectl/verifier` commit status** on the branch's HEAD, reflecting the task's latest verifier verdict (PASS → success, FAIL → failure, none → pending). It rides the existing best-effort path — the status post can't undo the comment already landed, and a missing `gh`/repo just appends a skip note. Auto-posting on task DONE from the reconciler is the remaining slice of #369.

Workspace crates bumped: `claudectl-core` and `claudectl-tui` → 0.57.0 (new `Actions` task-control methods).

## [0.61.0] - 2026-06-29

### Fixed — headless relay delegate/interrupt actually send now (#378)
- `claudectl relay delegate <peer> "<prompt>"` and `relay interrupt` were no-ops outside the TUI: they built the message, printed a success line, and exited 0 without transmitting. They now open a one-shot authenticated connection to the peer (using the stored PSK + address, the same path `relay connect` uses) and send the frame — no running daemon required. On any failure (peer not paired, no address, connection refused) they print a clear error and **exit non-zero**, so scripts no longer mistake a built-but-unsent message for a delivered one.
- `relay interrupt` gains a required `--peer <id>` so the interrupt can be routed to the peer that owns the task.

### Added — supervisor panel: verdict + cost columns (#368, increment 2a)
- The Supervisor panel (`T`) now shows a **VERDICT** column (latest verifier result, green PASS / red FAIL) and a **COST** column (total $ across the task's attempts), replacing the low-value session-hash column. New `tasks::latest_verification` and `tasks::task_cost_usd` read APIs back this and are shared with PR-native (#369). One-key write actions (retry/approve/cancel/drain) remain the next slice of #368.

### Fixed — flaky relay HTTP server tests (#381)
- The relay coordinator's HTTP server tests raced the accept loop: a fixed 50ms pre-sleep then a single read could miss the response on a loaded CI runner, intermittently failing the release. The tests now retry connect+read until a response or a 5s deadline (deterministic), and the server's non-blocking accept poll shrank from 100ms to 10ms — bounding first-request latency and the race window.

Workspace crates bumped: `claudectl-core` and `claudectl-tui` → 0.56.0 (new `TaskSummary` verdict + cost fields).

## [0.60.0] - 2026-06-28

### Added — PR-native integration (#369, increment 1)
- **`claudectl supervisor pr <task_id>`** posts a task summary (state, attempts, role) as a comment on the PR for the task's branch, resolved via `git` + `gh`. Best-effort by construction: no open PR, no `gh`, or not a git repo prints a `skipped:` line and exits 0 — it never fails the task. Verifier-as-check and auto-posting on task DONE are increment 2.

### Added — brain model routing (#370, increment 2)
- The brain can now **escalate low-confidence decisions to a stronger model**. Set `escalation_model` (+ optional `escalation_threshold`, default 0.7) in the `[brain]` config: the cheap/primary `model` answers every gate decision, and only when it's uncertain is the prompt re-run on the stronger model. Off by default (no `escalation_model` ⇒ today's single-model behavior, byte-for-byte).
- A failed escalation falls back to the primary suggestion rather than erroring, so routing never makes the brain less available than before.

Workspace crates bumped: `claudectl-core` and `claudectl-tui` → 0.55.0 (new `BrainConfig` routing fields).

## [0.59.0] - 2026-06-28

### Added — budget-ETA cost forecasting (#370, increment 1)
- The dashboard detail panel now shows a **Budget ETA** when a per-session budget is set: smoothed time-to-cap with a p10/p90 band (e.g. `~1h20m  (range ~40m–~3h10m)`).
- Burn rate is now folded into an **EWMA** (~45s half-life) instead of being read off a single tick, so the forecast tracks real spend regime changes without flickering on bursty token usage. The ETA interval comes from the p10/p90 of recent samples, so auto-actions can later gate on the conservative bound.
- New pure, fully-tested `claudectl_core::forecast` module (EWMA, percentile, ETA band, formatting). Model routing — the other half of #370 — is a separate increment.

### Added — supervisor tasks.toml scaffolding + validation (#371)
- **`claudectl supervisor init`** scaffolds a documented starter `tasks.toml` in the cwd (every optional key present but commented). Refuses to overwrite without `--force`.
- **`claudectl supervisor validate <file>`** parses + validates without submitting, reporting the first problem with context — missing required field, dangling `depends_on`, or duplicate task name — instead of failing at insert time. `supervisor run` now runs the same validation before inserting.
- New **`examples/tasks/`** directory: `fan-out.toml`, `dependency-chain.toml`, `verify-then-merge.toml`. A test asserts the scaffold and all shipped examples parse and validate.

### Documented — MSRV (#328)
- README install section now states the **rustc 1.88+** requirement for source builds, so users on an older toolchain get a clear fix (`rustup update stable`) instead of an opaque transitive-dependency error. The `rust-version = "1.88"` pin was already in all three crate manifests; this closes the remaining doc gap.

Workspace crates bumped: `claudectl-core` and `claudectl-tui` → 0.54.0 (new `forecast` module + new public session API).

## [0.58.0] - 2026-06-25

### Added — Supervisor TUI panel (#368)
- **Full-screen Supervisor panel** in the dashboard (press `T`). The durable, verified task engine (`src/coord/`) was CLI-only; this renders the task ledger — `STATE / TRIES / TASK / ROLE / SESSION / UPDATED`, state-colored to match the session palette — so an operator can see tracked, verified work without dropping to `claudectl supervisor status`.
- Keymap: `T` opens; `j/k/g/G` navigate, `r` refresh, `Esc/T/q` close. Help overlay + draw dispatch wired. All behind the `coord` feature.
- **Contract extension**: new `TaskSummary` DTO + `CoordView::tasks()` on the UI↔runtime trait. `LiveCoordView::tasks()` reads coord `tasks` newest-first, deriving attempt count + latest session per task.

This is increment 1 (read-only). One-key retry/approve/cancel/drain, per-task cost + verifier-verdict columns, and sessions↔tasks linkage are the next increment of #368.

Workspace crates bumped: `claudectl-core` and `claudectl-tui` → 0.53.0 (the `CoordView` trait gained a method).

## [0.57.3] - 2026-06-22

### Fixed — notification cooldown + unified toggle (#364)
- Desktop notifications now route through a single gate (`App::notify_user`) that honors the master `notify` toggle **and** a per-event cooldown. Previously only the "needs input" notification was gated by `notify`; budget, context, and conflict pings fired regardless, so `notify = false` did not actually silence everything.
- **Anti-flap cooldown** — a session oscillating `NeedsInput ↔ Running` no longer re-pings on every transition. Per-event keys (`needs-input:<pid>`, `budget-warn:<pid>`, `conflict:<wt>`, …) scope the cooldown so each event fires at most once per window.
- New **`notify_cooldown_secs`** config knob (default **30s**), wired through all three layers: CLI `--notify-cooldown <SECS>`, `[defaults]` TOML, `config set`/`config show`, and known-keys validation.
- `fire_notification` no longer hardcodes `" needs input"` onto every message — budget/conflict notifications read correctly instead of e.g. "x budget needs input".
- New pure `should_notify` helper with unit tests for the never-fired / within-cooldown / after-cooldown cases.

Workspace crates bumped: `claudectl-core` and `claudectl-tui` → 0.52.2 (both touched by the fix).

## [0.57.2] - 2026-06-07

### Added — `claudectl init --upgrade` + plugin-version doctor row (closes #327)
- **`claudectl init --upgrade`** — re-sync everything the previous `init` wrote to match the running binary. Used after `brew upgrade claudectl` (or `cargo install ... --force`). Four steps, each with a ✓ / — / ✗ report:
  1. Claude Code hook entries (`~/.claude/settings.json`)
  2. Embedded plugin files (`~/.claude/plugins/claudectl/`) — checksums on-disk vs embedded before writing, so the report distinguishes "updated" from "unchanged"
  3. DB schema migrations (touching the bus + coord stores triggers `migrate(&conn)` as a side effect of `open()`)
  4. Onboarding marker version bump — when the recorded version differs from the binary's
- **`claudectl doctor` gains a `plugin version` row** — compares the on-disk `.claude-plugin/plugin.json` version against the binary's `CARGO_PKG_VERSION`. Pass when they match; Advisory when they differ, with `claudectl init upgrade` as the fix hint. This is how operators discover they need to upgrade in the first place.
- README "Get started" section and `docs/quickstart.md` gain a new "Upgrading" callout pointing at the new verb.
- 4 new init tests cover the upgrade helpers: first pass writes everything, second pass writes nothing, modified file gets rewritten, marker version bump round-trips.

Closes the last open issue of DX epic #320. With this shipped, all 8 sub-issues of the DX overhaul are done — `brew install` → `claudectl init` → `claudectl doctor` is a complete activation, and `brew upgrade` → `claudectl init --upgrade` is the complete refresh.

## [0.57.1] - 2026-06-07

### Added — bus retention + `prune` (closes #337)
- **`bus::store::prune(retention_days)`** — deletes `delivered` messages older than the cutoff (default 30 days, matches `coord::store::prune`). Pending and acked rows untouched. Returns the count deleted.
- **`claudectl bus prune [--days N] [--dry-run]`** — manual prune verb. Without `--days`, uses the 30-day default.
- **`claudectl doctor`** gains a `bus retention` row: Pass while the table is under 5000 messages; Advisory above, with a `claudectl bus prune` fix hint.
- New helpers `bus::store::message_count` and `bus::store::prune_dry_run` for the doctor advisory + dry-run.
- 5 new bus store tests cover prune semantics, dry-run, the zero-day edge case, the empty-table noop, and total-count accounting.

`docs/agent-bus.md` picks up a "Retention" section explaining the prune cadence and what stays vs goes.

Closes the "no retention path — `bus.db` grows forever" gap surfaced after the 0.57.0 release.

## [0.57.0] - 2026-06-07

The **DX overhaul release**. Closes #322, #324, #328, #325, #321, #326 — six issues from epic #320. A fresh Homebrew install now activates in three commands:

```bash
brew install mercurialsolo/tap/claudectl   # full feature set, bus included
claudectl init                              # writes plugin + hooks (slash commands, MCP, agent)
claudectl doctor                            # ✓ confirms everything is wired up
```

No repo clone. No manual MCP server registration. No "but how do I get the bus subcommand?"

### Added — `claudectl doctor` for unified install + runtime health (closes #326)
- **`claudectl doctor`** — top-down checklist answering "is everything wired up?" in one command. Replaces what was scattered across `--doctor` (terminal compat only), `init --check` (onboarding marker only), and ad-hoc probes.
- Checks: binary on PATH, Claude Code hooks installed, plugin files installed, brain endpoint reachable, bus feature compiled in, bus DB writable, session discovery working, terminal integration.
- Each check returns Pass / Advisory / Fail / Skipped with a one-line message and a fix hint. Failures advise the exact command to run (e.g. `claudectl init --plugin-only` when plugin files are missing).
- Exit code 0 when all Pass / Advisory / Skipped; non-zero on any Fail — pipeline-friendly.
- `--json` flag for scripting. Schema is stable: `[{name, status, message, fix_hint}]`.
- Legacy `--doctor` flag still works but prints a deprecation note pointing at the new subcommand. Will be removed one release after consolidation.
- 7 unit tests cover rendering, exit-code semantics, count math, and JSON round-trip.

Part of the DX overhaul epic #320.

### Changed — Homebrew bottle now ships with all features (closes #321)
- **`brew install mercurialsolo/tap/claudectl`** now produces a binary with `bus`, `coord`, `relay`, and `hive` all compiled in. `claudectl bus`, `claudectl coord`, `claudectl relay`, `claudectl hive` work end-to-end with no source rebuild. Binary grows from ~1.7 MB → ~6.3 MB; the async runtime exception for the `bus` feature is already documented in CLAUDE.md.
- **`cargo install claudectl`** still defaults to the minimal build (`hive` only). Users who want the full feature set use `cargo install claudectl --features bus,coord,relay,hive`.
- README and quickstart docs updated to surface both choices and call out the size trade-off.

Part of the DX overhaul epic #320.

### Added — Plugin embedded in binary (closes #325)
- **Plugin files now ship inside the `claudectl` binary** via `include_str!` — 17 files, ~29 KB total. `claudectl init` writes them to `~/.claude/plugins/claudectl/` automatically. No repo clone, no manual `.mcp.json` copy. The biggest single Homebrew-user UX win.
- **`claudectl init --plugin-only`** — install (or re-install) just the plugin without re-running the rest of the wizard. Useful after `brew upgrade claudectl`.
- Shell hook scripts (`brain-gate.sh`, `budget-check.sh`, `inbox-drain.sh`, `outcome-record.sh`, `session-briefing.sh`) are written with mode 0755 on POSIX.
- `init --remove` (soft uninstall) now also removes `~/.claude/plugins/claudectl/`. The on-disk plugin tree was claudectl-managed; the soft uninstall should treat it the same way as it does the hook entries in `settings.json`.
- 6 new tests in `init::plugin_assets` cover round-trip writes, idempotency, the executable-bit, removal, and missing-dir tolerance.

Part of the DX overhaul epic #320.

### Added — DX activation quick wins (closes #322, #324, #328)
- **First-run banner (#322)** — running `claudectl` for the first time (no `~/.claudectl/onboarding.json` and no `claudectl` entries in `~/.claude/settings.json`) now prints a one-screen banner above the TUI explaining how to onboard. Skipped in `--demo`, in non-TUI output modes (`--json`, `--list`, `--watch`, `--summary`, `--headless`), and when `CLAUDECTL_SKIP_FIRST_RUN=1`.
- **Brain phase ollama install hint (#324)** — when the Brain phase of `claudectl init` can't reach a local-LLM endpoint, it now prints concrete install steps (`brew install ollama && ollama serve &` + `ollama pull gemma4:e4b`) instead of silently recording `not_installed`. Non-interactive mode shows the hint too.
- **MSRV declared in `Cargo.toml`** (#328) — all three crates set `rust-version = "1.88"`. Users on older toolchains get a clean MSRV error from cargo before the resolver tries to compile transitive deps (which would otherwise produce opaque `darling@0.23.0 requires rustc 1.88.0` errors).

Part of the DX overhaul epic #320.

### Changed — bus role slash command renamed `/bind` → `/role`
- The plugin slash command shipped in 0.55.0 as `/bind <name>` is renamed to `/role <name>` (e.g. `/role frontend`, `/role tester`). Reads better — it matches the CLI noun (`claudectl bus role …`) and reflects what the operator is actually doing (setting a role, not binding to one).
- `claude-plugin/commands/bind.md` is removed in favour of `claude-plugin/commands/role.md`. The command instructions and `--self` ancestor-walk behaviour are unchanged; only the user-facing name changed.
- 0.55.0 / 0.56.0 install bases who typed `/bind` will need to retype `/role` after upgrading the plugin.

## [0.56.0] - 2026-06-07

### Added — `claudectl init --purge` for full uninstall
- **`claudectl init --purge`** — hard uninstall. Does everything `--remove` does (strips Claude Code hooks + clears the onboarding marker) **plus** wipes `~/.claudectl/` entirely (bus DB, brain decisions, hive knowledge, relay identity, coord state) and removes `~/.config/claudectl/config.toml`. Idempotent — re-running after a successful purge is a no-op.
- **`--yes`** flag pairs with `--purge` to skip the confirmation prompt for automation. Without it, you see a list of paths and confirm before anything is deleted.
- `--remove` is unchanged: it remains the safe form that preserves user data. The CLI help text now spells out the data-preservation contract.
- 3 new unit tests for the `remove_*_if_present` helpers (idempotency, recursive tree wipe, sibling preservation). Tested live end-to-end against a fake `$HOME`: plant fake artifacts → `--purge --yes` removes them → `init --non-interactive --skip-*` reinits cleanly.

## [0.55.0] - 2026-06-07

### Added — agent-bus role binding (closes #307, #310)
- **PID-keyed role bindings.** The bus `roles` table gained a nullable `pid` column. When set, the resolver walks the caller's parent process chain (depth 8 via `getppid` + native `ps`) and picks the first role bound to any ancestor pid before falling back to cwd-inference. Disambiguates "two sessions in one worktree" — different pids, same cwd, distinct roles.
- **`claudectl bus role bind <NAME> <CWD> --pid <PID>`** — explicit pid binding for orchestrator scripts.
- **`claudectl bus role bind <NAME> --self`** — auto-detects Claude's pid by walking the ancestor chain looking for a process whose `ps -o command=` contains `claude`. Captures the current cwd. Used by the new `/bind` slash command.
- **TUI `Ctrl+R`** on the selected session opens a `role>` prompt and binds the selected session's pid + cwd through the new `Actions::bind_bus_role` trait method. Detail panel now shows `Bus role: <name> (bound by pid|cwd)` so the current binding is visible at a glance.
- **`/bind <role>` plugin slash command** (`claude-plugin/commands/bind.md`) — operator types `/bind frontend` from inside a Claude session; the plugin runs `claudectl bus role bind --self frontend`.
- **`bus role list`** prints the new pid column; **`bus whoami --json`** payload gains a `pid` field.

### Added — role-name suggester (closes #309)
- **`claudectl bus role suggest [--pid <PID>] [--top N] [--json]`** — scans a session's transcript and cwd for signals and emits ranked role-name candidates. Pure analysis: never writes a binding, never queries the LLM.
- Four heuristic analyzers in `src/bus/suggest.rs`: cwd basename (with noise-suffix stripping), explicit role mentions in early user messages (`you are the X`, `acting as X`, `role: X`), tool fan-out shape (writes-heavy → `impl`, reads-heavy → `reviewer`, frequent test runs → `tester`), and path patterns in tool inputs (`frontend`, `backend`, `infra`, `tests`, `docs`).
- Transcript scan capped at 2 MiB and seeks to the file *tail* so recent activity drives suggestions and a runaway scan can't freeze the dashboard.
- 6 new unit tests; smoke-verified against a live Claude session.

### Internals
- Schema migration is idempotent — guarded by a `PRAGMA table_info` check before `ADD COLUMN` (SQLite has no `IF NOT EXISTS` for column adds). Existing cwd-only bindings keep working unchanged.
- `upsert_role` uses `COALESCE` on the pid update, so a re-bind that only refreshes `session_id` doesn't clobber an existing pid.
- New runtime trait method `Actions::bind_bus_role(name, cwd, pid)`; LiveActions writes through `bus::store::upsert_role`; off-bus builds return a clear error.
- 3 new bus tests covering pid precedence, fall-through, and pid-preservation on re-bind. All 30 bus tests pass (24 existing + 3 from #307 + 3 from #309's analyzers' shape; 6 new from #309 total in the suggest module).

## [0.54.0] - 2026-06-06

### Internals (workspace refactor — closes epic #279)
- **`claudectl-tui` extracted into its own crate (closes #275).** The `App` state struct (3300 LoC), every `ui/*` render module (table, detail, help, status_bar, peers, skills), the recorder pair, and the demo fixtures now live in `crates/claudectl-tui/`. Depends on `claudectl-core` only. The binary keeps `brain_screen.rs` (the full-screen Brain Review surface) because it imports `brain::metrics` and `brain::risk`.
- **Dependency direction enforced at three levels.** `claudectl → claudectl-tui → claudectl-core` is checked by (a) a grep guard against `crate::{brain,bus,coord,hive,relay,…}` inside `claudectl-core/src/`, and (b) two standalone build jobs (`Core (standalone)`, `TUI (standalone)`) that catch creeping cross-deps even when the workspace happens to compile.
- **Eight runtime traits + DTOs in `claudectl-core::runtime`** are the only surface between the TUI and the binary's brain/bus/coord subsystems: `SessionSource`, `BrainView`, `BrainReviewView`, `CoordView`, `BusView`, `Actions`, `HiveActions`, `Orchestrator`, plus the stateful `BrainDriver`. The binary's `src/runtime/` provides `Live*` adapters; `MockRuntime` drives in-crate tests.
- **`hooks.rs`, `launch.rs`, `skills.rs` moved into core** (#300), as did the `BrainConfig` and `IdleConfig` data structs (#301). The binary still owns TOML parsing and CLI flag layering; only the value types are downstreamed.
- **Feature propagation:** the binary's `coord`, `relay`, `hive` features now cascade into `claudectl-tui` via the `claudectl-tui/coord` notation in `[features]`, so the same `#[cfg(feature = "…")]` gates resolve consistently across both crates.
- **CLAUDE.md updated** to describe the post-refactor layout and the no-upward-deps rule (closes #278).

### Compatibility
- No user-facing CLI changes. Existing `crate::*` paths inside the binary continue to resolve unchanged thanks to a thin re-export bridge in `src/lib.rs` (`pub use claudectl_tui::{app, demo, recorder, session_recorder, ui};`).
- No new dependencies. `claudectl-tui` pulls only what the TUI already used (`ratatui`, `crossterm`, `serde_json`).

## [0.53.0] - 2026-06-06

### Added
- **`claudectl init` — opinionated onboarding wizard (closes #257).** Single canonical first-run flow that walks five phases in order: weekly budget cap, local-LLM brain auto-detection (probes ollama / llama.cpp / LM Studio / vLLM), Claude Code hook install, agent-bus role binding, and curated skill suggestions. Replaces the planned `claudectl setup` verb from `docs/AGENT_BUS.md` § 8 — onboarding lives in one place.
- **`claudectl init --non-interactive`** with per-phase flags (`--budget`, `--brain-url`, `--install-plugin` / `--skip-plugin`, `--bus-role` / `--bus-cwd`, `--skip-*` for every phase). For CI and dotfile automation.
- **`claudectl init --check`** — drift report. Detects each phase's current state and diffs against the recorded marker; exits non-zero when the live environment no longer matches what was onboarded.
- **`claudectl init --remove`** — uninstall every claudectl-managed artifact (hooks, marker). Phases that own user state (the bus DB, the config file's `budget` line) deliberately decline to delete it — we don't erase a user's setup, only artifacts claudectl actively manages.
- **`claudectl init --reset`** — clear the onboarding marker so the next `init` starts fresh. Doesn't touch installed artifacts.
- **`~/.claudectl/onboarding.json` marker** — durable record of which phases ran, when, and against which claudectl version. Loaded via `serde_json` with `#[serde(default)]` on optional fields so older markers stay forward-compatible.

### Changed
- **Existing `--init` / `--uninstall` flags** are now deprecated aliases. They still write/remove the hook entries (existing dotfile automation keeps working), but each prints a deprecation note pointing at the new `init` subcommand. Slated for removal one release after consolidation.

### Internals
- New `src/init/` module replacing the single-file `src/init.rs`:
  - `hooks.rs` — moved unchanged from the old `init.rs` (the hook writer the plugin phase delegates to).
  - `marker.rs` — atomic-rename `OnboardingMarker` read/write at `~/.claudectl/onboarding.json`.
  - `prompt.rs` — minimal stdin/stdout helpers (yes/no, number-or-default, line-or-default).
  - `state.rs` — environment probes for each phase. Uses `curl --max-time 1` for HTTP probes (matching the existing brain client pattern; no new deps).
  - `phases.rs` — `Phase` trait + `Budget` / `Brain` / `Plugin` / `Bus` / `Skills` impls + the ordered `registry()`. Single uniform shape so the wizard, `--check`, and `--remove` all walk the same list without per-phase branching.
  - `mod.rs` — orchestrator (`run_wizard`, `run_non_interactive`, `run_check`, `run_remove`, `run_reset`) plus the drift-comparison logic (`is_drift` treats `not_installed` and `skipped` as equivalent so the report only flags real divergence).
- 21 new unit tests (marker roundtrip, drift comparison matrix, phase registry order, role-from-cwd derivation, TOML upsert, status-label stability). Plus a 9-scenario end-to-end smoke verifying every CLI verb (non-interactive all-skipped → marker → `--check` green → tamper → `--check` drift → `--remove` cleans up settings.json and marker; legacy `--init` still works and prints the deprecation note).

### Compatibility
- The Phase trait lets every phase live in its own file with no per-phase branching in the orchestrator — adding a new phase later (e.g., "MCP plugin discovery") is one new impl plus one line in `registry()`.
- No new dependencies. The wizard's brain probe and the budget-config writer both use the project's existing patterns (`curl` shell-out, tiny TOML upsert that avoids a `toml` crate dep).

## [0.52.0] - 2026-06-06

### Added
- **Agent bus Stop-hook delivery (Trigger A, phase 5 of `docs/AGENT_BUS.md`)** — closes the loop on bus messaging. After every turn finishes, the Claude Code plugin's new `Stop` hook drains the caller's mailbox and, when mail is present, returns `decision: "block"` with the rendered messages as `additionalContext`. The agent picks the work up **in the same turn** without waiting for the user to type `/inbox`. The bus is now self-driving.
- **`claudectl bus stop-hook` subcommand** — owns the Claude Code Stop-hook output protocol. Silent + exit 0 on every failure mode (no role bound, empty inbox, missing DB, ambiguous cwd) so the hook can never block a session because of a bus problem. All logic lives in Rust (`src/bus/stop_hook.rs`) where it is unit-tested.
- **`--json` flag on `bus inbox` and `bus whoami`** — machine-readable output for tooling. `inbox --json` soft-fails on unbound/ambiguous cwds (returns `{"role":null,"messages":[],"note":"..."}`) so the Stop hook never errors out on a session that hasn't bound a role yet.
- **`claude-plugin/hooks/scripts/inbox-drain.sh`** — Stop-hook wrapper installed by the plugin. Intentionally thin: protects against the case where `claudectl` is not on PATH, then delegates to `claudectl bus stop-hook`. Wired into `hooks.json` with a 5 s timeout.

### Internals
- New `src/bus/stop_hook.rs` module owning the Stop-output schema (`StopHookResponse`, `HookSpecificOutput`) and the markdown rendering of drained messages into context. Decoupled from the CLI so the schema is independently testable.
- `dispatch_inbox` / `dispatch_whoami` in `src/bus/cli.rs` refactored to separate data-fetch from rendering. Both now share a single `fetch_inbox` helper; human and JSON paths render the same `InboxOutcome` differently.
- 5 new unit tests covering Stop-hook envelope shape, pluralization, JSON wire format, and the critical "no raw newlines inside the JSON string fields" invariant (caught a real bug in development).

### Compatibility
- `claudectl bus inbox` without `--json` is unchanged — human-readable output, errors interactively on unbound/ambiguous cwds.
- No new dependencies. Stop hook ships behind the existing `bus` feature.

## [0.51.0] - 2026-06-06

### Added
- **Agent bus (phases 1–4 of `docs/AGENT_BUS.md`)** — a durable role directory + persistent mailbox exposed as an MCP server. Running Claude Code sessions discover each other (`list_agents`), look up their own role (`whoami`), send directed messages (`publish`), and drain their inbox (`read_inbox`) at turn boundaries. Gated behind the new opt-in `bus` Cargo feature.
- **`claudectl bus` CLI** with five verbs: `stdio` (run the MCP server, what the plugin invokes), `role bind/list` (durable role addresses), `send` (directed messaging), `inbox` (drain queued messages), `whoami` (resolve the caller's role from cwd or `CLAUDECTL_BUS_ROLE`).
- **Mailbox persistence** at `~/.claudectl/bus/bus.db` (SQLite WAL). Survives restarts; the role address outlives the session it was last bound to.
- **Content sanitization at the injection boundary.** A leading `/` in a message body is neutralized before delivery so a queued message cannot smuggle a slash command into the recipient. Subject grammar, type allowlist, and an 8 KiB body cap also enforced.
- **Claude Code plugin updates.** `claude-plugin/.mcp.json` registers the bus as an MCP server; `claude-plugin/commands/inbox.md` is the new `/inbox` slash command that drains the caller's mailbox through the `read_inbox` tool.

### Changed
- **Architecture invariants in `CLAUDE.md` carve out an exception for the `bus` feature.** The bus pulls rmcp + a current-thread Tokio runtime, deliberately relaxing the no-async-runtime rule for that feature path only. Default build is unchanged at ~3.5 MB / <50 ms startup; `--features bus` is ~6.4 MB.
- **Plugin manifest version** (`claude-plugin/.claude-plugin/plugin.json`) synced from a drifted `0.48.0` back to the crate version.

### Internals
- New `src/bus/` module: `store.rs` (SQLite schema, drain-on-read), `roles.rs` (cwd inference with macOS symlink canonicalization), `policy.rs` (sanitization + validation), `mcp.rs` (rmcp stdio server), `cli.rs` (subcommand dispatch).
- 16 new unit tests covering role resolution, ambiguity, env override, priority-ordered drain, drain-once idempotency, leading-`/` neutralization, subject grammar, and type allowlist. End-to-end MCP handshake + CLI roundtrip exercised before merge.

## [0.50.0] - 2026-05-29

### Added
- **Brain Scorecard** (`claudectl --brain-stats scorecard`). One-screen periodic-review surface: north-star auto-handled accuracy, guardrails (Critical-tier false-approve count + rolling override rate), latency p50/p95/p99, few-shot cache hit rate, per-risk-tier accuracy breakdown, counterfactual summary, and review status. The single command you want to run to see whether the brain is healthy.
- **Per-risk-tier breakdown** (`--brain-stats tier`). Accuracy, false-approves, false-denies, and override rate split by `Low` / `Medium` / `High` / `Critical` tier. Critical-tier false-approves are flagged with a warning marker — they are the safety-critical number.
- **Latency report** (`--brain-stats latency`). p50/p95/p99/mean/max + ASCII distribution histogram over the new `brain_decision_ms` field. Reads gracefully on histories without instrumentation.
- **Cache hit report** (`--brain-stats cache`). Percentage of decisions handled from the few-shot store without an LLM call, over the new `cache_hit` field.
- **Counterfactual analyzer** (`--brain-stats counterfactual`). Surfaces user-overrides where the subsequent same-PID outcome failed (brain was right) or succeeded (brain over-cautious). Each entry prints a one-shot `--brain-mark-canonical <id>` command for promotion.
- **Interactive review CLI** (`claudectl --brain-review`). Walks the prioritized queue (counterfactual brain-was-right → Critical-tier false-approves → high-confidence calibration misses) one decision at a time with `m`/`n`/`s`/`d`/`q` controls. `--brain-review list` prints the queue non-interactively.
- **Canonical teaching store**. Decisions marked canonical (via the review flow or `--brain-mark-canonical <id>`) are appended to `~/.claudectl/brain/canonical.jsonl` and get a `+50` score boost in `retrieval::retrieve_similar`, so reviewed examples dominate future few-shots. Each review pass becomes supervised training signal.
- **Brain Review TUI mode** (`M` hotkey). Full-screen mode integrated with the dashboard: Scorecard tab mirrors the CLI scorecard; Review tab provides a list + detail split with `j/k` navigate, `m` mark canonical, `n` mark with inline note, `s` skip, `r` refresh, `Tab` cycle, `Esc/M/q` close. Marked items drop from the queue in-place and selection advances — triage is one keystroke per item.
- **DecisionRecord schema extensions**. `brain_decision_ms: Option<u64>`, `cache_hit: Option<bool>`, `canonical: Option<bool>` on every record. `Option`-wrapped for full backward compat with existing decision logs. New `log_decision_full(..., brain_decision_ms, cache_hit)` for instrumented call sites; the legacy `log_decision` continues to work and writes `None`.
- **Source-built `packaging/homebrew-core/claudectl.rb`** formula template with `livecheck`, `generate_completions_from_executable`, `man1` install, and a real `test do` block. Submitted to Homebrew/homebrew-core (declined for now per the self-submission notability bar — re-pursueable when star/fork/watcher thresholds are cleared).

### Changed
- **Repositioned every public-facing surface** to the new tagline: *"Orchestrate a swarm of Claude Code agents with a local-LLM brain that learns from you."* Lands in the README hero, mkdocs site description, `docs/index.md`, `docs/llms.txt`, `Cargo.toml` description, clap `--help`, `flake.nix`, `AGENTS.md`, `CLAUDE.md`, `LAUNCH_POSTS.md`, `blog/posts.md`, the nixpkgs handoff README, and the GitHub repo description. Homebrew `desc` and AUR `pkgdesc` use the 73-char trimmed variant *"Orchestrate a swarm of Claude Code agents with a learning local-LLM brain"* to fit their 80-char cap.
- **Homebrew-core template** `desc` trimmed from 82 → 61 chars to clear `brew audit --strict --new --online`'s 80-char limit, and pinned to the v0.49.3 source-tarball sha256 so future bumps start from a known-good baseline instead of the `REPLACE_WITH_SHA256` placeholder.

### Internals
- Bulk-extended every `DecisionRecord` construction site across `briefing.rs`, `detectors.rs`, `insights.rs`, `metrics.rs`, `pref_store.rs`, `preferences.rs`, `retrieval.rs`, `sequences.rs`, and `hive/distiller.rs` for the three new fields. All existing tests pass without modification.
- `src/brain/review.rs` (new): `ReviewItem`, `build_queue`, `mark_by_id`, `run_interactive`, `print_queue`.
- `src/ui/brain.rs` (new): full-screen renderer mirroring the Skills & Hive K-screen pattern.

## [0.49.3] - 2026-05-27

### Added
- **`CLAUDECTL_DEMO_SKILLS=1` demo recording hook.** With this env var plus `--demo`, claudectl boots straight into the Skills & Hive mode with a scripted tab-rotation in `refresh_demo` (Skills → Hive → Skills every 14 ticks) and seeded peer/invite data so the Hive tab renders convincingly even without the `relay` feature compiled in. Lets `scripts/record-demos.sh skills` produce a deterministic GIF for launch posts.
- **`scripts/record-demos.sh skills` target** — records `docs/assets/claudectl-demo-skills.gif` (30 s, both tabs) using the same agg flags as the other demo gifs. Bundled into the `all` target.
- **`docs/assets/claudectl-demo-skills.gif`** — embedded in `docs/index.md` Screenshots and `docs/reference.md` Skills & Hive section.

## [0.49.2] - 2026-05-27

### Fixed
- **Skills & Hive footer now sticks to the bottom** of the screen. The previous version reserved 9 rows for the footer but only filled 3–4, leaving a band of empty space above the bottom border. Restructured so the body uses `Min` and the hint strip is a tight 1–2 rows pinned at the bottom; selected-skill detail (path + status) moves into the body section above the hint.

## [0.49.1] - 2026-05-27

### Changed
- **Skills & Hive is now a full-screen mode**, not a centered overlay. Pressing `K` swaps the entire frame from the session table to the Skills & Hive view; `Esc` / `K` / `q` returns to the table. Same two tabs (Skills, Hive) and same hotkeys as before.
- **`K:skills` hint added to the bottom footer** of the session table so the shortcut is discoverable. Empty-state hint (no sessions) also calls out `K`.

## [0.49.0] - 2026-05-27

### Added
- **Skills & Hive TUI overlay** — press `K` from the TUI to open a Skills & Hive panel. Two tabs (Tab to switch):
  - **Skills tab** lists every Claude Code skill on disk (scans `~/.claude/skills`, `~/.claude/plugins/*/skills`, and `<cwd>/.claude/skills`). A `✓` marker shows which skills are already shared with the local hive; `s` shares the highlighted skill via the existing hive pipeline. Honours the 32 KiB skill-share limit and surfaces a warning when a skill exceeds it.
  - **Hive tab** shows local identity, listener status, and known peers (read from `~/.claudectl/relay/peers/`). Hotkeys: `h` start hive listener (spawns detached `claudectl relay serve`), `i` generate an invite (relay code, word phrase, and invite link, shown inline), `J` join a hive via pasted code/link/words (detached `claudectl relay join`), `r` refresh peers.
- **`hive::cli::share_artifact_from_path()`** — public wrapper around the previously private CLI-only `cmd_share` so callers outside the dispatch table (the new TUI overlay) can share skills/commands/hooks without reimplementing frontmatter + scope parsing.
- **`src/skills.rs`** — new skill discovery module with YAML frontmatter parsing, source classification (user / plugin / project), and a shared-key lookup that aligns with the hive's `skill:<lowercased-name>` semantic key.

### Technical details
- Detached subprocess spawning for `relay serve` and `relay join` keeps the TUI event loop responsive; the invite generator shells out to `claudectl --json relay invite --words` and parses the JSON envelope.
- New module wired into `src/lib.rs`, `src/main.rs`, and `src/ui/mod.rs`; help overlay (`?`) gains a `K` entry.
- 585 tests passing (5 new: 3 for the skills module, 2 for the overlay rendering).

## [0.48.0] - 2026-05-12

### Added
- **Test-failure feedback loop** -- when a configured test runner (`cargo test`, `npm test`, `pytest`, `go test`, `bun test`, ...) exits non-zero, the reaper fans the failure out to the most recent brain-approved `Edit`/`Write`/`MultiEdit`/`NotebookEdit` decisions in the same project within a 5-minute window and tags them as `DecisionOutcome::TestFailed` (#238). Distillation weights `TestFailed` more strongly than transient `Error` (0.1 vs 0.3 for accepted-but-broken; 2.0 vs 1.5 for rejected-rightly), so a broken build is the strongest negative signal the brain has.
- **`test_runners` config** -- `[brain]` section accepts an override list; sensible defaults cover the major language runners. Empty list disables fan-out.
- **`continueOnBlock` for deny reasoning** -- `brain-gate.sh` emits the `hookSpecificOutput.continueOnBlock` envelope alongside the legacy `{decision, reason}` so newer Claude Code surfaces `permissionDecisionReason` and `systemMessage` into the model's next turn instead of blocking opaquely (#249). The brain stops being a wall and starts being a teacher.
- **Below-threshold approval advisory** -- uncertain approvals (below the adaptive threshold) emit `hookSpecificOutput.additionalContext` so Claude picks up the brain's hesitation without being blocked.
- **Robust hook output** -- `brain-gate.sh` now prefers `jq` for parsing the brain's response and constructing its envelope, with a manual JSON-escape fallback. Fixes a latent bug where reasoning containing quotes, backslashes, or newlines could corrupt the hook's stdout.

### Technical details
- `DecisionOutcome::TestFailed(String)` carries the failing test command; backfill overlays it onto `DecisionRecord.outcome` after the consecutive-pair pass so a marker beats a clean tool-error signal.
- `BrainConfig.test_runners: Vec<String>` parsed from `[brain]` TOML; `default_test_runners()` exposed for tests.
- Fan-out is idempotent via `create_new` on `test-failures/<decision_id>.json` markers; 5-minute attribution window, capped at 5 recent edits per failure.
- New hook envelope is unconditional -- Claude Code < 2.1.138 ignores the extra fields and falls back to the legacy deny.
- 1240 tests passing across all build configurations (12 new for this release).

## [0.45.0] - 2026-04-28

### Added
- **Configurable event log retention** -- `retention_days` in `[lifecycle]` config section controls auto-prune period (default 30 days), wired to headless auto-prune loop (#186)
- **Per-session recording toggle** -- `R` key now starts/stops recording for the selected session only, not all recordings. Recordings include a ~30-second lookback buffer of events before record-start. Output filenames include timestamps for uniqueness (#73)
- **Config validation** -- `claudectl --config-validate` reports unknown keys, unknown sections, and malformed values in config files with line numbers and actionable messages (#74)
- **Hook dry-run** -- `claudectl --init --dry-run` shows what hooks would be written to `.claude/settings.json` without modifying the file (#74)
- **Sample config generation** -- `claudectl --config-init` writes an annotated `.claudectl.toml` template in the current directory (#74)
- **False-deny friction cost** -- `--brain-stats false-deny` now shows friction cost (avg override delay, total friction time) and override reason breakdown. Brain denial overrides prompt for categorized reasons: always safe, one-time exception, or brain is wrong (#134)
- **Override reason capture** -- when accepting a brain denial, TUI prompts for override reason (1/2/3 keys) to feed back into preference distillation
- **Decision record timestamps** -- `resolved_at` field on all brain decisions enables friction latency measurement

### Technical details
- `LifecycleConfig` gains `retention_days: u64` (default 30), parsed from `[lifecycle]` TOML section
- `SessionRecorder` lookback: seeks back 50KB and aligns to line boundary before recording
- `validate_config_file()` enumerates valid keys per known section and reports unknowns
- `DecisionRecord` gains `resolved_at: Option<u64>` and `override_reason: Option<String>`, backward-compatible with old JSONL
- 677 tests passing across all build configurations

## [0.44.0] - 2026-04-27

### Added
- **Session state in heartbeats** -- relay heartbeats now carry the worker's session list, enabling cross-machine visibility (#107)
  - `WorkerState` storage in `PeerRegistry` with automatic stale worker expiry (3x heartbeat interval)
  - Backward compatible: peers running older versions send empty payloads (liveness-only)
- **HTTP coordinator API** -- lightweight raw-TCP HTTP/1.1 server for coordinator mode, zero new dependencies (#108)
  - `POST /api/heartbeat` -- receive worker session state
  - `GET /api/sessions` -- unified session list across all connected workers
  - `GET /api/workers` -- worker status summary with staleness detection
  - Bearer token auth, 1 MB body cap, background thread
  - CLI: `claudectl relay serve --http-port 9876 --auth-token <token>`
  - Config: `[relay]` section supports `http_port` and `auth_token`
- **Unified dashboard** -- remote sessions from connected workers appear in the TUI alongside local sessions (#109)
  - Remote sessions shown with `[worker-id] project` prefix
  - Terminal actions (kill, approve, input, compact, switch) gracefully blocked for remote sessions
  - Peers panel now shows session count per peer
  - Demo mode includes fake remote sessions from connected peers
- **Secure pairing** -- confirmed already complete: HMAC challenge-response, PSK, invite codes/words/links/QR, LAN discovery, rate limiting (#113)

### Technical details
- All new code feature-gated behind `--features relay` -- default build unaffected
- HTTP server uses `std::net::TcpListener` (same pattern as relay listener) -- no new runtime dependencies
- `ClaudeSession` gains `worker_origin: Option<String>`, `is_remote()`, and `from_remote_json()` for remote session hydration
- 596 tests passing across all build configurations

## [0.33.0] - 2026-04-21

### Added
- **Coordination layer** -- local-first coordination plane for multi-agent coding workflows (`--features coord`) (#180, #181, #182, #183)
  - **Phase 0: Event Log** -- SQLite-backed event store with typed records for leases, blockers, interrupts, handoffs, and memory. FTS5 full-text search. 19 CLI subcommands under `--coord`
  - **Phase 1: Ownership and Handoffs** -- `claim`/`release` with exclusive conflict detection, structured handoff packets with goal/artifacts/next_steps, TUI badges (`L`/`H`/`I`) and detail panel coordination section
  - **Phase 2: Interrupt Bus** -- typed interrupt delivery with 4 modes (immediate, safe_boundary, waiting_only, manual_review), deduplication, expiry, full lifecycle tracking (pending -> delivered -> acknowledged), wired into app tick loop
  - **Phase 3: Memory Promotion and Injection** -- promotes high-confidence brain patterns into typed memory records, injects compact coordination context (leases, conflicts, blockers, handoffs, memory) into brain prompts before every decision
  - **Phase 4: External Agent Adapters** -- `AgentAdapter` trait with capability negotiation, Claude Code adapter wrapping existing discovery/terminal code, Codex stub adapter
  - **Evaluation layer** -- 10 coordination eval scenarios, metrics engine computing conflict rate, handoff completion rate, interrupt delivery rate, blocker resolution time from the event log
- **`--headless` mode** -- run the full autonomous stack (brain + coordination + context rot prevention) without a TUI. Attach a dashboard from another terminal. Emits structured JSON events to stdout
  - Automatic context rot intervention: raises `compact` interrupts at decay >= 50, `stop` at >= 85
  - Periodic coordination summaries every ~30s
  - Usage: `claudectl --headless --brain --auto-run`

### Technical details
- Feature-gated behind `--features coord` -- default build is unaffected (zero cost for users who don't need coordination)
- 12 new source files in `src/coord/` (adapter, CLI, evals, injection, interrupt bus, metrics, promotion, store, types)
- SQLite with WAL mode for concurrent access between headless and TUI processes
- 454 tests passing across both build configurations

## [0.32.0] - 2026-04-20

### Added
- **6 new `--brain-stats` subcommands** completing all metrics issues (#174, #175)
  - `distribution` — decision volume by tool, risk, project, action with inline bar charts
  - `novel-rate` — how quickly the frontier of novel situations shrinks
  - `false-deny` — false-deny rate and friction cost with 30% warning threshold
  - `calibration` — confidence vs actual accuracy, ECE score, per-tool calibration gap
  - `incidents` — post-mortem of every false approval with root cause classification
  - `time-to-correct` — user reaction latency to brain suggestions (protege effect)
- **`suggested_at` timestamp** on brain suggestions for reaction latency measurement (#175)
- **Demo mode** now shows cognitive decay icons and more brain activity (#173)

### Changed
- **Refactored `decisions.rs`** (3074 lines) into 3 focused modules: `decisions.rs` (992), `preferences.rs` (1950), `retrieval.rs` (302) (#176)

## [0.31.0] - 2026-04-19

### Added
- **Claude Code plugin** — integrates the brain directly into Claude Code sessions, no TUI required (#169)
  - PreToolUse hooks: `brain-gate.sh` (auto-approve/deny) and `budget-check.sh` (spend limits)
  - Slash commands: `/sessions`, `/spend`, `/brain-stats`, `/brain`, `/auto-insights`
  - Supervisor agent for session health triage
  - Session monitoring skill (auto-activated)
- **`--init` / `--uninstall`** — one-command setup to wire up Claude Code hooks in `.claude/settings.json` (#169)
- **`--brain-query`** — standalone brain query for single tool-call decisions (JSON output), used by plugin hooks (#169)
- **`--mode on|off|auto|status`** — toggle brain gate mode mid-session without restarting (#169)
- **`-s / --scope`** — configure hooks at user or project level (#169)
- **Auto-insights** — self-improving session analysis that detects friction patterns from brain decision history (#170)
  - 7 detectors: friction patterns, error loops, context blowouts, missing rules, accuracy gaps, temporal friction, cost trends
  - Differential tracking: only new insights are surfaced (fingerprint-based dedup)
  - `--insights [on|off|status]` — view insights or enable auto-generation every 10 decisions
  - `/auto-insights` plugin command
- **Impact scorecard** — `--brain-stats impact` with visual card layout, bar charts, and headline metrics (#171)
  - Auto-approve rate, brain accuracy, coverage vs static rules, dangerous ops blocked, time saved, learning curve
- Star prompt after first successful run (#168)
- `--demo` mode for fake sessions without Claude Code (#168)

## [0.30.0] - 2026-04-18

### Added
- **Cognitive rot detection** — temporal health monitoring that detects session degradation over time, not just point-in-time snapshots (#165)
  - Composite decay score (0-100) combining context saturation, error acceleration, token efficiency decline, and file re-read repetition
  - `check_proactive_compaction` — suggests `/compact` at 50% context usage (research shows degradation begins at 40-50%), independent of existing 80/90% context thresholds
  - `check_token_efficiency` — detects when a session spends increasingly more tokens per file edit vs its frozen baseline
  - `check_error_acceleration` — detects rising error rates over sliding windows vs a frozen baseline
  - `check_repetition` — detects files being re-read repeatedly without intervening edits (agent confusion signal)
  - `check_cognitive_decay` — composite check with severity-ranked icons: `◐` early (30-59), `◉` significant (60-79), `⊘` severe (80-100)
- **Cognitive Health section** in the detail panel showing decay score, efficiency vs baseline, error trend, repetition count, and context-aware mitigation suggestions
- `decay_score` field in `--json` output for programmatic access
- Brain context now includes decay score so the LLM factors cognitive health into its decisions
- Four new configurable thresholds in `[health]`: `decay_compaction_pct`, `efficiency_critical_factor`, `error_accel_factor`, `repetition_threshold`
- Demo mode showcases cognitive decay indicators on the ml-pipeline session

## [0.29.3] - 2026-04-18

### Fixed
- Kitty terminal not detected on Linux — `detect_terminal()` now checks `KITTY_WINDOW_ID` and `TERM=xterm-kitty` env vars before falling back to `TERM_PROGRAM`. Kitty on Linux doesn't set `TERM_PROGRAM`. (#160)
- Added native env var detection for WezTerm (`WEZTERM_EXECUTABLE`) and Ghostty (`GHOSTTY_RESOURCES_DIR`) as fallbacks when `TERM_PROGRAM` is not set.
- "No TTY associated with this session" error when pressing Tab in kitty — the blanket TTY guard now only applies to terminals that match by TTY name (tmux, WezTerm, iTerm2, Terminal.app). Kitty, Ghostty, and Warp use PID/cwd-based IPC and don't need a TTY. (#160)

## [0.29.2] - 2026-04-18

### Fixed
- Active sessions showing "No transcript" when JSONL files exist on disk. `cwd_to_slug` now strips trailing slashes before encoding, and a new fallback scan searches all project directories by session ID when the slug-based lookup fails. (#161)

### Added
- `--doctor` now includes a "Transcript Discovery" section that shows each active session's cwd, computed slug, and resolved JSONL path (or the exact paths tried when resolution fails).
- Debug-level logging at the transcript discovery step, showing which paths were tried and whether the fallback scan was used.

## [0.29.1] - 2026-04-17

### Fixed
- Few-shot retrieval rejection weight now auto-calibrates based on the user's actual accept/reject ratio instead of using a hardcoded factor of 8. Rare rejections (99/1) get amplified to 12, typical ratios (90/10) produce ~9, and frequent rejecters (60/40) see the weight drop to the floor of 3. (#158)

## [0.28.0] - 2026-04-16

### Added
- `--brain-stats` CLI command with four metrics subcommands for measuring brain effectiveness:
  - `learning-curve`: rolling correction rate over decision history with ASCII chart, phase transition detection, and improvement tracking (#129)
  - `accuracy`: per-tool, per-risk-tier, per-project, and temporal accuracy breakdown (#131)
  - `baseline`: replay all decisions against a deterministic rules-only classifier and compare accuracy by risk tier, with agreement analysis (#136)
  - `false-approve`: false-approve rate on risky actions by risk tier, with worst-case audit trail (#133)
- Risk tier classification system (Low/Medium/High/Critical) based on tool type and command patterns, shared across all metrics
- `src/brain/metrics.rs` module with 19 unit tests
- Passive observation logging: brain learns from ALL user actions, not just brain-involved decisions. Manual approves (`y` key), user input (`i` key), per-PID auto-approve (`a` key), static rule execution, and file conflict auto-deny all generate learning signals
- Multi-level learning architecture with four dimensions of intelligence:
  - **Rich context logging**: every decision captures 13 session state fields (cost, context%, errors, model, elapsed time, files modified, tool calls, conflicts, burn rate, subagents) — zero inference cost
  - **Conditional preferences**: distillation now learns context-dependent rules via Gini impurity splits (e.g., "approve git push when cost<$5", "deny writes when context>80%")
  - **Outcome tracking**: correlates consecutive decisions to detect "user accepted but it broke" (downweighted) vs "user rejected and it would have broken" (reinforced)
  - **Temporal patterns**: detects error streaks, cost pressure, and context pressure as compact situational rules in the prompt

### Fixed
- Observation records (passive learning signals) were silently dropped by the parser because `brain_action` field was required but observations have `null` — now correctly parsed

## [0.26.0] - 2026-04-16

### Added
- Continuous learning system: brain now closes the feedback loop — every accept, reject, auto-execute, and deny-rule override is logged and used to improve future decisions
- Preference distillation: decision history is periodically compacted into `~/.claudectl/brain/preferences.json` with compact rules like "always approve [Read]" — uses ~200 tokens vs ~500+ for raw few-shot examples, critical for Gemma4's limited context window
- Outcome-weighted few-shot retrieval: rejected decisions score higher than accepts (corrections are the strongest learning signal), with recency bonus for newer decisions
- Adaptive confidence thresholds: per-tool accuracy tracking adjusts the auto-execution bar — high-accuracy tools get lower thresholds (0.5), low-accuracy tools require 0.95 confidence. Below-threshold suggestions are automatically demoted to advisory mode
- Smart context budgeting: when distilled preferences exist, raw few-shot count is reduced to save context for transcript and decision prompt
- Auto-mode decision logging: all auto-executed brain suggestions are now recorded to decisions.jsonl (previously only advisory-mode accept/reject was captured)
- Deny-rule override logging: when static deny rules override brain suggestions, the override is logged so the brain learns the boundaries

## [0.25.2] - 2026-04-15

### Fixed
- Permission prompt detection: sessions waiting for tool approval (e.g., Web Search "Do you want to proceed?") now immediately show "Needs Input" instead of incorrectly showing "Processing" — uses pending_tool_name as primary signal instead of relying on 5-second age threshold

## [0.25.1] - 2026-04-15

### Fixed
- UTF-8 panic when brain processes transcripts containing multi-byte characters (em dashes, unicode, etc.) — 10 unsafe byte-slice truncations replaced with char-boundary-safe helper across 4 files

## [0.25.0] - 2026-04-15

### Added
- File-level conflict detection: detects when multiple sessions edit the same file, with `!F` indicator in dashboard and per-file detail in the expanded panel
- Predictive conflict detection: flags pending Edit/Write tool calls that target files already modified by another session
- Auto-deny for file conflicts: `[orchestrate] auto_deny_file_conflicts = true` automatically denies writes to files being edited by another session, with actionable error message naming the conflicting session
- `match_file_conflict` rule condition: match sessions with pending file conflicts in the auto-rule engine
- `pending_file_path` tracking: Edit/Write/NotebookEdit tool calls now track the target file path for conflict detection
- `[orchestrate]` config section with `file_conflicts` (default true) and `auto_deny_file_conflicts` (default false)
- Configurable health check thresholds via `[health]` TOML section — all 5 checks (cache, cost spike, loop, stall, context) accept user-defined thresholds
- Capture actual error messages from tool results — detail panel shows "Recent Errors" section with tool name and message text
- `--config-template` flag prints a fully annotated `.claudectl.toml` with all available settings
- CLI flags grouped by purpose in `--help`: Dashboard, Output Modes, Filtering, Session Management, Budget & Notifications, Brain, Orchestration, Recording, Cleanup, History & Diagnostics

### Fixed
- Brain connection failure now shows in the TUI status bar instead of being lost to stderr

### Changed
- Orchestrator shows task plan at launch, uses `[n/total]` progress fractions and terminal-width-aware status line

## [0.24.0] - 2026-04-15

### Added
- Session health monitoring with visual icons: 🔥 low cache, 💸 cost spike, 🔄 looping, 🐌 stalled, 🧠 context full — proactively detects cache TTL bugs, cost anomalies, retry loops, and context saturation
- Health icons appear next to project name in the dashboard table, sorted by severity

## [0.23.2] - 2026-04-15

### Fixed
- Ghost sessions from PID reuse: when a Claude Code process exits and macOS reassigns the PID to another process, claudectl now correctly detects the mismatch and marks the session as Finished instead of showing stale status

## [0.23.1] - 2026-04-15

### Fixed
- Brain client now auto-detects OpenAI-compatible endpoints (/v1/chat/completions) vs ollama (/api/generate) from the URL — llama.cpp, vLLM, and LM Studio now work correctly without extra config

## [0.23.0] - 2026-04-15

### Added
- External agent integration: register agents (Codex, Aider, custom) via `[agents.*]` config, brain can delegate work to them with output capture to `.claudectl-runs/agents/`
- `RuleAction::Delegate` — brain can delegate work to named agents
- Agent output logged to `.claudectl-runs/agents/{name}.{timestamp}.log`

## [0.22.0] - 2026-04-15

### Added
- Externalized prompt library: all brain prompts loaded from `~/.claudectl/brain/prompts/` with built-in fallbacks, users can override any prompt template
- Local eval framework: `--brain-eval` runs 6 built-in scenarios (approve/deny/send) against the local LLM and reports accuracy
- Custom eval scenarios via JSON files in `~/.claudectl/brain/evals/`
- `--brain-prompts` CLI command lists all prompt templates and their source (built-in vs user override)

## [0.21.0] - 2026-04-15

### Added
- Dynamic auto-orchestration: the brain periodically evaluates all sessions and decides cross-session actions (spawn, route, terminate) without a pre-written tasks.json
- Configurable orchestration interval (default 30s) and max_sessions limit
- `--auto-run` flag (renamed from `--brain-auto`) for cleaner CLI
- Documentation for all supported LLM backends (ollama, llama.cpp, vLLM, LM Studio)

## [0.20.0] - 2026-04-15

### Added
- Spawn action: the brain can launch new Claude Code sessions with derived prompts, with configurable `max_sessions` limit (default 10)
- Persistent mailbox system: messages between sessions are queued in `~/.claudectl/brain/mailbox/` and delivered when the target session is ready (WaitingInput), preventing interruption during active work
- Smart routing: Route action queues to mailbox when target is busy, delivers directly when target is waiting

## [0.19.0] - 2026-04-15

### Added
- Cross-session awareness: the brain now sees all active sessions (project, status, pending tool, cost, context%) when evaluating any single session, enabling cross-session reasoning
- Inter-session routing: new `Route` action lets the brain send summarized output from one session to another via the local LLM, preventing context bloat in the target session
- Auto-summarization: `summarize_for_routing()` asks the local LLM to compress source output for the target's specific task context before sending

## [0.18.2] - 2026-04-15

### Added
- Brain diagnostics in `--doctor`: checks curl, ollama binary, config status, and endpoint reachability
- Startup connectivity check: when `--brain` is enabled, verifies the LLM endpoint is reachable before creating the engine; prints clear fix instructions if not
- README documentation for the brain feature: setup, activation, config, keybindings, decision learning

## [0.18.1] - 2026-04-15

### Added
- Few-shot decision learning: the brain now retrieves relevant past decisions from the local log and includes them as examples in the LLM prompt, so it learns from user corrections over time
- Configurable `few_shot_count` (default 5) in `[brain]` config section
- Relevance scoring: past decisions matching the same tool name rank highest, then same project, then most recent

## [0.18.0] - 2026-04-15

### Added
- **Local LLM brain** (opt-in): connect to ollama or any OpenAI-compatible local LLM for session advisory. Enable with `--brain` or `[brain]` config section.
- Brain context builder: compacts session transcripts into LLM prompts with configurable token budget
- Brain LLM client: communicates via curl subprocess (no new dependencies, follows webhook pattern)
- Brain inference loop: non-blocking async inference with 10-second per-PID cooldown
- Advisory UI: pending brain suggestions shown inline (`[b:approve]`), accept with `b`, reject with `B`
- Auto mode: `--brain-auto` executes suggestions without confirmation
- Decision logging: every brain suggestion + user response logged to `~/.claudectl/brain/decisions.jsonl`
- Deny rules always override brain suggestions regardless of confidence

## [0.17.1] - 2026-04-15

### Added
- Cross-session data routing: task prompts can reference `{{name.stdout}}` to inject the stdout of a completed dependency, enabling data pipelines between orchestrated sessions
- Template validation at load time catches missing tasks, missing dependencies, and unsupported fields before any task starts
- Output truncation at 32KB with `... (truncated)` marker to prevent context overflow

## [0.17.0] - 2026-04-15

### Added
- Rule-based auto-actions: configure `[rules.*]` sections in `.claudectl.toml` to automatically approve, deny, send messages, or terminate sessions based on tool name, command pattern, project, cost threshold, and error state
- Pending tool tracking: sessions now expose the tool name and command awaiting approval for rule matching and display
- Deny-first precedence: deny rules always override approve rules regardless of config order

## [0.16.2] - 2026-04-14

### Fixed
- Sessions blocked on a permission prompt now correctly show Needs Input when Claude Code writes `stop_reason: null` with a tool_use content block; the monitor infers `tool_use` from the message content instead of requiring the explicit stop_reason field

## [0.16.1] - 2026-04-14

### Fixed
- Sessions blocked on a permission prompt ("Do you want to proceed?") no longer misclassify as Idle after the first refresh tick; status inference now persists JSONL signals across ticks so tool_use-based NeedsInput survives when no new transcript data arrives

## [0.16.0] - 2026-04-14

### Added
- GNOME Terminal support on Linux for `--new` and the `n` launch wizard, with doctor output that makes the current control limitations explicit
- GNOME Terminal launch support for Ubuntu's default terminal, verified under Docker/X11
- Homebrew release automation for both macOS and Linux artifacts, updating `mercurialsolo/homebrew-tap` on tagged releases

### Fixed
- Parent sessions now keep subagent token and cost rollups even when transient task files disappear from `/tmp`
- Session detail and JSON output now distinguish active subagents from total rolled-up subagent usage
- The main dashboard now expands parent sessions into child subagent rows, with a completed-subagent aggregate plus live active subagents underneath
- Release automation now publishes to crates.io instead of stopping at GitHub release assets

## [0.15.5] - 2026-04-14

### Fixed
- Unified Claude transcript parsing across monitoring and highlight reels, so status/cost/context now come from one parser instead of separate ad-hoc readers
- Sessions with missing or unsupported transcript telemetry now show an explicit `Unknown` state with `n/a` metrics instead of looking like idle zero-cost sessions
- `--run` now tracks real child exit status, drains stdout/stderr, writes per-task logs under `.claudectl-runs/`, and fails tasks on non-zero exit instead of treating any vanished PID as success
- `n` and `--new` now launch visible Claude sessions only in supported terminals (`tmux`, Kitty, WezTerm) and fail clearly elsewhere instead of spawning detached background processes
- Cost estimation now uses a model registry with config overrides; unknown models are marked as fallback estimates instead of silently pretending pricing is verified
- `install.sh` now downloads the tagged release assets that the GitHub release workflow actually publishes

### Added
- `[models."..."]` config sections for overriding pricing and context limits per model
- Telemetry metadata in JSON and webhook outputs, including whether estimates are verified or fallback
- Shared transcript fixtures and parser tests for both current and legacy Claude JSONL shapes

## [0.13.1] - 2026-04-13

### Changed
- README updated with all v0.13.0 features in feature list, usage section, and architecture table

## [0.13.0] - 2026-04-13

### Added
- **Session highlight reel** — press `R` on any session to start recording a supercut of its activity. Parses the session's JSONL in real-time, extracts the interesting bits (file edits, bash commands, status transitions), compresses idle time, and outputs as `.gif` or `.cast`. Press `R` again to stop (#66)
- **Multiple simultaneous recordings** — press `R` on different sessions to record them all at once. Each gets its own highlight reel
- **Per-session REC indicator** — table shows `REC` prefix on recorded sessions, status bar shows count
- **Supercut format** — title card, running stats header (edits/commands/errors), paced playback, final summary card with claudectl branding
- Only highlight events make the cut: Edit, Write, Bash, Agent. Read/Grep/Glob filtered out
- Errors marked ✗ red, successes ✓ green, verbose text trimmed
- Works passively in background while TUI stays interactive
- Split terminal support — records via JSONL on disk, not terminal output

## [0.11.2] - 2026-04-13

### Added
- **Direct GIF recording** (`--record session.gif`) — specify `.gif` extension and claudectl automatically records asciicast then converts via `agg`. No manual pipeline needed (#65)
- Falls back gracefully: if `agg` not installed, saves `.cast` with install instructions
- `.cast` extension still supported for raw asciicast v2 output

## [0.11.1] - 2026-04-13

### Added
- **Live session recording** (`--record session.cast`) — captures pixel-perfect ANSI terminal output via a tee writer. Records exact colors, sparklines, and TUI layout as asciicast v2 format
- **Demo mode** (`--demo`) — deterministic fake sessions for when no real sessions are running. 8 sessions with realistic names, statuses, costs, context levels, conflicts, sparklines, tool usage, and file changes. Works with all output modes: `--demo --list`, `--demo --json`

### Fixed
- **Worktree-aware conflict detection** — sessions in different git worktrees of the same repo no longer false-positive as conflicts. Uses `git rev-parse --show-toplevel` to resolve each session's worktree identity, cached per unique cwd

## [0.10.0] - 2026-04-13

### Added
- **Remote compaction trigger** — press `c` to send `/compact` to a running Claude Code session. Only works when session is idle/waiting. Prevents context window from filling up before auto-compaction kicks in (#64)
- **Rate limit exhaustion ETA** — title bar shows `$spent/$budget (ETA: Xh Ym)` based on aggregate burn rate. Color-coded: green (>2h), yellow (<2h), red (<30m) (#57)
- **Conflict detection** — warns when 2+ sessions share the same working directory with `!!` prefix on project name. Desktop notification and `on_conflict_detected` hook (#58)
- **Context threshold hooks** — new `on_context_high` event fires when context window % crosses configurable threshold (default 75%). Resets after `/compact`. New `{context_pct}` template variable (#59)
- **Per-tool token attribution** — detail panel shows tool call counts sorted by frequency (Bash, Read, Edit, etc.). Exposed in `--json` export (#60)
- **Session cleanup command** — `claudectl --clean` with `--older-than`, `--finished`, `--dry-run` flags. Removes dead session JSON + JSONL transcripts, reports freed disk space (#61)
- **File change tracking** — detail panel shows which files each session modified (extracted from Edit/Write tool_use events in JSONL). Exposed in `--json` export (#62)
- **Permission wait time** — status column shows `Needs Input (2m 34s)` with escalating colors (yellow >1m, red >5m). NeedsInput sessions sorted by longest-waiting first (#63)
- `[context] warn_threshold` config option for context alert threshold

## [0.9.1] - 2025-04-12

### Added
- Daily and weekly aggregate cost budget alerts
- `[budget] daily_limit` and `[budget] weekly_limit` config options
- Aggregate budget hooks fire `on_budget_warning` and `on_budget_exceeded` with synthetic sessions

## [0.9.0] - 2025-04-11

### Added
- **Event hooks system** — run shell commands on session events
- 7 hook events: `on_session_start`, `on_status_change`, `on_needs_input`, `on_finished`, `on_budget_warning`, `on_budget_exceeded`, `on_idle`
- Template variables: `{pid}`, `{project}`, `{status}`, `{cost}`, `{model}`, `{cwd}`, `{tokens_in}`, `{tokens_out}`, `{elapsed}`, `{session_id}`, `{old_status}`, `{new_status}`
- Hooks configured in `[hooks.on_*]` sections of config.toml
- `claudectl --hooks` to list configured hooks
- Verified hooks repository at mercurialsolo/claudectl-hooks

## [0.8.3] - 2025-04-10

### Added
- Weekly and daily cost/token summary in TUI title bar

## [0.8.0] - 2025-04-09

### Added
- **Multi-session orchestration** — `claudectl --run tasks.json` with dependency ordering and `--parallel` flag
- **Session history** — persist completed sessions with `--history` and `--stats` commands
- **Configuration files** — `~/.config/claudectl/config.toml` (global) and `.claudectl.toml` (per-project) with layered overrides
- **Theme system** — dark, light, and monochrome themes with `NO_COLOR` support
- **Diagnostic logging** — `--log` flag for structured debug output
- **Install script and Nix flake** for easier distribution
- First-run experience with empty state hints

### Fixed
- Approve/input for Warp terminal using AppleScript with focus management

## [0.7.0] - 2025-04-07

### Added
- **Watch mode** — `claudectl --watch` streams status changes without TUI
- **Debug mode** — timing instrumentation in the footer
- **Activity sparklines** — 30-second history ring buffer per session
- **Grouped view** — press `g` to group sessions by project with aggregate stats
- **Detail panel** — press `Enter` for expanded session info (tokens, cost, model, paths)
- **Session summary** — `claudectl --summary` for what happened while you were away
- **Webhooks** — POST JSON to Slack/Discord/URL on status changes with event filtering
- **Session launcher** — press `n` or `claudectl --new` to start sessions from the TUI
- **Budget enforcement** — `--budget` with 80% warning and optional `--kill-on-budget`
- Custom output format for watch mode
- Linux support (monitoring without terminal switching)
- Stale session cleanup for dead PIDs >24h old

## [0.6.0] - 2025-04-05

### Added
- Context window % column with visual bar
- Burn rate ($/hr) column with cost decay
- Desktop notifications when sessions enter NeedsInput (`--notify`)
- Help overlay (press `?`)
- Sort and filter by status, context, cost, $/hr, elapsed (press `s`)
- JSON export (`--json`) for scripting
- Subagent tracking with +N indicator
- Auto-approve mode (press `a` twice)

### Changed
- Renamed Tokens column to In/Out for clarity

### Fixed
- 5 critical issues: performance, burn rate calc, CPU smoothing, dropped sysinfo dependency, timestamp handling

## [0.5.0] - 2025-04-03

### Added
- Quick approve — press `y` to send Enter to NeedsInput sessions
- Input mode — press `i` to type arbitrary text to sessions
- Kill sessions — press `d`/`x` (double-tap to confirm)
- NeedsInput status detection for permission prompts
- Terminal switching — press `Tab` to jump to a session's terminal

### Fixed
- JSONL session ID mapping (use sessionId before falling back to latest)
- Input sending via terminal emulator instead of raw TTY device
- Status inference: CPU priority over JSONL flags

## [0.4.0] - 2025-04-02

### Added
- Terminal support for **Ghostty**, **Kitty**, **WezTerm**, **tmux**, **Warp**, **iTerm2**, and **Terminal.app**
- Process table enrichment (CPU, MEM, TTY, elapsed) via `ps`
- Session file scanner for `~/.claude/sessions/*.json`
- JSONL tail reader for incremental token accumulation
- Status inference engine (Processing / NeedsInput / WaitingInput / Idle / Finished)
- Cost estimation with model-aware pricing (Opus, Sonnet, Haiku)
- Diff-based UI updates (only re-render changed rows)
- Configurable poll interval

## [0.1.0] - 2025-04-01

### Added
- Initial release
- Basic TUI table showing running Claude Code sessions
- Process discovery via `~/.claude/sessions/` directory
- ratatui-based terminal UI

---

## Feature Overview

### Dashboard & Monitoring
- Live TUI dashboard with PID, project, status, context %, cost, $/hr, elapsed, CPU%, MEM, tokens, sparklines
- Smart status detection: Processing, Needs Input (with wait time), Waiting, Idle, Finished
- Context window % with configurable threshold alerts
- Cost tracking with per-session and aggregate USD estimates
- Burn rate ($/hr) with budget exhaustion ETA projection
- Activity sparklines (30-second history per session)
- Weekly/daily cost summary in title bar

### Session Actions
- `y` — Approve permission prompts (send Enter)
- `i` — Send custom text input to sessions
- `c` — Trigger `/compact` on idle sessions
- `a` — Toggle auto-approve (double-tap)
- `d`/`x` — Kill sessions (double-tap to confirm)
- `n` — Launch new Claude Code sessions
- `Tab` — Switch to session's terminal

### Observability
- Per-tool token attribution (Bash, Read, Edit call counts)
- File change tracking (which files each session modified)
- Conflict detection (2+ sessions sharing same directory)
- Permission wait time tracking with color escalation
- Detail panel with full session breakdown

### Budget & Limits
- Per-session budget with 80% warning and 100% auto-kill
- Daily and weekly aggregate spend limits
- Rate limit exhaustion ETA projection
- Context threshold alerts with `on_context_high` hook

### Event Hooks
- 9 hook events: `on_session_start`, `on_status_change`, `on_needs_input`, `on_finished`, `on_budget_warning`, `on_budget_exceeded`, `on_idle`, `on_context_high`, `on_conflict_detected`
- Template variables for shell command interpolation
- Webhook integration (POST JSON to Slack/Discord/URLs)
- Desktop notifications

### Output Modes
- Interactive TUI (default)
- `--list` — print formatted table and exit
- `--json` — export session data for scripting
- `--watch` — stream status changes without TUI
- `--summary` — session activity summary
- `--history` / `--stats` — historical analytics
- `--clean` — remove old session data

### Configuration
- Global config: `~/.config/claudectl/config.toml`
- Per-project config: `.claudectl.toml`
- CLI flags override config values
- Theme system: dark, light, monochrome, NO_COLOR

### Terminal Support
- Ghostty (native AppleScript)
- Kitty (remote control API)
- tmux (send-keys)
- WezTerm (CLI JSON API)
- Warp (System Events)
- iTerm2 (AppleScript)
- Terminal.app (AppleScript)

### Task Orchestration
- `--run tasks.json` with dependency ordering
- `--parallel` for independent tasks
- Per-task budget and cwd settings
