//! Grant records and the on-disk store (#427, RFC §3.2, §4.8).
//!
//! One JSON file per grant under `<access_dir>/grants/<grant_id>.json`, plus an
//! append-only `audit.jsonl`. Every entry point takes the access dir explicitly
//! so the whole module is unit-testable against a `tempdir` with no network and
//! no `$HOME` dependency.
//!
//! Three contracts this store keeps:
//!
//! 1. **Writes are atomic.** Temp file in the same directory, `sync_data`, then
//!    rename. A crash mid-write leaves either the old grant or a dotfile the
//!    enumerator skips, never a truncated grant.
//! 2. **Reads of a grant are strict, not fail-open.** A malformed grant file is
//!    an error, because treating an unreadable capability as "absent" and an
//!    absent one as "denied" would silently un-grant someone. Enumeration for
//!    `access list` is the one place that skips bad files, so a single corrupt
//!    record cannot hide every other grant.
//! 3. **Authority is frozen, accounting is mutable.** See `token.rs`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::scope::Scope;
use super::{AccessError, DenyReason};

/// Default per-grant rate limit (RFC §4.8; the bus uses 60/min per role).
const DEFAULT_RATE_LIMIT_PER_MIN: u32 = 20;

/// Default per-grant daily query budget (RFC §4.8).
const DEFAULT_DAILY_QUERY_BUDGET: u32 = 500;

fn default_rate_limit() -> u32 {
    DEFAULT_RATE_LIMIT_PER_MIN
}

fn default_daily_budget() -> u32 {
    DEFAULT_DAILY_QUERY_BUDGET
}

/// A named, scoped, expiring capability issued to one external party.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Grant {
    pub grant_id: String,
    pub label: String,
    /// Signed into the MAC. Editing this invalidates the issued token.
    pub scopes: Vec<Scope>,
    pub issued_ms: u64,
    /// Signed into the MAC. Editing this invalidates the issued token.
    pub expires_ms: u64,
    /// Not signed — revoking takes effect immediately with nothing to restart.
    #[serde(default)]
    pub revoked: bool,
    #[serde(default = "default_rate_limit")]
    pub rate_limit_per_min: u32,
    #[serde(default = "default_daily_budget")]
    pub daily_query_budget: u32,
    #[serde(default)]
    pub last_used_ms: Option<u64>,
    #[serde(default)]
    pub use_count: u64,
    /// The UTC day the `budget_used` counter belongs to, as
    /// `now_ms / 86_400_000`.
    ///
    /// Stored rather than derived so the rollover is a comparison instead of a
    /// scan: when the current day differs, the counter is stale and resets.
    /// Unsigned, like `revoked` — editing it changes accounting, not
    /// capability, so it must not invalidate a live token.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub budget_day: u64,
    /// Queries charged against `daily_query_budget` on `budget_day`.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub budget_used: u32,
}

impl Grant {
    pub fn is_expired_at(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_ms
    }

    /// Whether this grant carries a scope matching `wanted` exactly.
    pub fn has_scope(&self, wanted: &Scope) -> bool {
        self.scopes.contains(wanted)
    }
}

/// One line of `audit.jsonl`.
///
/// Denied attempts are recorded too — that is what makes "a hammering grant is
/// visible" true before the query surface in #429 exists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEntry {
    pub ts_ms: u64,
    /// The grant the caller claimed. Recorded even when it does not exist, so
    /// probing for valid ids shows up.
    pub grant_id: String,
    /// `"allowed"` or `"denied"`.
    pub event: String,
    /// Machine-readable deny reason; `None` when allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// What was asked — a scope, a query, an endpoint. Free-form by design so
    /// #429 and #431 can append richer detail without a schema change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The question text, truncated to [`MAX_AUDIT_QUESTION_BYTES`].
    ///
    /// #431's load-bearing field. Thresholds and caps stop the abuse you
    /// anticipated; this is how the owner sees the abuse nobody anticipated —
    /// it is the only record of what a grant actually *asked*, as opposed to
    /// how often.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// The paths cited in the answer, so the log says what came back and not
    /// only what went in. Empty on a denial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cited: Option<Vec<String>>,
}

/// Why a daily-budget charge did not go through.
///
/// Typed rather than a `String`, because the two cases must produce different
/// answers and string-comparing them was a fail-open: "budget exhausted" is a
/// refusal the caller earned, while "the store could not be read or written"
/// means the surface cannot meter at all. Returning the latter as a `500`
/// would hand out *unmetered* requests to anyone able to induce a transient
/// store failure, so `query::core` maps it to the same opaque denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChargeError {
    /// The day's allowance is spent. Audited as `budget_exhausted`.
    Exhausted,
    /// The grant could not be read or the charge could not be persisted.
    Store(String),
}

impl std::fmt::Display for ChargeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChargeError::Exhausted => write!(f, "daily query budget exhausted"),
            ChargeError::Store(e) => write!(f, "{e}"),
        }
    }
}

/// Whether a counter is still at zero, so a freshly minted grant serializes
/// without accounting noise and `docs/access.md`'s verbatim grant stays true.
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// Cap on the question text kept in one audit line.
///
/// The question is attacker-supplied and the log is append-only, so it needs a
/// bound. 512 bytes keeps a real question whole while making the log's growth
/// a function of request count rather than of request size.
pub const MAX_AUDIT_QUESTION_BYTES: usize = 512;

/// Truncate `text` to at most `MAX_AUDIT_QUESTION_BYTES`, on a char boundary.
///
/// Byte-slicing attacker-supplied UTF-8 is how `parse_duration` panicked in
/// #427 review; `char_indices` is the fix that generalises.
pub fn truncate_for_audit(text: &str) -> String {
    if text.len() <= MAX_AUDIT_QUESTION_BYTES {
        return text.to_string();
    }
    // Walk back to the nearest char boundary. `is_char_boundary` rather than
    // arithmetic on `char_indices`, because slicing mid-codepoint panics and a
    // question is attacker-supplied text.
    let mut cut = MAX_AUDIT_QUESTION_BYTES;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

/// Grants plus the audit log, rooted at an explicit directory.
#[derive(Debug, Clone)]
pub struct GrantStore {
    root: PathBuf,
}

impl GrantStore {
    /// Open a store rooted at an explicit access dir. Nothing is created yet.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Open the real store at `~/.claudectl/access`.
    ///
    /// Fails rather than falling back to `/tmp` when `HOME` is unset — see
    /// `access_dir`.
    pub fn open_default() -> Result<Self, String> {
        Ok(Self::new(super::access_dir()?))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn grants_dir(&self) -> PathBuf {
        self.root.join("grants")
    }

    pub fn audit_path(&self) -> PathBuf {
        self.root.join("audit.jsonl")
    }

    fn grant_path(&self, grant_id: &str) -> Option<PathBuf> {
        if super::is_valid_grant_id(grant_id) {
            Some(self.grants_dir().join(format!("{grant_id}.json")))
        } else {
            None
        }
    }

    /// The revocation tombstone's path.
    ///
    /// `.revoked` rather than `.json`, so [`Self::list`]'s extension filter
    /// skips it and a tombstone never reads as a grant.
    fn revoked_path(&self, grant_id: &str) -> Option<PathBuf> {
        if super::is_valid_grant_id(grant_id) {
            Some(self.grants_dir().join(format!("{grant_id}.revoked")))
        } else {
            None
        }
    }

    /// Whether a revocation tombstone exists for `grant_id`.
    ///
    /// # Why revocation is a file and not just a field
    ///
    /// Three writers rewrite the *whole* grant record: `charge_daily_budget`,
    /// `record_query_use` and `revoke` itself. They are serialised inside one
    /// `query serve` process, but `access revoke` runs in a **different
    /// process**, and a process-local mutex cannot order writes across a
    /// process boundary. So this interleaving was possible:
    ///
    /// ```text
    ///   serve: load grant (revoked: false)
    ///   cli:   load, set revoked = true, write
    ///   serve: bump use_count, write the whole record  -> revoked: false
    /// ```
    ///
    /// A token the owner believes is dead keeps answering. #431 called a
    /// cross-process clobber "the benign direction", which was true of the
    /// budget counters and false of this.
    ///
    /// Revocation is **monotonic** — once revoked, always revoked; there is no
    /// un-revoke command — and a monotonic fact does not belong in a mutable
    /// record. A file that is only ever *created* cannot be clobbered by a
    /// writer that never writes it, so the property holds by construction
    /// rather than by locking. `revoke` still sets the JSON field too, so the
    /// on-disk shape `docs/access.md` documents is unchanged for anything that
    /// reads it; the tombstone is simply the authority when the two disagree.
    fn is_tombstoned(&self, grant_id: &str) -> bool {
        self.revoked_path(grant_id).is_some_and(|p| p.exists())
    }

    /// Persist a grant, refusing to clobber an existing id.
    ///
    /// A grant-id collision is an error rather than a silent overwrite that
    /// would strand the previous holder's token. This checks `exists()` and
    /// then atomically renames, rather than using `create_new` on the final
    /// path, because the atomic-write guarantee matters more than exclusive
    /// creation: two concurrent `access grant` calls that draw the same 24-bit
    /// id could both pass the check, and the loser is a lost grant rather than
    /// a corrupt one.
    pub fn create(&self, grant: &Grant) -> Result<(), String> {
        let path = self
            .grant_path(&grant.grant_id)
            .ok_or_else(|| format!("invalid grant id: {}", grant.grant_id))?;
        let dir = self.grants_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("create grants dir: {e}"))?;

        if path.exists() {
            return Err(format!("grant {} already exists", grant.grant_id));
        }
        self.write_atomic(&path, grant)
    }

    /// Overwrite an existing grant (revoke, or a use-count bump).
    pub fn update(&self, grant: &Grant) -> Result<(), String> {
        let path = self
            .grant_path(&grant.grant_id)
            .ok_or_else(|| format!("invalid grant id: {}", grant.grant_id))?;
        fs::create_dir_all(self.grants_dir()).map_err(|e| format!("create grants dir: {e}"))?;
        self.write_atomic(&path, grant)
    }

    fn write_atomic(&self, path: &Path, grant: &Grant) -> Result<(), String> {
        let dir = path
            .parent()
            .ok_or_else(|| "grant path has no parent".to_string())?;
        // Dotfile sibling with a .tmp extension: the enumerator filters on
        // `.json`, so a crash leftover is invisible to `access list`.
        let tmp = dir.join(format!(".{}.json.tmp", grant.grant_id));
        let body = serde_json::to_vec_pretty(grant).map_err(|e| format!("encode grant: {e}"))?;
        {
            // Owner-only from the open(2) call, not chmodded afterwards. A
            // grant file holds every field the MAC covers, so anyone who can
            // read both it and the secret can recompute the live token.
            let mut f = create_private(&tmp)?;
            f.write_all(&body)
                .map_err(|e| format!("write temp grant: {e}"))?;
            f.sync_data().map_err(|e| format!("sync grant: {e}"))?;
        }
        fs::rename(&tmp, path).map_err(|e| format!("publish grant: {e}"))
    }

    /// Load one grant. `Ok(None)` means absent; `Err` means present-but-broken.
    pub fn load(&self, grant_id: &str) -> Result<Option<Grant>, String> {
        let Some(path) = self.grant_path(grant_id) else {
            // A malformed id can't name a file, so it is simply absent rather
            // than an error — callers map that to the same opaque denial.
            return Ok(None);
        };
        let body = match fs::read_to_string(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("read grant {grant_id}: {e}")),
        };
        let mut grant: Grant = serde_json::from_str(&body)
            .map_err(|e| format!("grant {grant_id} is malformed: {e}"))?;
        // Folded in here rather than at each call site, so `verify`,
        // `verify_detailed`, `charge_daily_budget` and `access list` are all
        // correct without knowing the tombstone exists.
        if self.is_tombstoned(grant_id) {
            grant.revoked = true;
        }
        Ok(Some(grant))
    }

    /// Every readable grant, newest first. Unreadable files are skipped so one
    /// corrupt record cannot hide the rest.
    pub fn list(&self) -> Vec<Grant> {
        let Ok(entries) = fs::read_dir(self.grants_dir()) else {
            return Vec::new();
        };
        // Parsed, then the tombstone applied — `list` reads the file directly
        // rather than going through `load`, so it has to do the same fold or
        // `access list` would print `active` for a grant `verify` refuses.
        let mut out: Vec<Grant> = entries
            .flatten()
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .filter_map(|body| serde_json::from_str::<Grant>(&body).ok())
            .map(|mut g| {
                if self.is_tombstoned(&g.grant_id) {
                    g.revoked = true;
                }
                g
            })
            .collect();
        out.sort_by_key(|g| std::cmp::Reverse(g.issued_ms));
        out
    }

    /// Mark a grant revoked. Idempotent.
    ///
    /// The tombstone is written **first**, and that order is the fail-closed
    /// one: if this dies between the two writes, a tombstone with a stale JSON
    /// field still reads as revoked, while a written field with no tombstone
    /// could be clobbered back to live by a concurrent `query serve`. See
    /// [`Self::is_tombstoned`].
    pub fn revoke(&self, grant_id: &str) -> Result<Grant, String> {
        let mut grant = self
            .load(grant_id)?
            .ok_or_else(|| format!("no such grant: {grant_id}"))?;
        self.tombstone(grant_id)?;
        grant.revoked = true;
        // Best-effort: the tombstone above is what makes the revocation
        // durable, so a failure to also update the JSON must not report a
        // revocation that did not happen.
        let _ = self.update(&grant);
        Ok(grant)
    }

    /// Create the revocation tombstone. Idempotent.
    fn tombstone(&self, grant_id: &str) -> Result<(), String> {
        let path = self
            .revoked_path(grant_id)
            .ok_or_else(|| format!("invalid grant id: {grant_id}"))?;
        fs::create_dir_all(self.grants_dir()).map_err(|e| format!("create grants dir: {e}"))?;
        // Created, never rewritten, and the content is irrelevant — existence
        // is the whole signal. `create_private` truncates, which is harmless
        // on an empty marker and keeps the 0600 discipline.
        let f = create_private(&path)?;
        f.sync_data()
            .map_err(|e| format!("sync revocation tombstone: {e}"))
    }

    /// Verify a presented token against the stored grant and a required scope.
    ///
    /// Order matters and differs from RFC §3.2 as written: the MAC covers
    /// `scopes` and `expires_ms`, which live only in the grant file, so the
    /// file must be loaded before the MAC can be recomputed.
    ///
    /// This never mutates the grant: accounting is [`Self::record_use`]'s job,
    /// so checking a capability cannot inflate its use count. It does append a
    /// line to `audit.jsonl` on a denial, which is the point of the log.
    ///
    /// Every failure returns the same opaque [`AccessError::Denied`]. The
    /// specific [`DenyReason`] goes to the audit log and never to the caller —
    /// distinguishing "no such grant" from "revoked" would leak which grant
    /// ids exist, which is the 404-not-403 rule from RFC §3.3 applied one
    /// layer down.
    // The opaque form, and the one a third-party-facing caller should reach
    // for: `AccessError` carries no detail, so a response built from it cannot
    // leak which grants exist. #431 moved the one production caller
    // (`query::core`) to `verify_detailed`, which it needs to tell a
    // `MissingScope` denial apart from the rest for budget accounting. Kept
    // because the safe default is worth keeping available — and this module's
    // own tests exercise the verification contract through it.
    #[allow(dead_code)]
    pub fn verify(
        &self,
        secret: &[u8; 32],
        token: &str,
        required: Option<&Scope>,
        now_ms: u64,
    ) -> Result<Grant, AccessError> {
        self.verify_detailed(secret, token, required, now_ms)
            .map_err(|_| AccessError::Denied)
    }

    /// [`Self::verify`], but telling the caller *why* it failed and which
    /// grant id was claimed.
    ///
    /// This exists for one caller and one reason. #431 charges a denied query
    /// against the grant's daily budget so that probing is self-limiting — but
    /// only for `MissingScope`, the single denial that proves the caller holds
    /// a *valid* token and is probing other verbs or projects. Charging
    /// `BadMac` or `UnknownGrant` would let anyone who guesses a 24-bit grant
    /// id exhaust the legitimate holder's budget, turning a probing defence
    /// into a denial-of-service against the person being protected.
    ///
    /// The reason stays internal. [`Self::verify`] is the surface every
    /// third-party-facing caller should use, and both map to the same opaque
    /// `404` (RFC §3.3) — this returns a `DenyReason` so the *owner's* own
    /// accounting can branch on it, not so a response can.
    pub(crate) fn verify_detailed(
        &self,
        secret: &[u8; 32],
        token: &str,
        required: Option<&Scope>,
        now_ms: u64,
    ) -> Result<Grant, (DenyReason, Option<String>)> {
        let parsed = match super::token::parse(token) {
            Ok(p) => p,
            Err(_) => {
                self.audit_denied("<unparseable>", DenyReason::MalformedToken, None, now_ms);
                return Err((DenyReason::MalformedToken, None));
            }
        };

        let grant = match self.load(&parsed.grant_id) {
            Ok(Some(g)) => g,
            Ok(None) => {
                self.audit_denied(&parsed.grant_id, DenyReason::UnknownGrant, None, now_ms);
                return Err((DenyReason::UnknownGrant, None));
            }
            Err(_) => {
                self.audit_denied(&parsed.grant_id, DenyReason::UnreadableGrant, None, now_ms);
                return Err((DenyReason::UnreadableGrant, None));
            }
        };

        // Defence in depth. The MAC already covers `grant_id`, so a token
        // naming the grant by a different spelling fails as `bad_mac` on a
        // case-insensitive filesystem rather than verifying — but that safety
        // is a property of `canonical_payload`, not of this function. Checking
        // the identity here means a future change to the signed payload cannot
        // quietly turn an alias into a bypass.
        if grant.grant_id != parsed.grant_id {
            self.audit_denied(&parsed.grant_id, DenyReason::UnknownGrant, None, now_ms);
            return Err((DenyReason::UnknownGrant, None));
        }

        let expected =
            super::token::compute_mac(secret, &grant.grant_id, &grant.scopes, grant.expires_ms);
        if !super::token::mac_matches(&parsed.mac, &expected) {
            self.audit_denied(&parsed.grant_id, DenyReason::BadMac, None, now_ms);
            return Err((DenyReason::BadMac, None));
        }

        if grant.revoked {
            self.audit_denied(&parsed.grant_id, DenyReason::Revoked, None, now_ms);
            return Err((DenyReason::Revoked, Some(grant.grant_id)));
        }

        if grant.is_expired_at(now_ms) {
            self.audit_denied(&parsed.grant_id, DenyReason::Expired, None, now_ms);
            return Err((DenyReason::Expired, Some(grant.grant_id)));
        }

        if let Some(want) = required
            && !grant.has_scope(want)
        {
            self.audit_denied(
                &parsed.grant_id,
                DenyReason::MissingScope,
                Some(want.to_string()),
                now_ms,
            );
            return Err((DenyReason::MissingScope, Some(grant.grant_id)));
        }

        Ok(grant)
    }

    /// Charge one query against `grant_id`'s daily budget.
    ///
    /// Returns `Ok(remaining)` when the charge went through, or
    /// `Err(DenyReason::BudgetExhausted)` when the day's allowance is already
    /// spent. Rolls the counter over when `now_ms` falls in a later UTC day
    /// than the one recorded, so no background job is needed to reset it.
    ///
    /// Load-check-increment-write is not atomic against a concurrent caller.
    /// `QueryCore` serialises it behind a process-local mutex, which is
    /// sufficient because one server process serves one project; a second
    /// process sharing the same access dir could lose a charge, and the
    /// failure mode is undercounting rather than over-serving a grant past
    /// its expiry or scope. Making it atomic would mean file locking the
    /// grant, and a cap on query volume is not worth that.
    pub fn charge_daily_budget(
        &self,
        grant_id: &str,
        question: Option<&str>,
        now_ms: u64,
    ) -> Result<u32, ChargeError> {
        let mut grant = match self.load(grant_id) {
            Ok(Some(g)) => g,
            Ok(None) => {
                self.audit_denied_outside_verify(
                    grant_id,
                    DenyReason::UnknownGrant,
                    Some("vanished between verify and charge".into()),
                    question,
                    now_ms,
                );
                return Err(ChargeError::Store(format!("no such grant: {grant_id}")));
            }
            Err(e) => {
                self.audit_denied_outside_verify(
                    grant_id,
                    DenyReason::UnreadableGrant,
                    Some("could not be read to charge the budget".into()),
                    question,
                    now_ms,
                );
                return Err(ChargeError::Store(e));
            }
        };

        let today = super::utc_day(now_ms);
        // `>` rather than `!=`, so the counter only ever rolls *forward*. An
        // NTP correction backwards across a day boundary would make `today`
        // less than `budget_day`, and `!=` read that as a new day and handed
        // the grant a second full allowance for a day it had already spent.
        // Keeping the old day's counter is the conservative direction, and a
        // fresh grant (`budget_day: 0`) still rolls forward on first use.
        if today > grant.budget_day {
            grant.budget_day = today;
            grant.budget_used = 0;
        }
        // `max(1)`, matching `rate_limit::try_acquire_with_capacity`. These
        // fields have no CLI flag and `docs/access.md` tells owners to edit the
        // grant file, so a hand-written `0` is far more likely to mean "I did
        // not think about this" than "deny every query" — and two guardrails
        // reading `0` in opposite directions would be worse than either
        // reading alone. Read-time only: the file is not rewritten.
        let budget = grant.daily_query_budget.max(1);
        if grant.budget_used >= budget {
            self.audit_denied_outside_verify(
                grant_id,
                DenyReason::BudgetExhausted,
                Some(format!("{}/{} used today", grant.budget_used, budget)),
                question,
                now_ms,
            );
            return Err(ChargeError::Exhausted);
        }
        grant.budget_used = grant.budget_used.saturating_add(1);
        let remaining = budget.saturating_sub(grant.budget_used);
        if let Err(e) = self.update(&grant) {
            self.audit_denied_outside_verify(
                grant_id,
                DenyReason::UnreadableGrant,
                Some("the charge could not be persisted".into()),
                question,
                now_ms,
            );
            return Err(ChargeError::Store(e));
        }
        Ok(remaining)
    }

    /// Record a successful use: bump accounting and append an audit line.
    ///
    /// Deliberately separate from [`Self::verify`], which never touches the
    /// grant. A caller that only needs to check a capability should not bump
    /// its counters.
    #[allow(dead_code)] // The question-free form. `query::core` uses `record_query_use`.
    pub fn record_use(
        &self,
        grant_id: &str,
        detail: Option<&str>,
        now_ms: u64,
    ) -> Result<(), String> {
        self.record_query_use(grant_id, detail, None, None, now_ms)
    }

    /// [`Self::record_use`] with the question and the paths that answered it.
    ///
    /// #431 §"Why audit is the load-bearing one": caps stop anticipated abuse,
    /// the log is how unanticipated abuse becomes visible — and a log of
    /// counts alone cannot show what a grant was actually fishing for. The
    /// question is truncated by [`truncate_for_audit`]; `cited` is the span
    /// paths, so a reader can see what left the machine without replaying the
    /// query against a since-changed index.
    pub fn record_query_use(
        &self,
        grant_id: &str,
        detail: Option<&str>,
        question: Option<&str>,
        cited: Option<Vec<String>>,
        now_ms: u64,
    ) -> Result<(), String> {
        // Refuse rather than log a use of nothing. Callers reach here only
        // after `verify` returned a grant, so an absent record means the store
        // changed underneath them and the audit line would be a lie.
        let mut grant = self
            .load(grant_id)?
            .ok_or_else(|| format!("no such grant: {grant_id}"))?;
        grant.last_used_ms = Some(now_ms);
        grant.use_count = grant.use_count.saturating_add(1);
        self.update(&grant)?;
        self.append_audit(&AuditEntry {
            ts_ms: now_ms,
            grant_id: grant_id.to_string(),
            event: "allowed".into(),
            reason: None,
            detail: detail.map(str::to_string),
            question: question.map(truncate_for_audit),
            cited,
        })
    }

    /// Append a `denied` line for a refusal decided outside [`Self::verify`] —
    /// the rate limit and the daily budget, which are checked only after a
    /// token has verified.
    ///
    /// `pub(crate)` for the same reason as [`Self::verify_detailed`]: it is a
    /// hook for `query::core`, not part of the access surface.
    pub(crate) fn audit_denied_outside_verify(
        &self,
        grant_id: &str,
        reason: DenyReason,
        detail: Option<String>,
        question: Option<&str>,
        now_ms: u64,
    ) {
        let _ = self.append_audit(&AuditEntry {
            ts_ms: now_ms,
            grant_id: grant_id.to_string(),
            event: "denied".into(),
            reason: Some(reason.as_str().to_string()),
            detail,
            question: question.map(truncate_for_audit),
            cited: None,
        });
    }

    fn audit_denied(
        &self,
        grant_id: &str,
        reason: DenyReason,
        detail: Option<String>,
        now_ms: u64,
    ) {
        // Best-effort: a failing audit write must not turn a denial into an
        // error the caller could distinguish from any other denial.
        let _ = self.append_audit(&AuditEntry {
            ts_ms: now_ms,
            grant_id: grant_id.to_string(),
            event: "denied".into(),
            reason: Some(reason.as_str().to_string()),
            detail,
            question: None,
            cited: None,
        });
    }

    pub fn append_audit(&self, entry: &AuditEntry) -> Result<(), String> {
        fs::create_dir_all(&self.root).map_err(|e| format!("create access dir: {e}"))?;
        let line = serde_json::to_string(entry).map_err(|e| format!("encode audit entry: {e}"))?;
        let path = self.audit_path();
        let mut opts = fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Owner-only from the first line. #429 appends query text here, so
            // this log only gets more sensitive.
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&path)
            .map_err(|e| format!("open audit log: {e}"))?;
        // `mode` applies only when the file is created, so repair anything a
        // previous version (or an interrupted run) left wider. Unconditional
        // rather than gated on a `path.exists()` check taken before the open:
        // that check could never notice a log already sitting at 0644, so it
        // would stay 0644 forever.
        set_owner_only(&path)?;
        writeln!(f, "{line}").map_err(|e| format!("write audit entry: {e}"))
    }

    /// Audit entries, oldest first. `grant_id` filters to one grant.
    pub fn read_audit(&self, grant_id: Option<&str>) -> Vec<AuditEntry> {
        let Ok(body) = fs::read_to_string(self.audit_path()) else {
            return Vec::new();
        };
        body.lines()
            .filter_map(|line| serde_json::from_str::<AuditEntry>(line).ok())
            .filter(|e| grant_id.is_none_or(|want| e.grant_id == want))
            .collect()
    }
}

/// Restrict a file to its owner. No-op off unix.
fn set_owner_only(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Create a file owner-only *at creation*, so there is no window in which it
/// exists with a wider mode.
///
/// `File::create` opens `0o666 & ~umask` — 0644 on a default umask — and a
/// later `chmod` does not revoke descriptors another process already holds.
/// Setting the mode in the `open(2)` call closes that race.
fn create_private(path: &Path) -> Result<fs::File, String> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))
}

/// Build a new grant with defaults filled in. Does not persist.
pub fn new_grant(
    grant_id: String,
    label: String,
    scopes: Vec<Scope>,
    issued_ms: u64,
    expires_ms: u64,
) -> Grant {
    Grant {
        grant_id,
        label,
        scopes,
        issued_ms,
        expires_ms,
        revoked: false,
        rate_limit_per_min: DEFAULT_RATE_LIMIT_PER_MIN,
        daily_query_budget: DEFAULT_DAILY_QUERY_BUDGET,
        last_used_ms: None,
        use_count: 0,
        budget_day: 0,
        budget_used: 0,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; 32] = [3u8; 32];
    const NOW: u64 = 1_791_210_482_180;
    const HOUR: u64 = 3_600_000;

    fn scope(s: &str) -> Scope {
        s.parse().unwrap()
    }

    /// A store in a tempdir plus a live grant and its token.
    fn fixture() -> (tempfile::TempDir, GrantStore, Grant, String) {
        let dir = tempfile::tempdir().unwrap();
        let store = GrantStore::new(dir.path());
        let grant = new_grant(
            "gr_7f2a1b".into(),
            "acme integration review".into(),
            vec![scope("project.query:claudectl")],
            NOW,
            NOW + HOUR,
        );
        store.create(&grant).unwrap();
        let token =
            super::super::token::mint(&SECRET, &grant.grant_id, &grant.scopes, grant.expires_ms);
        (dir, store, grant, token)
    }

    // ── The acceptance path: issue → verify → use → expire → revoke ──

    #[test]
    fn a_fresh_token_verifies() {
        let (_d, store, grant, token) = fixture();
        let out = store
            .verify(
                &SECRET,
                &token,
                Some(&scope("project.query:claudectl")),
                NOW,
            )
            .expect("fresh token should verify");
        assert_eq!(out.grant_id, grant.grant_id);
    }

    #[test]
    fn verify_is_a_pure_read() {
        let (_d, store, _g, token) = fixture();
        store.verify(&SECRET, &token, None, NOW).unwrap();
        let after = store.load("gr_7f2a1b").unwrap().unwrap();
        assert_eq!(after.use_count, 0, "verify must not bump accounting");
        assert_eq!(after.last_used_ms, None);
    }

    #[test]
    fn record_use_bumps_accounting_and_audits() {
        let (_d, store, _g, _t) = fixture();
        store
            .record_use("gr_7f2a1b", Some("asked about auth"), NOW)
            .unwrap();
        store.record_use("gr_7f2a1b", None, NOW + 1).unwrap();

        let g = store.load("gr_7f2a1b").unwrap().unwrap();
        assert_eq!(g.use_count, 2);
        assert_eq!(g.last_used_ms, Some(NOW + 1));

        let audit = store.read_audit(Some("gr_7f2a1b"));
        assert_eq!(audit.len(), 2);
        assert_eq!(audit[0].event, "allowed");
        assert_eq!(audit[0].detail.as_deref(), Some("asked about auth"));
    }

    #[test]
    fn an_expired_token_is_denied() {
        let (_d, store, _g, token) = fixture();
        assert!(store.verify(&SECRET, &token, None, NOW + HOUR).is_err());
        assert!(store.verify(&SECRET, &token, None, NOW + HOUR * 2).is_err());
        // Still good a millisecond before expiry.
        assert!(store.verify(&SECRET, &token, None, NOW + HOUR - 1).is_ok());
    }

    #[test]
    fn a_revoked_token_is_denied_immediately() {
        let (_d, store, _g, token) = fixture();
        assert!(store.verify(&SECRET, &token, None, NOW).is_ok());
        store.revoke("gr_7f2a1b").unwrap();
        assert!(
            store.verify(&SECRET, &token, None, NOW).is_err(),
            "revocation must take effect with nothing restarted"
        );
    }

    #[test]
    fn revoke_is_idempotent_and_errors_on_a_missing_grant() {
        let (_d, store, _g, _t) = fixture();
        store.revoke("gr_7f2a1b").unwrap();
        store.revoke("gr_7f2a1b").unwrap();
        assert!(store.revoke("gr_nope1").is_err());
    }

    // ── The property the whole design rests on ──

    #[test]
    fn widening_a_grants_scopes_on_disk_invalidates_its_token() {
        let (_d, store, mut grant, token) = fixture();
        assert!(store.verify(&SECRET, &token, None, NOW).is_ok());

        // The attacker's move: edit the grant file to grant more than was
        // issued. The scopes are signed, so the token stops verifying.
        grant.scopes.push(scope("project.docs:claudectl"));
        store.update(&grant).unwrap();

        assert!(
            store.verify(&SECRET, &token, None, NOW).is_err(),
            "a widened grant must invalidate the issued token"
        );
    }

    #[test]
    fn pushing_out_expiry_on_disk_also_invalidates_the_token() {
        let (_d, store, mut grant, token) = fixture();
        grant.expires_ms = NOW + HOUR * 100;
        store.update(&grant).unwrap();
        assert!(store.verify(&SECRET, &token, None, NOW).is_err());
    }

    #[test]
    fn a_revocation_cannot_be_undone_by_editing_the_grant_file() {
        // This test used to assert the opposite — that writing `revoked:
        // false` back restored the token — on the reasoning that an unsigned
        // field is a cheap two-way switch. That reasoning was the hole: three
        // writers rewrite the whole record and one of them lives in another
        // process, so "whatever was written last wins" meant a concurrent
        // `query serve` could un-revoke a grant by accident. Nothing in
        // `docs/access.md` ever offered un-revoke; it says revocation is
        // immediate and idempotent, and that widening means issuing a new
        // grant. Revocation is monotonic now, and the tombstone is what makes
        // it so.
        let (_d, store, _g, token) = fixture();
        store.revoke("gr_7f2a1b").unwrap();
        assert!(store.verify(&SECRET, &token, None, NOW).is_err());

        let mut g = store.load("gr_7f2a1b").unwrap().unwrap();
        g.revoked = false;
        store.update(&g).unwrap();

        // The field is back to `false` on disk and the MAC still matches — it
        // was never signed — but the tombstone outranks the field.
        assert!(
            store.verify(&SECRET, &token, None, NOW).is_err(),
            "editing the field must not resurrect a revoked grant"
        );
        assert!(
            store.load("gr_7f2a1b").unwrap().unwrap().revoked,
            "`load` folds the tombstone in, so every consumer sees revoked"
        );
    }

    #[test]
    fn a_concurrent_write_cannot_clobber_a_revocation() {
        // The two-process shape, as two stores over one directory: `serve`
        // loads the grant, the CLI revokes it, `serve` writes its stale copy
        // back. Before the tombstone this silently un-revoked the grant.
        let dir = tempfile::tempdir().unwrap();
        let serve = GrantStore::new(dir.path());
        let cli = GrantStore::new(dir.path());
        let grant = new_grant(
            "gr_7f2a1b".into(),
            "t".into(),
            vec!["project.query:p".parse().unwrap()],
            NOW,
            NOW + HOUR,
        );
        serve.create(&grant).unwrap();
        let secret = [7u8; 32];
        let token =
            super::super::token::mint(&secret, &grant.grant_id, &grant.scopes, grant.expires_ms);

        // `serve` has the record in hand, pre-revocation.
        let stale = serve.load("gr_7f2a1b").unwrap().unwrap();
        assert!(!stale.revoked);

        cli.revoke("gr_7f2a1b").unwrap();

        // …and writes it back, exactly as a `use_count` bump would.
        serve.update(&stale).unwrap();

        assert!(
            serve.verify(&secret, &token, None, NOW).is_err(),
            "a stale write must not resurrect the grant"
        );
        // And `access list` agrees with `verify`, which is the other half:
        // `list` reads the files directly rather than through `load`.
        let listed = serve.list();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].revoked, "list must fold the tombstone in too");
    }

    #[test]
    fn use_counts_do_not_invalidate_the_mac() {
        let (_d, store, _g, token) = fixture();
        store.record_use("gr_7f2a1b", None, NOW).unwrap();
        assert!(store.verify(&SECRET, &token, None, NOW).is_ok());
    }

    // ── Denials ──

    #[test]
    fn a_grant_id_alias_does_not_verify() {
        // On a case-insensitive filesystem (APFS by default) `gr_7F2A1B.json`
        // resolves to `gr_7f2a1b.json`. If verify recomputes the MAC from the
        // *file's* id rather than checking it against the presented one, an
        // aliased token verifies — and every audit line then lands under the
        // alias, so `access audit gr_7f2a1b` shows nothing while the holder
        // hammers under a different spelling.
        let (_d, store, grant, _t) = fixture();
        let alias = grant.grant_id.to_uppercase();
        assert_ne!(alias, grant.grant_id);

        let aliased = super::super::token::mint(&SECRET, &alias, &grant.scopes, grant.expires_ms);
        assert!(
            store.verify(&SECRET, &aliased, None, NOW).is_err(),
            "a token naming the grant by a different spelling must not verify"
        );
    }

    #[test]
    fn a_token_from_a_different_secret_is_denied() {
        let (_d, store, grant, _t) = fixture();
        let forged =
            super::super::token::mint(&[9u8; 32], &grant.grant_id, &grant.scopes, grant.expires_ms);
        assert!(store.verify(&SECRET, &forged, None, NOW).is_err());
    }

    #[test]
    fn a_token_for_an_unknown_grant_is_denied() {
        let (_d, store, _g, _t) = fixture();
        let token = super::super::token::mint(
            &SECRET,
            "gr_abcdef",
            &[scope("project.query:p")],
            NOW + HOUR,
        );
        assert!(store.verify(&SECRET, &token, None, NOW).is_err());
    }

    #[test]
    fn a_malformed_token_is_denied_without_panicking() {
        let (_d, store, _g, _t) = fixture();
        for bad in ["", "garbage", "cctl_", "cctl_gr_1_short", "cctl_../x_aaaa"] {
            assert!(store.verify(&SECRET, bad, None, NOW).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_missing_scope_is_denied_even_with_a_valid_token() {
        let (_d, store, _g, token) = fixture();
        assert!(store.verify(&SECRET, &token, None, NOW).is_ok());
        assert!(
            store
                .verify(&SECRET, &token, Some(&scope("project.docs:claudectl")), NOW)
                .is_err(),
            "a grant must not satisfy a scope it was not issued"
        );
        // Right verb, wrong project.
        assert!(
            store
                .verify(&SECRET, &token, Some(&scope("project.query:other")), NOW)
                .is_err()
        );
    }

    #[test]
    fn every_denial_records_its_reason_in_the_audit_log() {
        let (_d, store, _g, token) = fixture();
        store.revoke("gr_7f2a1b").unwrap();
        let _ = store.verify(&SECRET, &token, None, NOW);
        let _ = store.verify(&SECRET, "garbage", None, NOW);

        let audit = store.read_audit(None);
        let reasons: Vec<&str> = audit.iter().filter_map(|e| e.reason.as_deref()).collect();
        assert!(reasons.contains(&"revoked"), "got {reasons:?}");
        assert!(reasons.contains(&"malformed_token"), "got {reasons:?}");
        assert!(audit.iter().all(|e| e.event == "denied"));
    }

    #[test]
    fn probing_for_unknown_grants_is_visible_in_the_audit_log() {
        let dir = tempfile::tempdir().unwrap();
        let store = GrantStore::new(dir.path());
        for id in ["gr_aaaaaa", "gr_bbbbbb", "gr_cccccc"] {
            let t = super::super::token::mint(&SECRET, id, &[scope("project.query:p")], NOW + HOUR);
            let _ = store.verify(&SECRET, &t, None, NOW);
        }
        let audit = store.read_audit(None);
        assert_eq!(audit.len(), 3);
        assert!(
            audit
                .iter()
                .all(|e| e.reason.as_deref() == Some("unknown_grant"))
        );
    }

    // ── Store mechanics ──

    #[test]
    fn create_refuses_to_clobber_an_existing_grant() {
        let (_d, store, grant, _t) = fixture();
        assert!(
            store.create(&grant).is_err(),
            "a grant-id collision must not strand the previous holder's token"
        );
    }

    #[test]
    fn load_distinguishes_absent_from_malformed() {
        let (_d, store, _g, _t) = fixture();
        assert!(
            store.load("gr_nope1").unwrap().is_none(),
            "absent → Ok(None)"
        );

        fs::write(store.grants_dir().join("gr_bad000.json"), "{ not json").unwrap();
        assert!(
            store.load("gr_bad000").is_err(),
            "present-but-broken must not read as absent"
        );
    }

    #[test]
    fn list_skips_corrupt_files_rather_than_hiding_everything() {
        let (_d, store, _g, _t) = fixture();
        fs::write(store.grants_dir().join("gr_bad000.json"), "{ not json").unwrap();
        fs::write(store.grants_dir().join("notes.txt"), "ignored").unwrap();
        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].grant_id, "gr_7f2a1b");
    }

    #[test]
    fn list_is_newest_first_and_empty_without_a_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = GrantStore::new(dir.path().join("nothing-here"));
        assert!(store.list().is_empty());

        let store = GrantStore::new(dir.path());
        for (id, issued) in [("gr_old111", NOW), ("gr_new111", NOW + 5000)] {
            store
                .create(&new_grant(
                    id.into(),
                    "l".into(),
                    vec![scope("project.query:p")],
                    issued,
                    issued + HOUR,
                ))
                .unwrap();
        }
        let ids: Vec<String> = store.list().into_iter().map(|g| g.grant_id).collect();
        assert_eq!(ids, vec!["gr_new111", "gr_old111"]);
    }

    #[test]
    fn writes_leave_no_temp_files_behind() {
        let (_d, store, grant, _t) = fixture();
        store.update(&grant).unwrap();
        let leftovers: Vec<_> = fs::read_dir(store.grants_dir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "found {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn grant_files_and_the_audit_log_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, store, _g, _t) = fixture();
        store.record_use("gr_7f2a1b", Some("x"), NOW).unwrap();

        let mode = |p: std::path::PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        // A grant file carries every field the MAC covers, so default-umask
        // 0644 would leave the secret as the only barrier.
        assert_eq!(mode(store.grants_dir().join("gr_7f2a1b.json")), 0o600);
        assert_eq!(mode(store.audit_path()), 0o600);
    }

    #[test]
    fn a_path_traversing_grant_id_cannot_escape_the_grants_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = GrantStore::new(dir.path());
        let g = new_grant(
            "../../escaped".into(),
            "l".into(),
            vec![scope("project.query:p")],
            NOW,
            NOW + HOUR,
        );
        assert!(store.create(&g).is_err());
        assert!(!dir.path().parent().unwrap().join("escaped.json").exists());
    }

    #[test]
    fn audit_filters_by_grant_and_survives_a_torn_line() {
        let (_d, store, _g, _t) = fixture();
        store
            .create(&new_grant(
                "gr_other1".into(),
                "second".into(),
                vec![scope("project.query:p")],
                NOW,
                NOW + HOUR,
            ))
            .unwrap();
        store.record_use("gr_7f2a1b", Some("one"), NOW).unwrap();
        store.record_use("gr_other1", Some("two"), NOW).unwrap();
        // A line truncated by a crash must not discard the readable ones.
        fs::OpenOptions::new()
            .append(true)
            .open(store.audit_path())
            .unwrap()
            .write_all(b"{\"ts_ms\":1,\"grant\n")
            .unwrap();

        assert_eq!(store.read_audit(Some("gr_7f2a1b")).len(), 1);
        assert_eq!(store.read_audit(Some("gr_other1")).len(), 1);
        assert_eq!(store.read_audit(None).len(), 2);
    }

    #[test]
    fn record_use_refuses_a_grant_that_does_not_exist() {
        // Logging an "allowed" line for a grant with no record would put a
        // lie in the audit log.
        let (_d, store, _g, _t) = fixture();
        assert!(store.record_use("gr_nope11", None, NOW).is_err());
        assert!(store.read_audit(Some("gr_nope11")).is_empty());
    }

    #[test]
    fn denial_timestamps_match_the_decision_they_record() {
        // audit_denied used to stamp wall-clock time while verify took an
        // explicit now_ms, so the log disagreed with the decision.
        let (_d, store, _g, token) = fixture();
        let at = NOW + 1234;
        let _ = store.verify(&SECRET, &token, Some(&scope("project.docs:claudectl")), at);
        let audit = store.read_audit(Some("gr_7f2a1b"));
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].ts_ms, at);
        assert_eq!(audit[0].reason.as_deref(), Some("missing_scope"));
        assert_eq!(audit[0].detail.as_deref(), Some("project.docs:claudectl"));
    }

    #[test]
    fn a_short_question_is_kept_whole() {
        assert_eq!(
            truncate_for_audit("how is auth structured?"),
            "how is auth structured?"
        );
        let exact = "x".repeat(MAX_AUDIT_QUESTION_BYTES);
        assert_eq!(truncate_for_audit(&exact), exact, "the cap is inclusive");
    }

    #[test]
    fn a_long_question_is_capped_and_marked() {
        let long = "x".repeat(MAX_AUDIT_QUESTION_BYTES + 100);
        let got = truncate_for_audit(&long);
        assert!(got.ends_with('…'));
        assert_eq!(
            got.chars().filter(|c| *c == 'x').count(),
            MAX_AUDIT_QUESTION_BYTES
        );
    }

    /// The question is attacker-supplied, so the cut must never land
    /// mid-codepoint. This is the same class of bug as `parse_duration`'s
    /// `split_at(len-1)` panic found in #427 review.
    #[test]
    fn truncating_multibyte_text_never_panics_or_splits_a_codepoint() {
        // 3 bytes each, so the 512-byte cap lands inside a character.
        let padded = "あ".repeat(MAX_AUDIT_QUESTION_BYTES);
        let got = truncate_for_audit(&padded);
        assert!(got.len() <= MAX_AUDIT_QUESTION_BYTES + "…".len());
        assert!(got.ends_with('…'));
        // Round-trips as valid UTF-8 through serde, which is where it goes.
        let encoded = serde_json::to_string(&got).unwrap();
        let back: String = serde_json::from_str(&encoded).unwrap();
        assert_eq!(back, got);

        // A 4-byte codepoint at the boundary too.
        let emoji = "🔒".repeat(MAX_AUDIT_QUESTION_BYTES);
        assert!(truncate_for_audit(&emoji).ends_with('…'));
    }

    #[test]
    fn audit_lines_serialize_the_shape_the_docs_promise() {
        // docs/access.md documents audit.jsonl verbatim, so pin the field
        // order and the omit-when-absent behaviour.
        let allowed = AuditEntry {
            ts_ms: 1_791_072_975_409,
            grant_id: "gr_cd2630".into(),
            event: "allowed".into(),
            reason: None,
            detail: Some("query.ask".into()),
            question: Some("how is auth structured?".into()),
            cited: Some(vec!["docs/auth.md".into()]),
        };
        assert_eq!(
            serde_json::to_string(&allowed).unwrap(),
            r#"{"ts_ms":1791072975409,"grant_id":"gr_cd2630","event":"allowed","detail":"query.ask","question":"how is auth structured?","cited":["docs/auth.md"]}"#
        );

        let denied = AuditEntry {
            ts_ms: 1_791_238_575_409,
            grant_id: "gr_cd2630".into(),
            event: "denied".into(),
            reason: Some(DenyReason::BadMac.as_str().into()),
            detail: None,
            question: None,
            cited: None,
        };
        assert_eq!(
            serde_json::to_string(&denied).unwrap(),
            r#"{"ts_ms":1791238575409,"grant_id":"gr_cd2630","event":"denied","reason":"bad_mac"}"#
        );
    }

    #[test]
    fn audit_is_empty_before_anything_happens() {
        let dir = tempfile::tempdir().unwrap();
        assert!(GrantStore::new(dir.path()).read_audit(None).is_empty());
    }

    // ── Serde / forward-compat ──

    #[test]
    fn grant_json_matches_the_rfc_shape() {
        let (_d, store, _g, _t) = fixture();
        let body = fs::read_to_string(store.grants_dir().join("gr_7f2a1b.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["grant_id"], "gr_7f2a1b");
        assert_eq!(v["scopes"][0], "project.query:claudectl");
        assert_eq!(v["revoked"], false);
        assert_eq!(v["rate_limit_per_min"], 20);
        assert_eq!(v["daily_query_budget"], 500);
        assert_eq!(v["use_count"], 0);
    }

    #[test]
    fn a_minimal_grant_file_loads_with_defaults() {
        // Fields added later must not make older grant files unreadable.
        let dir = tempfile::tempdir().unwrap();
        let store = GrantStore::new(dir.path());
        fs::create_dir_all(store.grants_dir()).unwrap();
        fs::write(
            store.grants_dir().join("gr_min000.json"),
            r#"{"grant_id":"gr_min000","label":"l",
                "scopes":["project.query:p"],"issued_ms":1,"expires_ms":2}"#,
        )
        .unwrap();

        let g = store.load("gr_min000").unwrap().unwrap();
        assert!(!g.revoked);
        assert_eq!(g.rate_limit_per_min, 20);
        assert_eq!(g.daily_query_budget, 500);
        assert_eq!(g.use_count, 0);
        assert_eq!(g.last_used_ms, None);
    }

    #[test]
    fn has_scope_is_exact_not_prefix_matching() {
        let g = new_grant(
            "gr_1aaaaa".into(),
            "l".into(),
            vec![scope("project.query:claudectl")],
            NOW,
            NOW + HOUR,
        );
        assert!(g.has_scope(&scope("project.query:claudectl")));
        assert!(!g.has_scope(&scope("project.query:claudectl-fork")));
        assert!(!g.has_scope(&scope("project.docs:claudectl")));
    }

    // ── Review follow-ups on #431 ──

    #[test]
    fn a_clock_step_backwards_does_not_hand_out_a_second_allowance() {
        let (_d, store, mut grant, _t) = fixture();
        grant.daily_query_budget = 2;
        store.update(&grant).unwrap();

        let day2 = super::super::MS_PER_DAY * 2 + 1_000;
        store.charge_daily_budget("gr_7f2a1b", None, day2).unwrap();
        store.charge_daily_budget("gr_7f2a1b", None, day2).unwrap();
        assert_eq!(
            store.charge_daily_budget("gr_7f2a1b", None, day2),
            Err(ChargeError::Exhausted)
        );

        // An NTP correction backwards across the day boundary. With `!=` this
        // read as "a new day" and reset the counter, handing the grant a
        // second full allowance for time it had already spent.
        let day1 = super::super::MS_PER_DAY + 1_000;
        assert_eq!(
            store.charge_daily_budget("gr_7f2a1b", None, day1),
            Err(ChargeError::Exhausted),
            "going back in time must not refill the budget"
        );

        // Forward still rolls, which is the direction that has to work.
        let day3 = super::super::MS_PER_DAY * 3 + 1_000;
        assert_eq!(store.charge_daily_budget("gr_7f2a1b", None, day3), Ok(1));
    }

    #[test]
    fn a_zero_daily_budget_reads_as_one_rather_than_a_dead_grant() {
        // `rate_limit_per_min: 0` is deliberately raised to 1, on the grounds
        // that a 0 in a hand-edited grant file is far likelier to be an
        // oversight than a deliberate denial. These fields have no CLI flag
        // and `docs/access.md` tells owners to edit the file, so the budget
        // follows the same convention — two guardrails reading 0 in opposite
        // directions would be worse than either reading alone.
        let (_d, store, mut grant, _t) = fixture();
        grant.daily_query_budget = 0;
        store.update(&grant).unwrap();

        assert_eq!(store.charge_daily_budget("gr_7f2a1b", None, NOW), Ok(0));
        assert_eq!(
            store.charge_daily_budget("gr_7f2a1b", None, NOW),
            Err(ChargeError::Exhausted)
        );
        // Read-time only: the file keeps the 0 the owner wrote.
        assert_eq!(
            store.load("gr_7f2a1b").unwrap().unwrap().daily_query_budget,
            0
        );
    }
}
