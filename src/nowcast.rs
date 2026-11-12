//! Combining the long-range forecast with live measurements into optimizer inputs.

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{
    forecaster::empirical_quantile, forecasts::LongRangeForecast, live::floor_to_minute,
    series::floor_to_minutes,
};

const MINUTE: SignedDuration = SignedDuration::from_mins(1);
const FINE_STEP: SignedDuration = SignedDuration::from_mins(5);
const COARSE_STEP: SignedDuration = SignedDuration::from_mins(15);
/// The measured house load fades into the long-range forecast with this time constant. 20 min
/// matched a 1-minute Chronos-2 forecast in the backtest (`backtest.py nowcast`).
const NOWCAST_TAU_MINUTES: f64 = 20.0;
/// The measured PV fades into the corrected forecast with this time constant. Not backtested.
const SOLAR_NOWCAST_TAU_MINUTES: f64 = 10.0;
/// Solar correction: the measured/forecast ratio fades with this time constant.
const SOLAR_RATIO_DECAY_HOURS: f64 = 1.5;
const SOLAR_RATIO_WINDOW_MINUTES: usize = 30;
/// Below this average forecast the ratio is meaningless (night, dawn).
const SOLAR_RATIO_MIN_FORECAST_W: f64 = 50.0;

/// Optimizer intervals: 5 minutes up to the second 15-minute boundary, 15 minutes after that.
/// A first interval shorter than a minute is merged into the next one.
pub fn planning_intervals(now: Timestamp, until: Timestamp) -> Vec<(Timestamp, Timestamp)> {
    let floor = |t: Timestamp, step: SignedDuration| floor_to_minutes(t, step.as_mins());
    let mut first_end = floor(now, FINE_STEP) + FINE_STEP;
    if first_end.duration_since(now) < MINUTE {
        first_end += FINE_STEP;
    }
    let fine_end = floor(now, COARSE_STEP) + COARSE_STEP * 2;

    let mut intervals = vec![(now, first_end)];
    let mut t = first_end;
    while t < fine_end {
        intervals.push((t, t + FINE_STEP));
        t += FINE_STEP;
    }
    while t + COARSE_STEP <= until {
        intervals.push((t, t + COARSE_STEP));
        t += COARSE_STEP;
    }
    intervals
}

/// Time-weighted mean of a per-minute function over `[start, end)`. `None` if any minute is
/// unknown.
pub fn interval_mean(
    start: Timestamp,
    end: Timestamp,
    f: impl Fn(Timestamp) -> Option<f64>,
) -> Option<f64> {
    let mut t = floor_to_minute(start);
    let (mut sum, mut weight) = (0.0, 0.0);
    while t < end {
        let secs = (t + MINUTE)
            .min(end)
            .duration_since(t.max(start))
            .as_secs_f64();
        sum += f(t)? * secs;
        weight += secs;
        t += MINUTE;
    }
    (weight > 0.0).then(|| sum / weight)
}

/// The latest measurement, to correct the start of the long-range forecast.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Nowcast {
    /// Start of the minute containing "now"; minute 0 of the blend.
    pub start: Timestamp,
    /// House load averaged over the last few seconds.
    pub current_house_load_w: f64,
    /// PV averaged over the last few seconds.
    pub current_solar_w: f64,
}

impl Nowcast {
    /// Weight of the measurement at minute `t`; `None` before the nowcast starts.
    fn weight(&self, t: Timestamp, tau_minutes: f64) -> Option<f64> {
        (t >= self.start).then(|| {
            let minute = t.duration_since(self.start).as_secs_f64() / 60.0;
            (-(minute + 0.5) / tau_minutes).exp()
        })
    }

    /// PV at minute `t`: the measurement fading into `forecast_w`.
    pub fn solar_at(&self, t: Timestamp, forecast_w: f64) -> f64 {
        match self.weight(t, SOLAR_NOWCAST_TAU_MINUTES) {
            Some(w) => w * self.current_solar_w + (1.0 - w) * forecast_w,
            None => forecast_w,
        }
    }
}

/// House load per minute: the current measurement fading into the long-range forecast.
pub struct HouseLoad<'a> {
    pub nowcast: Option<Nowcast>,
    pub long: &'a LongRangeForecast,
}

impl HouseLoad<'_> {
    /// Expected house load at minute `t`.
    pub fn mean_at(&self, t: Timestamp) -> Option<f64> {
        let long = self.long.house_load_at(t)?;
        let Some((nowcast, w)) = self
            .nowcast
            .and_then(|n| Some((n, n.weight(t, NOWCAST_TAU_MINUTES)?)))
        else {
            return Some(long);
        };
        Some(w * nowcast.current_house_load_w + (1.0 - w) * long)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SolarCorrection {
    /// Measured / forecast PV over the last half hour.
    pub ratio: f64,
    /// Standard deviation of the measured PV around the corrected forecast (W).
    pub residual_std_w: f64,
}

impl Default for SolarCorrection {
    fn default() -> Self {
        Self {
            ratio: 1.0,
            residual_std_w: 0.0,
        }
    }
}

impl SolarCorrection {
    /// From aligned per-minute measured and forecast PV (last values are the most recent).
    pub fn estimate(actual: &[f64], forecast: &[f64]) -> Self {
        let pairs: Vec<(f64, f64)> = actual
            .iter()
            .zip(forecast)
            .rev()
            .take(SOLAR_RATIO_WINDOW_MINUTES)
            .map(|(a, f)| (*a, *f))
            .filter(|(a, f)| a.is_finite() && f.is_finite())
            .collect();
        if pairs.is_empty() {
            return Self::default();
        }
        let sum_actual: f64 = pairs.iter().map(|p| p.0).sum();
        let sum_forecast: f64 = pairs.iter().map(|p| p.1).sum();
        let ratio = if sum_forecast / (pairs.len() as f64) < SOLAR_RATIO_MIN_FORECAST_W {
            1.0
        } else {
            (sum_actual / sum_forecast).clamp(0.1, 1.5)
        };
        let variance = pairs
            .iter()
            .map(|(a, f)| (a - ratio * f).powi(2))
            .sum::<f64>()
            / pairs.len() as f64;
        Self {
            ratio,
            residual_std_w: variance.sqrt(),
        }
    }

    /// Corrected forecast `hours_ahead` from now: the ratio fades back to 1.
    pub fn apply(&self, forecast_w: f64, hours_ahead: f64) -> f64 {
        let decay = (-hours_ahead.max(0.0) / SOLAR_RATIO_DECAY_HOURS).exp();
        forecast_w * (1.0 + (self.ratio - 1.0) * decay)
    }
}

/// How much house load moved over `minutes` in the recent past: quantiles `1 - quantile` and
/// `quantile` of (mean of the next `minutes`) − (value now), over all minutes of `history`.
/// Returned as `(below, above)` magnitudes; `(0, 0)` without enough data.
pub fn house_load_change_spread(history: &[f64], minutes: usize, quantile: f64) -> (f64, f64) {
    let k = minutes.max(1);
    let changes: Vec<f64> = (0..history.len().saturating_sub(k))
        .filter_map(|i| {
            let window = &history[i + 1..=i + k];
            (history[i].is_finite() && window.iter().all(|v| v.is_finite()))
                .then(|| window.iter().sum::<f64>() / k as f64 - history[i])
        })
        .collect();
    if changes.len() < 60 {
        return (0.0, 0.0);
    }
    (
        (-empirical_quantile(&changes, 1.0 - quantile)).max(0.0),
        empirical_quantile(&changes, quantile).max(0.0),
    )
}

/// How far the high and low scenarios of the first interval's net load lie from its expected
/// value: `(below, above)`, both ≥ `min_spread_w`. Combines the house load spread with the PV
/// uncertainty, assuming they are independent.
pub fn net_load_spread(
    house_load_spread: (f64, f64),
    quantile: f64,
    solar: &SolarCorrection,
    min_spread_w: f64,
) -> (f64, f64) {
    let solar_spread = normal_quantile(quantile).abs() * solar.residual_std_w;
    let combine = |load: f64| load.hypot(solar_spread).max(min_spread_w);
    (combine(house_load_spread.0), combine(house_load_spread.1))
}

/// Inverse of the standard normal CDF (Abramowitz & Stegun 26.2.23, |error| < 4.5e-4).
fn normal_quantile(p: f64) -> f64 {
    let p = p.clamp(1e-6, 1.0 - 1e-6);
    let (q, sign) = if p < 0.5 { (p, -1.0) } else { (1.0 - p, 1.0) };
    let t = (-2.0 * q.ln()).sqrt();
    let z = t
        - (2.515517 + 0.802853 * t + 0.010328 * t * t)
            / (1.0 + 1.432788 * t + 0.189269 * t * t + 0.001308 * t * t * t);
    sign * z
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forecaster::Forecast;

    fn ts(h: i64, m: i64, s: i64) -> Timestamp {
        Timestamp::from_second(h * 3600 + m * 60 + s).unwrap()
    }

    #[test]
    fn intervals_are_fine_then_coarse() {
        let i = planning_intervals(ts(10, 7, 0), ts(11, 0, 0));
        let starts: Vec<_> = i.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            starts,
            vec![
                ts(10, 7, 0),
                ts(10, 10, 0),
                ts(10, 15, 0),
                ts(10, 20, 0),
                ts(10, 25, 0),
                ts(10, 30, 0),
                ts(10, 45, 0),
            ]
        );
        assert_eq!(i.last().unwrap().1, ts(11, 0, 0));
    }

    #[test]
    fn tiny_first_interval_is_merged() {
        let i = planning_intervals(ts(10, 9, 30), ts(10, 30, 0));
        assert_eq!(i[0], (ts(10, 9, 30), ts(10, 15, 0)));
    }

    #[test]
    fn interval_mean_weights_partial_minutes() {
        let v = interval_mean(ts(0, 0, 30), ts(0, 2, 0), |t| {
            Some(if t < ts(0, 1, 0) { 0.0 } else { 300.0 })
        });
        assert_eq!(v, Some(200.0));
        assert_eq!(interval_mean(ts(0, 0, 0), ts(0, 1, 0), |_| None), None);
    }

    fn long(start: Timestamp, value: f64) -> LongRangeForecast {
        LongRangeForecast {
            start,
            house_load: Forecast::deterministic(vec![value; 8]),
        }
    }

    #[test]
    fn measurement_fades_into_long_range() {
        let start = ts(0, 0, 0);
        let long = long(start, 100.0);
        let load = HouseLoad {
            nowcast: Some(Nowcast {
                start,
                current_house_load_w: 1100.0,
                current_solar_w: 0.0,
            }),
            long: &long,
        };
        let at = |m| load.mean_at(ts(0, m, 0)).unwrap();
        assert!((at(0) - 1075.3).abs() < 0.1, "{}", at(0));
        // After one time constant ~37% of the difference is left.
        assert!((at(20) - (100.0 + 1000.0 * (-20.5f64 / 20.0).exp())).abs() < 1e-9);
        assert!((at(119) - 100.0).abs() < 3.0);
        let without = HouseLoad {
            nowcast: None,
            long: &long,
        };
        assert_eq!(without.mean_at(ts(0, 0, 0)), Some(100.0));
    }

    #[test]
    fn measured_solar_fades_into_forecast() {
        let n = Nowcast {
            start: ts(12, 0, 0),
            current_house_load_w: 300.0,
            current_solar_w: 1500.0,
        };
        let at = |m| n.solar_at(ts(12, m, 0), 400.0);
        assert!((at(0) - 1446.0).abs() < 1.0, "{}", at(0));
        assert!((at(10) - (400.0 + 1100.0 * (-1.05f64).exp())).abs() < 1e-9);
        assert!((at(60) - 400.0).abs() < 3.0);
        assert_eq!(n.solar_at(ts(11, 59, 0), 400.0), 400.0);
    }

    #[test]
    fn change_spread_from_history() {
        // Flat 500 W with a +2 kW step lasting 30 minutes once every 4 hours.
        let history: Vec<f64> = (0..1440)
            .map(|i| if i % 240 < 30 { 2500.0 } else { 500.0 })
            .collect();
        let (below, above) = house_load_change_spread(&history, 5, 0.99);
        assert!(above > 1000.0 && below > 1000.0, "{below} {above}");
        let (below, above) = house_load_change_spread(&history, 5, 0.9);
        assert_eq!((below, above), (0.0, 0.0));
        assert_eq!(house_load_change_spread(&history[..30], 5, 0.9), (0.0, 0.0));
    }

    #[test]
    fn solar_correction_ratio_and_decay() {
        let c = SolarCorrection::estimate(&[500.0; 30], &[1000.0; 30]);
        assert_eq!(c.ratio, 0.5);
        assert_eq!(c.residual_std_w, 0.0);
        assert_eq!(c.apply(1000.0, 0.0), 500.0);
        assert!((c.apply(1000.0, 24.0) - 1000.0).abs() < 1e-3);
        // At night the ratio is not estimated.
        assert_eq!(
            SolarCorrection::estimate(&[5.0; 30], &[10.0; 30]).ratio,
            1.0
        );
    }

    #[test]
    fn spread_respects_minimum_and_adds_solar() {
        let no_solar = SolarCorrection::default();
        assert_eq!(
            net_load_spread((0.0, 100.0), 0.9, &no_solar, 300.0),
            (300.0, 300.0)
        );
        assert_eq!(
            net_load_spread((400.0, 800.0), 0.9, &no_solar, 300.0),
            (400.0, 800.0)
        );
        let solar = SolarCorrection {
            ratio: 1.0,
            residual_std_w: 1000.0,
        };
        let (below, above) = net_load_spread((0.0, 0.0), 0.9, &solar, 300.0);
        assert!((above - 1281.6).abs() < 1.0, "{above}");
        assert_eq!(below, above);
    }
}
