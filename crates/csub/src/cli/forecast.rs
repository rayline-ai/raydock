use rayline_subscriptions::LimitClaim;
use time::OffsetDateTime;

use super::parse_reset_timestamp;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum DepletionForecast {
    Exhausted,
    NoBurn,
    ResetFirst,
    RunsOutAt(OffsetDateTime),
    Learning,
    Stale,
    Unavailable,
}

pub(super) fn depletion_forecast(
    claim: Option<&LimitClaim>,
    fresh: bool,
    window_seconds: i64,
    now: OffsetDateTime,
) -> DepletionForecast {
    let Some(claim) = claim else {
        return DepletionForecast::Unavailable;
    };
    if !fresh {
        return DepletionForecast::Stale;
    }
    if claim.is_hard_exhausted() {
        return DepletionForecast::Exhausted;
    }
    let Some(utilization) = claim.utilization.map(|value| value.clamp(0.0, 1.0)) else {
        return DepletionForecast::Unavailable;
    };
    if utilization <= f64::EPSILON {
        return DepletionForecast::NoBurn;
    }
    let Some(reset) = claim.resets_at.as_deref().and_then(parse_reset_timestamp) else {
        return DepletionForecast::Unavailable;
    };
    if reset <= now || window_seconds <= 0 {
        return DepletionForecast::Unavailable;
    }
    let window_start = reset - time::Duration::seconds(window_seconds);
    let elapsed_seconds = (now - window_start).whole_seconds();
    if elapsed_seconds <= 0 || elapsed_seconds > window_seconds {
        return DepletionForecast::Learning;
    }
    // Suppress extrapolation during the first tenth of a window unless an
    // unusually large share is already gone. Early bursts otherwise produce
    // alarming but unstable run-out times.
    if elapsed_seconds < window_seconds / 10 && utilization < 0.2 {
        return DepletionForecast::Learning;
    }
    let remaining_seconds = (elapsed_seconds as f64 * (1.0 - utilization) / utilization).ceil();
    if !remaining_seconds.is_finite() || remaining_seconds > i64::MAX as f64 {
        return DepletionForecast::Unavailable;
    }
    let Some(run_out) = now.checked_add(time::Duration::seconds(remaining_seconds as i64)) else {
        return DepletionForecast::Unavailable;
    };
    if run_out >= reset {
        DepletionForecast::ResetFirst
    } else {
        DepletionForecast::RunsOutAt(run_out)
    }
}

#[cfg(test)]
mod tests {
    use rayline_subscriptions::{ClaimScope, ClaimStatus, LimitClaim, LimitSource};
    use time::format_description::well_known::Rfc3339;

    use super::*;

    fn claim(utilization: f64) -> LimitClaim {
        LimitClaim {
            key: "five_hour".to_owned(),
            scope: ClaimScope::Global,
            utilization: Some(utilization),
            status: ClaimStatus::Allowed,
            resets_at: Some("2030-01-01T15:00:00Z".to_owned()),
            source: LimitSource::UsageEndpoint,
        }
    }

    #[test]
    fn avoids_unstable_early_window_estimates() {
        let now = OffsetDateTime::parse("2030-01-01T10:10:00Z", &Rfc3339).expect("now");

        assert_eq!(
            depletion_forecast(Some(&claim(0.05)), true, 5 * 60 * 60, now),
            DepletionForecast::Learning
        );
        assert!(matches!(
            depletion_forecast(Some(&claim(0.25)), true, 5 * 60 * 60, now),
            DepletionForecast::RunsOutAt(_)
        ));
    }

    #[test]
    fn distinguishes_pre_reset_risk_from_reset_first() {
        let now = OffsetDateTime::parse("2030-01-01T12:00:00Z", &Rfc3339).expect("now");

        assert_eq!(
            depletion_forecast(Some(&claim(0.5)), true, 5 * 60 * 60, now),
            DepletionForecast::RunsOutAt(
                OffsetDateTime::parse("2030-01-01T14:00:00Z", &Rfc3339).expect("run-out")
            )
        );
        assert_eq!(
            depletion_forecast(Some(&claim(0.25)), true, 5 * 60 * 60, now),
            DepletionForecast::ResetFirst
        );
    }

    #[test]
    fn labels_missing_stale_idle_and_exhausted_snapshots() {
        let now = OffsetDateTime::parse("2030-01-01T12:00:00Z", &Rfc3339).expect("now");

        assert_eq!(
            depletion_forecast(None, true, 5 * 60 * 60, now),
            DepletionForecast::Unavailable
        );
        assert_eq!(
            depletion_forecast(Some(&claim(0.5)), false, 5 * 60 * 60, now),
            DepletionForecast::Stale
        );
        assert_eq!(
            depletion_forecast(Some(&claim(0.0)), true, 5 * 60 * 60, now),
            DepletionForecast::NoBurn
        );
        assert_eq!(
            depletion_forecast(Some(&claim(1.0)), true, 5 * 60 * 60, now),
            DepletionForecast::Exhausted
        );
    }
}
