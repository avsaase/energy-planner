//! Turning irregular Home Assistant state changes into regularly sampled series.

use std::collections::VecDeque;

use jiff::{RoundMode, SignedDuration, Timestamp, TimestampRound, Unit};

/// Rounds `t` down to a multiple of `minutes` (which must divide a day).
pub fn floor_to_minutes(t: Timestamp, minutes: i64) -> Timestamp {
    t.round(
        TimestampRound::new()
            .smallest(Unit::Minute)
            .increment(minutes)
            .mode(RoundMode::Trunc),
    )
    .expect("increment divides a day")
}

/// A piecewise-constant signal built from state changes: each value holds until the next one.
///
/// `NaN` values mark gaps (unavailable/unknown states) and are excluded from averages.
#[derive(Debug, Clone)]
pub struct StepSeries {
    samples: VecDeque<(Timestamp, f64)>,
    /// A value older than this is treated as stale (sensor stopped reporting).
    max_hold: SignedDuration,
}

impl StepSeries {
    pub fn new(max_hold: SignedDuration) -> Self {
        Self {
            samples: VecDeque::new(),
            max_hold,
        }
    }

    /// Adds a sample. Samples are expected in time order; an older one is inserted in place.
    pub fn push(&mut self, at: Timestamp, value: f64) {
        match self.samples.back() {
            Some((last, _)) if at < *last => {
                let pos = self.samples.partition_point(|(t, _)| *t <= at);
                self.samples.insert(pos, (at, value));
            }
            _ => self.samples.push_back((at, value)),
        }
    }

    /// Drops samples that no longer influence anything at or after `before`.
    pub fn prune(&mut self, before: Timestamp) {
        while self.samples.len() >= 2 && self.samples[1].0 <= before {
            self.samples.pop_front();
        }
    }

    pub fn latest(&self) -> Option<(Timestamp, f64)> {
        self.samples.back().copied()
    }

    /// Current value at `now`, if the last sample is finite and not stale.
    pub fn value_at(&self, now: Timestamp) -> Option<f64> {
        let pos = self.samples.partition_point(|(t, _)| *t <= now);
        let (t, v) = *self.samples.get(pos.checked_sub(1)?)?;
        (v.is_finite() && now.duration_since(t) <= self.max_hold).then_some(v)
    }

    /// Time-weighted mean over `n` buckets of `step` starting at `start`, only using the signal
    /// up to `until`. Buckets without data are `NaN`.
    pub fn bucket_means(
        &self,
        start: Timestamp,
        step: SignedDuration,
        n: usize,
        until: Timestamp,
    ) -> Vec<f64> {
        let mut sums = vec![0.0; n];
        let mut weights = vec![0.0; n];
        let end = (start + step * n as i32).min(until);

        for (i, &(t, v)) in self.samples.iter().enumerate() {
            if !v.is_finite() {
                continue;
            }
            let next = self.samples.get(i + 1).map_or(until, |(t, _)| *t);
            let seg_start = t.max(start);
            let seg_end = next.min(t + self.max_hold).min(end);
            let mut s = seg_start;
            while s < seg_end {
                let bucket = bucket_index(start, step, s);
                let bucket_end = (start + step * (bucket as i32 + 1)).min(seg_end);
                let secs = bucket_end.duration_since(s).as_secs_f64();
                sums[bucket] += v * secs;
                weights[bucket] += secs;
                s = bucket_end;
            }
        }

        sums.iter()
            .zip(&weights)
            .map(|(s, w)| if *w > 0.0 { s / w } else { f64::NAN })
            .collect()
    }
}

/// Index of the bucket containing `t` (exact integer arithmetic, `t >= start`).
fn bucket_index(start: Timestamp, step: SignedDuration, t: Timestamp) -> usize {
    (t.duration_since(start).as_nanos() / step.as_nanos()) as usize
}

/// Mean of the points falling in each of `n` buckets of `step` from `start`. `NaN` if none.
pub fn aggregate_points(
    points: impl IntoIterator<Item = (Timestamp, f64)>,
    start: Timestamp,
    step: SignedDuration,
    n: usize,
) -> Vec<f64> {
    let mut sums = vec![0.0; n];
    let mut counts = vec![0usize; n];
    for (t, v) in points {
        if t < start || !v.is_finite() {
            continue;
        }
        let bucket = bucket_index(start, step, t);
        if bucket < n {
            sums[bucket] += v;
            counts[bucket] += 1;
        }
    }
    sums.iter()
        .zip(&counts)
        .map(|(s, &c)| if c > 0 { s / c as f64 } else { f64::NAN })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    #[test]
    fn bucket_means_are_time_weighted() {
        let mut s = StepSeries::new(SignedDuration::from_hours(1));
        s.push(ts(0), 100.0);
        s.push(ts(30), 300.0);
        let means = s.bucket_means(ts(0), SignedDuration::from_mins(1), 2, ts(120));
        assert_eq!(means, vec![200.0, 300.0]);
    }

    #[test]
    fn bucket_means_respect_until_gaps_and_staleness() {
        let mut s = StepSeries::new(SignedDuration::from_secs(90));
        s.push(ts(60), 100.0);
        s.push(ts(120), f64::NAN);
        s.push(ts(180), 50.0);
        let means = s.bucket_means(ts(0), SignedDuration::from_mins(1), 6, ts(330));
        assert!(means[0].is_nan()); // before first sample
        assert_eq!(means[1], 100.0);
        assert!(means[2].is_nan()); // unavailable
        assert_eq!(means[3], 50.0);
        assert_eq!(means[4], 50.0); // 240..270 still fresh, 270..300 stale
        assert!(means[5].is_nan()); // stale and after `until`
    }

    #[test]
    fn out_of_order_push_and_value_at() {
        let mut s = StepSeries::new(SignedDuration::from_mins(5));
        s.push(ts(10), 1.0);
        s.push(ts(30), 3.0);
        s.push(ts(20), 2.0);
        assert_eq!(s.value_at(ts(25)), Some(2.0));
        assert_eq!(s.value_at(ts(5)), None);
        assert_eq!(s.value_at(ts(30 + 301)), None);
        s.prune(ts(25));
        assert_eq!(s.value_at(ts(25)), Some(2.0));
    }

    #[test]
    fn aggregate_points_buckets_by_start() {
        let points = [(ts(0), 1.0), (ts(300), 3.0), (ts(900), 10.0)];
        let means = aggregate_points(points, ts(0), SignedDuration::from_mins(15), 3);
        assert_eq!(means[0], 2.0);
        assert_eq!(means[1], 10.0);
        assert!(means[2].is_nan());
    }
}
