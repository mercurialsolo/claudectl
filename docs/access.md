# Capability grants — scoped, expiring access for someone who isn't you

Everything else in claudectl assumes one trust level: you, on your machines. A
relay PSK is symmetric — a paired peer reads your sessions, delegates tasks to
you, and can interrupt your work. The coordinator's HTTP bearer token is one
shared secret with no identity, no scope, and no revocation short of restarting
with a different one.

A **grant** is a named, scoped, expiring, individually revocable capability you
hand to one external party. Every verb in the scope grammar is read-only, so a
grant can let someone read and never write. Grants live under
`~/.claudectl/access/` and the whole subsystem is behind the `relay` feature —
the minimal `--no-default-features --features hive` build has no `access`
command.

**There is no query surface yet.** #427 shipped the spine: mint a token, verify
one, list grants, read the audit log, revoke. `claudectl access grant` opens no
port and starts no listener. The read-only query surface a third party would
actually call is #429, and enforcement of the per-grant rate limit and daily
budget is #431. Until then a grant is a credential with nothing to present it
to. The design behind all of this is
[docs/open-cluster.md](open-cluster.md) §3; transport is
[docs/relay.md](relay.md#security).

## Quick start

```bash
claudectl access grant --project claudectl \
  --label "acme integration review" \
  --scopes project.query,project.docs \
  --expires 30d
```

```
Grant gr_a38487 issued for "acme integration review"

TOKEN: cctl_gr_a38487_e8fc30c0125c4aca239f3c988abaa542

This is the only time the token is shown. Store it now.

Scopes:
  project.query:claudectl
  project.docs:claudectl
Expires: in 30d

Revoke any time with: claudectl access revoke gr_a38487
```

`--project` and `--label` are required. `--scopes` defaults to `project.query`
and `--expires` to `30d`. The label is for you — it's the only thing in
`access list` that says who a grant belongs to.

The token is printed once and stored nowhere. It is deterministic — anyone with
the secret and the grant file can recompute it — but no CLI command does, so in
practice losing the token means issuing a new grant and revoking the old one.

## The four commands

```bash
claudectl access grant …              # issue one, print the token once
claudectl access list                 # every grant, its state, uses, last use
claudectl access audit gr_a38487      # what that grant actually asked for
claudectl access audit                # the whole log, every grant
claudectl access revoke gr_a38487     # immediate, nothing to restart
```

All four take `--json`. It's a global flag, so it goes **before** the
subcommand — `claudectl --json access list`. `claudectl access list --json`
fails with `unexpected argument '--json' found`.

```
$ claudectl access list
GRANT       STATE     LABEL                        USES  LAST USED    SCOPES
gr_a38487   active    acme integration review         0  never        project.query:claudectl,project.docs:claudectl
```

`STATE` is `active`, `expired`, or `revoked`; a grant that is both expired and
revoked reads as `revoked`, since revoking was the explicit action. Timestamps
render relative and coarse, rounded to the nearest unit — `3h ago`, `in 30d`,
`never`. Coarse on purpose: the useful questions are whether a grant has gone
stale and when it lapses.

```
$ claudectl access revoke gr_a38487
Revoked gr_a38487 ("acme integration review").
Its token stops verifying immediately — nothing to restart.
```

Revoke is idempotent. Revoking a grant id that was never issued is a plain
error (`no such grant: gr_nope`) — the opacity described below applies to a
third party presenting a token, not to you at your own CLI.

## The grant file

One JSON file per grant at `~/.claudectl/access/grants/<grant_id>.json`:

```json
{
  "grant_id": "gr_a38487",
  "label": "acme integration review",
  "scopes": [
    "project.query:claudectl",
    "project.docs:claudectl"
  ],
  "issued_ms": 1791245775409,
  "expires_ms": 1793837775409,
  "revoked": false,
  "rate_limit_per_min": 20,
  "daily_query_budget": 500,
  "last_used_ms": null,
  "use_count": 0
}
```

Writes are atomic: temp file in the same directory, `sync_data`, rename. A
crash mid-write leaves either the old grant or a dotfile `access list` skips,
never a truncated record. `rate_limit_per_min` and `daily_query_budget` are
persisted but not yet enforced — that's #431.

### Scopes

Scopes are `<resource>.<verb>:<qualifier>`. `--scopes project.query` takes
`--project` as its qualifier. Writing the qualifier out — `--scopes
project.query:claudectl` — is accepted only when it agrees with `--project`;
disagreeing is an error rather than a silent override, since `--project` reads
as the bound on the grant and is the only value validated up front:

```
$ claudectl access grant --project internal-api --label x \
    --scopes project.query:secrets
Error: … "scope 'project.query:secrets' is scoped to 'secrets' but --project is
'internal-api' — drop the qualifier, or pass the project you mean"
```

Duplicates collapse and whitespace is tolerated.

| Scope | Issuable today |
| --- | --- |
| `project.query:<project>` | Yes |
| `project.docs:<project>` | Yes |
| `fleet.read:<project>` | No — open-cluster Q8: defined, issued to nobody |
| `hive.read:<hive>` | No — needs named-hive identity (#424) |
| `hive.join:<hive>` | No — needs named-hive identity (#424) |

Asking for one of the last three is an error that says why:

```
$ claudectl access grant --project claudectl --label x --scopes fleet.read
Error: Custom { kind: Other, error: "cannot issue 'fleet.read:claudectl':
fleet.read is defined but issued to nobody by default (open-cluster RFC Q8) —
a third party reviewing your project should not see your live session costs" }
```

(One line, wrapped here. The `Custom { … }` envelope is Rust's default
`io::Error` rendering, not part of the message.)

All five verbs still *parse*, so a grant file written by a later claudectl
loads on today's binary. There is no write verb in the grammar at all.

Qualifiers are ASCII letters, digits, `-`, `_` and `.`, at most 128 characters,
and may not contain `..`. The grammar is narrow because scopes are signed into
the MAC over a `\n`-joined payload, so a qualifier containing whitespace, a
newline or `:` would make that encoding ambiguous and two different scope sets
could sign identically.

## What the signature covers

The token is `cctl_<grant_id>_<mac>`, where the MAC is HMAC-SHA256 over a
canonical payload of `grant_id`, the sorted scopes, and `expires_ms`,
truncated to 128 bits and hex-encoded. Scopes are sorted first, so typing them
in a different order yields the same token.

Signed: `grant_id`, `scopes`, `expires_ms`.
Not signed: `revoked`, `last_used_ms`, `use_count`.

Operationally that splits grant edits into two kinds:

- **Revoking** flips one unsigned field. It takes effect on the next
  verification, with no restart and no re-issue.
- **Editing `scopes` or `expires_ms`** — by hand in the JSON, or by any future
  tooling — invalidates the token already in the third party's hands. There is
  no "widen this grant" operation. Issue a new grant and revoke the old one.

The same property is why a token can't be edited to widen itself: the scopes it
would have to claim are the ones under the signature.

## The audit log

`~/.claudectl/access/audit.jsonl` is append-only, one JSON object per line, and
records allowed and denied attempts alike:

```json
{"ts_ms":1791072975409,"grant_id":"gr_cd2630","event":"allowed","detail":"how is the brain's decision logging structured?"}
{"ts_ms":1791234975409,"grant_id":"gr_cd2630","event":"denied","reason":"missing_scope","detail":"fleet.read:claudectl"}
{"ts_ms":1791238575409,"grant_id":"gr_cd2630","event":"denied","reason":"bad_mac"}
```

`reason` is absent on an allowed entry. `detail` is free-form and absent when
there's nothing to say. `claudectl access audit <grant_id>` renders the lines
for one grant, oldest first; `claudectl access audit` with no id renders the
whole log. The no-id form is the only way to see denials recorded against a
token too malformed to name a grant — those are filed under a sentinel id that
is not a valid grant id, which is precisely the probing the log exists to
surface:

```
GRANT        WHEN           EVENT    REASON           DETAIL
gr_cd2630    2d ago         allowed  -                how is the brain's decision logging structured?
gr_cd2630    3h ago         denied   missing_scope    fleet.read:claudectl
gr_cd2630    2h ago         denied   bad_mac
```

The seven deny reasons are `malformed_token`, `unknown_grant`,
`unreadable_grant`, `bad_mac`, `revoked`, `expired` and `missing_scope`. A
malformed token has no recoverable grant id, so its entry is filed under
`<unparseable>`.

**The audit log is the only place a denial is explained.** A caller whose token
fails verification gets one undifferentiated `denied` with no detail: unknown
grant, bad MAC, revoked, expired and missing scope are indistinguishable from
outside. That's the open-cluster RFC's 404-not-403 rule applied one layer
down — distinguishing "no such grant" from "revoked" would tell a prober which
grant ids exist. Because denied attempts are audited too, someone hammering an
invalid token is visible to you even though it's invisible to them.

Verification is where those entries come from, and nothing calls it yet. Until
#429 lands there is no code path a third party can reach, so in practice
`audit.jsonl` doesn't exist until the first attempt — `access audit` on a live
grant prints `No audit entries for gr_a38487 — issued but never used.` and
distinguishes that from `No such grant: gr_nope`.

## The secret

`~/.claudectl/access/secret` is the 32-byte HMAC key every grant token derives
from. It is created on first `access grant`, written `0600`, and never leaves
the machine.

Two properties of how it is created:

- It comes from a generator that **fails rather than guessing**. The LAN
  pairing-code path (`relay::crypto::generate_psk`) falls back to a
  timestamp/pid/thread-id hash when `/dev/urandom` cannot be read; the access
  secret uses `try_generate_psk`, which errors instead, because a predictable
  key here would let someone recompute tokens. If the RNG is unavailable,
  `access grant` fails.
- The `0600` mode is part of the `open(2)` call, so the file never exists at a
  wider mode and the rename publishes something nobody else could have read.
  Setting it with a `chmod` after `File::create` would not be enough: that
  leaves a window at `0666 & ~umask`, and a `chmod` cannot revoke a descriptor
  another process already opened during it. (The older
  `relay::save_peer_psk` does write first and chmod after.)
- `grants/*.json` and `audit.jsonl` get the same treatment, and the audit log's
  mode is re-asserted on every append so a log left wider by an earlier version
  is repaired rather than staying that way.

**Back the secret up together with `grants/`.** The two are only meaningful as
a pair:

- **Lose the secret and every issued token dies silently.** On the next
  `access grant` a fresh secret is minted, and tokens signed under the old one
  stop verifying. `access list` never reads the secret, so those grants still
  show as `active` — the list is not evidence the tokens still work. Re-issue
  and re-distribute.
- **Leak the secret and every live token becomes reproducible.** It does not
  let anyone invent a grant or widen one: `verify` recomputes the MAC from the
  stored grant's own `scopes` and `expires_ms`, so a forged token for a
  non-existent grant is `unknown_grant` and one claiming wider scopes is
  `bad_mac`. What a leak gives up is the ability to recompute the legitimate
  token for any grant whose signed fields you can read — and those fields are
  in the grant file. Tokens so recomputed are still bounded by `revoked` and
  `expires_ms`. There is nothing to rotate short of deleting the secret and
  re-issuing every grant.

File modes: `secret`, `grants/*.json` and `audit.jsonl` are all `0600`, each set
on the temp file or at creation rather than chmodded afterwards. The grant files
are owner-only because they carry every field the MAC covers — left at a default
umask, `secret` would be the only thing between an unprivileged local user and
every token on the machine. The containing directory is left at the default
mode, so tighten `~/.claudectl/access/` itself if other people have shell
accounts here.

A secret that exists but is corrupt or the wrong length is an error, not a
silent re-mint:

```
access secret at ~/.claudectl/access/secret is 16 bytes, expected 32 — move it
aside to mint a new one (every existing token stops verifying)
```

That error is deliberate. A silent re-mint would invalidate every live grant
without telling you.
