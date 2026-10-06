//! The monthly Jev spend ledger — #431's deferred §4.8 row.
//!
//! §4.8 lists a monthly spend ceiling alongside the per-grant rate limit and
//! daily budget, and #431 shipped the other two and said plainly that this one
//! had nothing to meter until there was an outbound call. Now there is.
//!
//! One file, `~/.claudectl/access/jev-spend.json`, holding one month:
//!
//! ```json
//! { "month": "2026-10", "usd": 0.0041, "calls": 97 }
//! ```
//!
//! Rollover is a string comparison on read, the same shape as the daily
//! budget's `budget_day`: when the stored month is not the current one the
//! record is stale and reads as zero. No background job, and nothing to run on
//! the first of the month.
//!
//! # The lock is inside, not outside
//!
//! Charging is a read-modify-write on a file, so it needs serialising for the
//! same reason `charge_daily_budget` does. The mutex lives in [`SpendLedger`]
//! rather than in `QueryCore` so a caller cannot forget it — and crucially so
//! it is *not* `QueryCore::budget_lock`, which must never be held across a
//! 70–500ms network call.
//!
//! Cross-process, two servers sharing one access dir can undercount. That is
//! the benign direction and the same accepted limitation #431 documented.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::access::{MS_PER_DAY, create_private};

use super::jev::USD_PER_INPUT_TOKEN;

/// Default ceiling, in US dollars per calendar month.
///
/// At `$0.042`/M input tokens and roughly 600 tokens per classification, five
/// dollars is on the order of 200,000 classified queries — far past anything a
/// per-grant daily budget would let through, which is the point. This is a
/// runaway-billing backstop, not a usage budget.
pub const DEFAULT_MONTHLY_USD: f64 = 5.00;

/// One month's accumulated spend.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Spend {
    /// `YYYY-MM`, UTC — the same clock the daily budget rolls on.
    #[serde(default)]
    pub month: String,
    #[serde(default)]
    pub usd: f64,
    #[serde(default)]
    pub calls: u64,
}

/// The UTC calendar month a timestamp falls in, as `YYYY-MM`.
pub fn utc_month(now_ms: u64) -> String {
    let (year, month, _) = claudectl_core::logger::days_to_date(now_ms / MS_PER_DAY);
    format!("{year:04}-{month:02}")
}

/// The spend file, with its own serialisation.
pub struct SpendLedger {
    path: PathBuf,
    lock: Mutex<()>,
}

impl SpendLedger {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        SpendLedger {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    /// The ledger beside the grants it bills for.
    pub fn in_access_dir(root: &Path) -> Self {
        Self::new(root.join("jev-spend.json"))
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// This month's spend. A missing, unreadable or stale-month file reads as
    /// zero — an unreadable ledger must not become an infinite ceiling, which
    /// is why [`Self::charge`] reports its write failures instead.
    pub fn read(&self, now_ms: u64) -> Spend {
        let month = utc_month(now_ms);
        let stored: Spend = fs::read_to_string(&self.path)
            .ok()
            .and_then(|b| serde_json::from_str(&b).ok())
            .unwrap_or_default();
        if stored.month == month {
            stored
        } else {
            Spend {
                month,
                usd: 0.0,
                calls: 0,
            }
        }
    }

    /// Whether this month has already reached `ceiling_usd`. Read-only: it
    /// never writes and never charges.
    ///
    /// Checked *before* the call, because an outbound request cannot be
    /// un-sent. Between this check and the charge, concurrent in-flight
    /// requests can all pass — at `$0.042`/M and ~600-token requests the
    /// overshoot is a few hundredths of a cent per concurrent request, so this
    /// is documented rather than engineered around. Same benign direction as
    /// #431's cross-process undercount.
    pub fn exceeded(&self, ceiling_usd: f64, now_ms: u64) -> bool {
        self.read(now_ms).usd >= ceiling_usd
    }

    /// Add one call's input tokens to this month's total.
    ///
    /// Charged *after* the response, from `usage.input_tokens`, because that is
    /// the only authoritative count — the request's token length is an estimate
    /// and the ceiling should bound real billing.
    pub fn charge(&self, input_tokens: u64, now_ms: u64) -> Result<Spend, String> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut spend = self.read(now_ms);
        spend.usd += input_tokens as f64 * USD_PER_INPUT_TOKEN;
        spend.calls += 1;
        self.write_atomic(&spend)?;
        Ok(spend)
    }

    /// Temp dotfile sibling → `sync_data` → rename, owner-only from `open(2)`.
    /// The same discipline `GrantStore::write_atomic` uses, for the same
    /// reason: a half-written ledger would read as zero and uncap the month.
    fn write_atomic(&self, spend: &Spend) -> Result<(), String> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| "spend path has no parent".to_string())?;
        fs::create_dir_all(dir).map_err(|e| format!("create access dir: {e}"))?;
        let tmp = dir.join(".jev-spend.json.tmp");
        let body = serde_json::to_vec_pretty(spend).map_err(|e| format!("encode spend: {e}"))?;
        {
            let mut f = create_private(&tmp)?;
            f.write_all(&body)
                .map_err(|e| format!("write temp spend: {e}"))?;
            f.sync_data().map_err(|e| format!("sync spend: {e}"))?;
        }
        fs::rename(&tmp, &self.path).map_err(|e| format!("publish spend: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "claudectl-spend-{tag}-{}-{}",
            std::process::id(),
            crate::access::epoch_ms()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    const OCT_2026: u64 = 1_791_200_000_000; // 2026-10-05
    const NOV_2026: u64 = 1_793_500_000_000; // 2026-11-01

    #[test]
    fn the_month_key_is_utc_year_and_month() {
        assert_eq!(utc_month(0), "1970-01");
        assert_eq!(utc_month(OCT_2026), "2026-10");
        assert_eq!(utc_month(NOV_2026), "2026-11");
    }

    #[test]
    fn an_absent_ledger_reads_as_this_month_at_zero() {
        let dir = tmpdir("absent");
        let l = SpendLedger::in_access_dir(&dir);
        let s = l.read(OCT_2026);
        assert_eq!(s.month, "2026-10");
        assert_eq!(s.usd, 0.0);
        assert_eq!(s.calls, 0);
        assert!(!l.path().exists(), "reading must not create the file");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn charges_accumulate_within_a_month() {
        let dir = tmpdir("accumulate");
        let l = SpendLedger::in_access_dir(&dir);
        l.charge(1_000_000, OCT_2026).unwrap();
        let s = l.charge(1_000_000, OCT_2026).unwrap();
        // Two million input tokens at $0.042/M.
        assert!((s.usd - 0.084).abs() < 1e-9, "{}", s.usd);
        assert_eq!(s.calls, 2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_new_month_starts_from_zero_without_a_background_job() {
        let dir = tmpdir("rollover");
        let l = SpendLedger::in_access_dir(&dir);
        l.charge(10_000_000, OCT_2026).unwrap();
        assert!(l.read(OCT_2026).usd > 0.0);
        // Same file, next month: stale by comparison, not by cleanup.
        let next = l.read(NOV_2026);
        assert_eq!(next.month, "2026-11");
        assert_eq!(next.usd, 0.0);
        assert!(!l.exceeded(0.001, NOV_2026));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_ceiling_is_reached_at_the_ceiling_not_past_it() {
        let dir = tmpdir("ceiling");
        let l = SpendLedger::in_access_dir(&dir);
        l.charge(1_000_000, OCT_2026).unwrap();
        let spent = l.read(OCT_2026).usd;
        assert!((spent - 0.042).abs() < 1e-9, "{spent}");
        // Compared against the recorded value rather than a literal, so the
        // `>=` is what is under test and not the last bit of a float.
        assert!(
            l.exceeded(spent, OCT_2026),
            "standing exactly on the ceiling counts as reached"
        );
        assert!(!l.exceeded(spent + 0.001, OCT_2026));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checking_the_ceiling_never_writes() {
        let dir = tmpdir("readonly");
        let l = SpendLedger::in_access_dir(&dir);
        l.charge(1_000, OCT_2026).unwrap();
        let before = fs::read_to_string(l.path()).unwrap();
        for _ in 0..5 {
            let _ = l.exceeded(0.0, OCT_2026);
            let _ = l.read(OCT_2026);
        }
        assert_eq!(fs::read_to_string(l.path()).unwrap(), before);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_ledger_reads_as_zero_rather_than_unbounded() {
        let dir = tmpdir("corrupt");
        let l = SpendLedger::in_access_dir(&dir);
        fs::write(l.path(), "{not json").unwrap();
        // Zero, so the ceiling still bites once charges resume — the failure
        // mode is "bills a little more", not "never checks again".
        assert_eq!(l.read(OCT_2026).usd, 0.0);
        let s = l.charge(1_000_000, OCT_2026).unwrap();
        assert!((s.usd - 0.042).abs() < 1e-9);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_ledger_file_is_owner_only() {
        let dir = tmpdir("mode");
        let l = SpendLedger::in_access_dir(&dir);
        l.charge(1, OCT_2026).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(l.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{mode:o}");
        }
        fs::remove_dir_all(&dir).ok();
    }
}
