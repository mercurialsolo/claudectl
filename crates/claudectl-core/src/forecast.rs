//! Cost burn-rate smoothing + budget-ETA forecasting (#370).
//!
//! Agent token spend is bursty — one large tool result or context dump spikes
//! cost for a single tick — so an *instantaneous* burn rate (one tick's cost
//! delta) flickers, and any ETA derived from it jumps around and triggers false
//! "about to blow budget" alarms. Two fixes, both here and both pure:
//!
//! 1. Smooth the rate with an EWMA (exponentially-weighted moving average) at a
//!    fixed half-life, so the rate tracks real regime changes without lurching.
//! 2. Because the burn distribution is heavy-tailed, report the budget ETA as an
//!    interval (from the p90/p10 of recent samples) rather than a single point.
//!
//! All functions are deterministic and take their inputs explicitly so the
//! forecasting math is unit-testable without a clock or a live session.

/// How many recent instantaneous burn samples to retain per session for the
/// percentile band.
pub const BURN_SAMPLE_CAP: usize = 30;

/// EWMA half-life in wall-clock seconds. A sample's influence halves every
/// ~45s, which smooths tick-to-tick noise while still reacting within a minute.
pub const BURN_HALF_LIFE_SECS: f64 = 45.0;

/// EWMA weight on the newest sample for a given half-life. `half_life` and
/// `interval` share a unit (seconds). Larger interval (or shorter half-life)
/// ⇒ more weight on the new sample.
pub fn alpha_for_half_life(half_life_secs: f64, interval_secs: f64) -> f64 {
    if half_life_secs <= 0.0 {
        return 1.0;
    }
    1.0 - 0.5_f64.powf(interval_secs / half_life_secs)
}

/// One EWMA step. `prev == None` seeds the average with the sample.
pub fn ewma(prev: Option<f64>, sample: f64, alpha: f64) -> f64 {
    match prev {
        Some(p) => alpha * sample + (1.0 - alpha) * p,
        None => sample,
    }
}

/// Linear-interpolated percentile of an unsorted slice. `p` in `0.0..=1.0`.
/// Returns `None` for an empty slice.
pub fn percentile(samples: &[f64], p: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut v: Vec<f64> = samples.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p = p.clamp(0.0, 1.0);
    let idx = p * (v.len() as f64 - 1.0);
    let lo = idx.floor() as usize;
    let hi = idx.ceil() as usize;
    let frac = idx - lo as f64;
    Some(v[lo] + (v[hi] - v[lo]) * frac)
}

/// Hours until `headroom_usd` is exhausted at `burn_per_hr`. `None` when the
/// burn is effectively zero (never runs out); `Some(0.0)` when headroom is
/// already gone.
pub fn eta_hours(headroom_usd: f64, burn_per_hr: f64) -> Option<f64> {
    if burn_per_hr <= 1e-6 {
        return None;
    }
    if headroom_usd <= 0.0 {
        return Some(0.0);
    }
    Some(headroom_usd / burn_per_hr)
}

/// Budget-ETA interval. `low` is the soonest exhaustion (computed from the fast
/// p90 burn), `high` the latest (slow p10 burn), `mid` from the smoothed rate.
/// Auto-actions should gate on `low` (the conservative bound), not `mid`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EtaBand {
    pub low_hours: Option<f64>,
    pub mid_hours: Option<f64>,
    pub high_hours: Option<f64>,
}

/// Forecast time-to-budget from current headroom, the smoothed burn, and the
/// recent sample window. Falls back to the smoothed rate when there aren't
/// enough samples for meaningful percentiles.
pub fn budget_eta_band(headroom_usd: f64, smoothed_burn: f64, samples: &[f64]) -> EtaBand {
    let p90 = percentile(samples, 0.9).unwrap_or(smoothed_burn);
    let p10 = percentile(samples, 0.1).unwrap_or(smoothed_burn);
    EtaBand {
        low_hours: eta_hours(headroom_usd, p90),
        mid_hours: eta_hours(headroom_usd, smoothed_burn),
        high_hours: eta_hours(headroom_usd, p10),
    }
}

/// Human-readable ETA: `"~12m"`, `"~3h40m"`, `">24h"`, `"now"`, or `"—"`.
pub fn format_eta(hours: Option<f64>) -> String {
    match hours {
        None => "—".into(),
        Some(h) if h <= 0.0 => "now".into(),
        Some(h) if h >= 24.0 => ">24h".into(),
        Some(h) => {
            let total_min = (h * 60.0).round() as i64;
            let hh = total_min / 60;
            let mm = total_min % 60;
            if hh > 0 {
                format!("~{hh}h{mm:02}m")
            } else {
                format!("~{mm}m")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ewma_seeds_then_smooths() {
        let a = 0.5;
        let seeded = ewma(None, 10.0, a);
        assert_eq!(seeded, 10.0);
        // One step toward a smaller sample moves halfway at alpha=0.5.
        assert_eq!(ewma(Some(10.0), 0.0, a), 5.0);
    }

    #[test]
    fn ewma_resists_a_single_spike() {
        // A small alpha (long half-life) barely reacts to one big spike.
        let alpha = alpha_for_half_life(45.0, 2.0);
        let after = ewma(Some(1.0), 100.0, alpha);
        assert!(after < 6.0, "one spike moved EWMA too far: {after}");
    }

    #[test]
    fn alpha_in_unit_range_and_monotonic() {
        let a = alpha_for_half_life(45.0, 2.0);
        assert!(a > 0.0 && a < 1.0);
        // Shorter half-life ⇒ more weight on the new sample.
        assert!(alpha_for_half_life(10.0, 2.0) > a);
    }

    #[test]
    fn percentile_interpolates() {
        let s = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&s, 0.0), Some(1.0));
        assert_eq!(percentile(&s, 1.0), Some(4.0));
        assert_eq!(percentile(&s, 0.5), Some(2.5));
        assert_eq!(percentile(&[], 0.5), None);
    }

    #[test]
    fn eta_hours_handles_zero_burn_and_spent_budget() {
        assert_eq!(eta_hours(5.0, 0.0), None);
        assert_eq!(eta_hours(0.0, 2.0), Some(0.0));
        assert_eq!(eta_hours(10.0, 5.0), Some(2.0));
    }

    #[test]
    fn band_orders_soonest_to_latest() {
        // headroom $10; samples spread 1..10/hr, smoothed 4/hr.
        let samples: Vec<f64> = (1..=10).map(|n| n as f64).collect();
        let b = budget_eta_band(10.0, 4.0, &samples);
        // fast burn (p90) exhausts sooner than slow burn (p10).
        assert!(b.low_hours.unwrap() <= b.mid_hours.unwrap());
        assert!(b.mid_hours.unwrap() <= b.high_hours.unwrap());
    }

    #[test]
    fn format_eta_renders_buckets() {
        assert_eq!(format_eta(None), "—");
        assert_eq!(format_eta(Some(0.0)), "now");
        assert_eq!(format_eta(Some(0.2)), "~12m");
        assert_eq!(format_eta(Some(3.0 + 40.0 / 60.0)), "~3h40m");
        assert_eq!(format_eta(Some(50.0)), ">24h");
    }
}
