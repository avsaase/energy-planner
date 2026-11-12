//! Live measurements from Home Assistant and replan triggers.
//!
//! Conventions:
//! - `P_grid`: + import, − export. `P_bat`: + discharge, − charge. `P_pv` ≥ 0.
//! - House load `L = P_grid + P_pv + P_bat`, net load `N = L − P_pv = P_grid + P_bat`.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use jiff::{SignedDuration, Timestamp, tz::TimeZone};
use tracing::{info, warn};

use crate::{
    AppState,
    home_assistant::{addon::AddonOptions, client::HaClient},
    series::{StepSeries, floor_to_minutes},
};

/// How much raw history is kept in memory (short-range forecast context).
pub const SHORT_HISTORY: SignedDuration = SignedDuration::from_hours(24);
/// Sensors only report changes, so a value can legitimately hold for a long time (no PV at
/// night). Gaps are marked explicitly by `unavailable` states instead.
const MAX_HOLD: SignedDuration = SignedDuration::from_hours(7 * 24);

/// A longer pause between two iterations of the live loop means events may be missing.
const MAX_LOOP_PAUSE: SignedDuration = SignedDuration::from_secs(60);

pub type SharedLive = Arc<Mutex<LiveData>>;

#[derive(Debug)]
pub struct Meter {
    pub entity: String,
    /// Unit conversion and sign normalisation.
    pub factor: f64,
    pub series: StepSeries,
}

impl Meter {
    fn new(entity: &str, factor: f64) -> Self {
        Self {
            entity: entity.to_string(),
            factor,
            series: StepSeries::new(MAX_HOLD),
        }
    }

    fn value_at(&self, now: Timestamp) -> Option<f64> {
        self.series.value_at(now)
    }
}

/// Regular 1-minute history ending at the last complete minute.
#[derive(Debug, Clone)]
pub struct MinuteHistory {
    pub start: Timestamp,
    pub house_load: Vec<f64>,
    pub solar: Vec<f64>,
}

#[derive(Debug)]
pub struct LiveData {
    pub grid: Meter,
    pub solar: Meter,
    pub battery: Meter,
    pub soc: Meter,
    pub change_detector: NetLoadChangeDetector,
    /// Up to when the meters are known complete: the time of the last history load or live
    /// event. A reconnect fills in the history from here.
    pub complete_until: Option<Timestamp>,
}

impl LiveData {
    /// Sets up meters, querying each entity's unit.
    pub async fn connect(ha: &HaClient, options: &AddonOptions) -> Self {
        let factor = async |entity: &str, invert: bool| {
            let unit = ha.get_unit_factor(entity).await.unwrap_or_else(|e| {
                warn!(entity, error = %e, "Could not read unit, assuming W");
                1.0
            });
            if invert { -unit } else { unit }
        };
        let s = &options.sensors;
        Self {
            grid: Meter::new(
                &s.grid_power_entity,
                factor(&s.grid_power_entity, s.grid_power_invert).await,
            ),
            solar: Meter::new(
                &s.solar_power_entity,
                factor(&s.solar_power_entity, false).await,
            ),
            battery: Meter::new(
                &s.battery_power_entity,
                factor(&s.battery_power_entity, s.battery_power_invert).await,
            ),
            soc: Meter::new(
                &options.battery_soc_entity,
                factor(&options.battery_soc_entity, false).await,
            ),
            change_detector: NetLoadChangeDetector::new(&options.replanning),
            complete_until: None,
        }
    }

    /// Whether the live data is current: the P1 meter reports about every second, so a newest
    /// value older than `max_age` means updates are missing (connection lost, process paused).
    /// Without fresh data the last values would be extended over the gap.
    pub fn is_fresh(&self, now: Timestamp, max_age: SignedDuration) -> bool {
        self.grid
            .series
            .latest()
            .is_some_and(|(at, _)| now.duration_since(at) <= max_age)
    }

    fn meters_mut(&mut self) -> impl Iterator<Item = &mut Meter> {
        [
            &mut self.grid,
            &mut self.solar,
            &mut self.battery,
            &mut self.soc,
        ]
        .into_iter()
    }

    pub fn entity_ids(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        [&self.grid, &self.solar, &self.battery, &self.soc]
            .into_iter()
            .map(|m| m.entity.clone())
            .filter(|e| seen.insert(e.clone()))
            .collect()
    }

    /// Records a raw state value (`NaN` for unavailable).
    pub fn record(&mut self, entity: &str, at: Timestamp, raw: f64) {
        for meter in self.meters_mut().filter(|m| m.entity == entity) {
            meter.series.push(at, raw * meter.factor);
        }
    }

    pub fn net_load_w(&self, now: Timestamp) -> Option<f64> {
        Some(self.grid.value_at(now)? + self.battery.value_at(now)?)
    }

    pub fn house_load_w(&self, now: Timestamp) -> Option<f64> {
        Some(self.net_load_w(now)? + self.solar.value_at(now)?)
    }

    /// House load averaged over the `window` before `now`. Short enough to reflect a change that
    /// just triggered a replan, long enough to smooth out meters updating at different times.
    pub fn recent_house_load_w(&self, now: Timestamp, window: SignedDuration) -> Option<f64> {
        let mean = |m: &Meter| recent_mean(m, now, window);
        Some(mean(&self.grid)? + mean(&self.solar)? + mean(&self.battery)?)
    }

    pub fn recent_solar_w(&self, now: Timestamp, window: SignedDuration) -> Option<f64> {
        recent_mean(&self.solar, now, window)
    }

    /// Battery state of charge as a fraction.
    pub fn soc(&self, now: Timestamp) -> Option<f64> {
        self.soc.value_at(now)
    }

    pub fn prune(&mut self, now: Timestamp) {
        let before = now - SHORT_HISTORY - SignedDuration::from_hours(1);
        self.meters_mut().for_each(|m| m.series.prune(before));
    }

    /// The last `minutes` complete minutes before `now`.
    pub fn minute_history(&self, now: Timestamp, minutes: usize) -> MinuteHistory {
        let end = floor_to_minute(now);
        let start = end - SignedDuration::from_mins(minutes as i64);
        let step = SignedDuration::from_mins(1);
        let means = |m: &Meter| m.series.bucket_means(start, step, minutes, end);

        let grid = means(&self.grid);
        let solar = means(&self.solar);
        let battery = means(&self.battery);
        let house_load: Vec<f64> = (0..minutes)
            .map(|i| grid[i] + solar[i] + battery[i])
            .collect();
        MinuteHistory {
            start,
            house_load,
            solar,
        }
    }
}

fn recent_mean(meter: &Meter, now: Timestamp, window: SignedDuration) -> Option<f64> {
    let mean = meter.series.bucket_means(now - window, window, 1, now)[0];
    mean.is_finite().then_some(mean)
}

pub fn floor_to_minute(t: Timestamp) -> Timestamp {
    floor_to_minutes(t, 1)
}

/// Fires when net load stays more than a threshold away from its value at the last replan,
/// in the same direction, for a sustained period.
#[derive(Debug)]
pub struct NetLoadChangeDetector {
    threshold_w: f64,
    sustain: SignedDuration,
    reference: Option<f64>,
    exceeded_since: Option<(Timestamp, bool)>,
}

impl NetLoadChangeDetector {
    pub fn new(options: &crate::home_assistant::addon::ReplanOptions) -> Self {
        Self {
            threshold_w: options.net_load_change_threshold_w,
            sustain: SignedDuration::from_secs(options.net_load_change_sustain_seconds as i64),
            reference: None,
            exceeded_since: None,
        }
    }

    /// Called by the planner with the net load it planned for.
    pub fn reset(&mut self, reference: Option<f64>) {
        self.reference = reference;
        self.exceeded_since = None;
    }

    pub fn observe(&mut self, now: Timestamp, net_load_w: f64) -> bool {
        let Some(reference) = self.reference else {
            self.reference = Some(net_load_w);
            return false;
        };
        let delta = net_load_w - reference;
        if delta.abs() <= self.threshold_w {
            self.exceeded_since = None;
            return false;
        }
        let up = delta > 0.0;
        match self.exceeded_since {
            Some((since, dir)) if dir == up => {
                if now.duration_since(since) >= self.sustain {
                    self.reset(Some(net_load_w));
                    true
                } else {
                    false
                }
            }
            _ => {
                self.exceeded_since = Some((now, up));
                false
            }
        }
    }
}

/// Fires when the measured SOC deviates from the plan's SOC path, at most once per plan: the
/// deviation persists until the replan has stored a new plan, and every sensor update in between
/// would otherwise trigger again.
#[derive(Debug)]
pub struct SocDeviationTrigger {
    threshold: f64,
    /// `planned_at` of the plan this trigger last fired for.
    fired_for: Option<Timestamp>,
}

impl SocDeviationTrigger {
    pub fn new(threshold: f64) -> Self {
        Self {
            threshold,
            fired_for: None,
        }
    }

    /// `plan` identifies the current plan (its `planned_at`).
    pub fn check(&mut self, soc: f64, planned_soc: f64, plan: Timestamp) -> bool {
        if (soc - planned_soc).abs() <= self.threshold || self.fired_for == Some(plan) {
            return false;
        }
        self.fired_for = Some(plan);
        true
    }
}

/// Loads the last [`SHORT_HISTORY`] of raw states into the meters.
pub async fn load_recent_history(ha: &HaClient, live: &SharedLive) -> anyhow::Result<()> {
    let now = Timestamp::now();
    backfill(ha, live, now - SHORT_HISTORY, now).await
}

/// Fills the meters with the raw states between `since` and `until` from HA's recorder, e.g.
/// after the live connection was down. Uses its own connection.
pub async fn backfill(
    ha: &HaClient,
    live: &SharedLive,
    since: Timestamp,
    until: Timestamp,
) -> anyhow::Result<()> {
    let since = since.max(until - SHORT_HISTORY);
    let entities = live.lock().unwrap().entity_ids();
    let refs: Vec<&str> = entities.iter().map(String::as_str).collect();
    let mut ws = ha.connect_websocket().await?;
    let history = ws.get_history(&refs, since, until).await?;
    let mut live = live.lock().unwrap();
    for (entity, points) in &history {
        for &(at, value) in points {
            live.record(entity, at, value);
        }
    }
    live.complete_until = Some(live.complete_until.map_or(until, |t| t.max(until)));
    info!(
        %since,
        %until,
        states = history.values().map(Vec::len).sum::<usize>(),
        "Loaded sensor history"
    );
    Ok(())
}

/// Follows all sensors and triggers replans on material changes. Runs forever, reconnecting on
/// errors.
pub async fn run_live_updates(
    ha: HaClient,
    live: SharedLive,
    app_state: AppState,
    soc_deviation: f64,
) {
    loop {
        if let Err(e) = live_session(&ha, &live, &app_state, soc_deviation).await {
            warn!(error = %e, "Live sensor subscription failed, reconnecting");
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn live_session(
    ha: &HaClient,
    live: &SharedLive,
    app_state: &AppState,
    soc_deviation: f64,
) -> anyhow::Result<()> {
    let mut ws = ha.connect_websocket().await?;
    let entities = live.lock().unwrap().entity_ids();
    let refs: Vec<&str> = entities.iter().map(String::as_str).collect();
    ws.subscribe_to_entities(&refs).await?;
    info!(?entities, "Subscribed to live sensors");
    let mut soc_trigger = SocDeviationTrigger::new(soc_deviation);

    // Anything missed while not subscribed (reconnect, startup) comes from the recorder. Events
    // arriving meanwhile are recorded too; the meters keep samples in time order.
    let since = live.lock().unwrap().complete_until;
    if let Some(since) = since {
        backfill(ha, live, since, Timestamp::now()).await?;
    }
    let mut last_iteration = Timestamp::now();

    loop {
        // Also wake up periodically to check the SOC against the plan when sensors are quiet.
        if let Ok(change) =
            tokio::time::timeout(Duration::from_secs(10), ws.next_state_change()).await
        {
            let (entity, state) = change?;
            let (at, value) = match state {
                Some(s) => (s.last_updated, s.state.parse().unwrap_or(f64::NAN)),
                None => (Timestamp::now(), f64::NAN),
            };
            let mut live = live.lock().unwrap();
            live.record(&entity, at, value);
            live.complete_until = Some(Timestamp::now());
        }

        // The loop runs at least every 10 s. A much longer pause (process suspended, e.g. a
        // sleeping laptop) may have lost events even though the connection survived.
        let now = Timestamp::now();
        if now.duration_since(last_iteration) > MAX_LOOP_PAUSE {
            warn!(paused_since = %last_iteration, "Live updates paused, filling in history");
            backfill(ha, live, last_iteration, now).await?;
        }
        last_iteration = now;

        let (reasons, soc) = {
            let mut live = live.lock().unwrap();
            let mut reasons: Vec<String> = Vec::new();
            if let Some(n) = live.net_load_w(now)
                && live.change_detector.observe(now, n)
            {
                reasons.push(format!("net load changed to {n:.0} W"));
            }
            live.prune(now);
            (reasons, live.soc(now))
        };

        let mut reasons = reasons;
        if let Some(soc) = soc {
            let zoned = now.to_zoned(TimeZone::system());
            let planned = app_state
                .read()
                .await
                .current_plan
                .as_ref()
                .and_then(|p| Some((p.planned_soc_at(&zoned)?, p.planned_at.timestamp())));
            if let Some((planned, plan)) = planned
                && soc_trigger.check(soc, planned, plan)
            {
                reasons.push(format!(
                    "SOC {:.1}% deviates from planned {:.1}%",
                    soc * 100.0,
                    planned * 100.0
                ));
            }
        }

        if !reasons.is_empty() {
            info!(?reasons, "Triggering replan");
            app_state.start_plan.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home_assistant::addon::ReplanOptions;

    fn ts(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    /// Synthetic 1 Hz net load: noise around 500 W, an oven step of +2 kW at t=100 s and a
    /// short spike (boiler burst) at t=300 s.
    fn trace() -> Vec<(Timestamp, f64)> {
        (0..600)
            .map(|s| {
                let noise = if s % 2 == 0 { 120.0 } else { -120.0 };
                let oven = if s >= 100 { 2000.0 } else { 0.0 };
                let spike = if (300..305).contains(&s) { 1500.0 } else { 0.0 };
                (ts(s), 500.0 + noise + oven + spike)
            })
            .collect()
    }

    #[test]
    fn detector_fires_once_per_sustained_change_and_ignores_noise_and_spikes() {
        let mut d = NetLoadChangeDetector::new(&ReplanOptions::default());
        d.reset(Some(500.0));
        let fired: Vec<i64> = trace()
            .into_iter()
            .filter(|&(t, n)| d.observe(t, n))
            .map(|(t, _)| t.as_second())
            .collect();
        assert_eq!(fired, vec![115]);
    }

    #[test]
    fn soc_trigger_fires_once_per_plan() {
        let mut trigger = SocDeviationTrigger::new(0.03);
        let plan_a = ts(0);
        let plan_b = ts(60);
        assert!(!trigger.check(0.50, 0.52, plan_a)); // within threshold
        assert!(trigger.check(0.55, 0.59, plan_a));
        assert!(!trigger.check(0.55, 0.59, plan_a)); // same plan, already fired
        assert!(trigger.check(0.55, 0.59, plan_b)); // new plan that still deviates
    }

    fn live_data() -> LiveData {
        LiveData {
            grid: Meter::new("sensor.grid", 1.0),
            solar: Meter::new("sensor.solar", 1.0),
            battery: Meter::new("sensor.battery", 1.0),
            soc: Meter::new("sensor.soc", 0.01),
            change_detector: NetLoadChangeDetector::new(&ReplanOptions::default()),
            complete_until: None,
        }
    }

    /// The 18:53 case from the dry run: 2.3 kW measured just before the connection was lost,
    /// the device switched off during the gap.
    #[test]
    fn backfilled_states_replace_values_held_over_a_gap() {
        let mut live = live_data();
        live.record("sensor.solar", ts(0), 0.0);
        live.record("sensor.battery", ts(0), 0.0);
        live.record("sensor.grid", ts(0), 2340.0);
        let now = ts(16 * 60 + 30);

        // Without the missed states the old value is extended over the gap.
        assert_eq!(live.minute_history(now, 1).house_load, vec![2340.0]);
        assert!(!live.is_fresh(now, SignedDuration::from_mins(2)));

        // Backfill: what the recorder has for the gap.
        live.record("sensor.grid", ts(3 * 60), 463.0);
        live.record("sensor.grid", ts(16 * 60 + 20), 470.0);
        assert_eq!(live.minute_history(now, 1).house_load, vec![463.0]);
        assert!(live.is_fresh(now, SignedDuration::from_mins(2)));
    }

    #[test]
    fn recent_house_load_reflects_a_change_within_the_window() {
        let mut live = live_data();
        live.record("sensor.solar", ts(0), 0.0);
        live.record("sensor.battery", ts(0), 0.0);
        live.record("sensor.grid", ts(0), 2000.0);
        live.record("sensor.grid", ts(100), 300.0);
        let window = SignedDuration::from_secs(15);
        assert_eq!(live.recent_house_load_w(ts(115), window), Some(300.0));
        // The last complete minute (60..120) still mostly holds the old level.
        assert!(live.minute_history(ts(120), 1).house_load[0] > 1400.0);
        let partial = live.recent_house_load_w(ts(105), window).unwrap();
        assert!((partial - 1433.3).abs() < 0.1, "{partial}");
        live.record("sensor.solar", ts(110), f64::NAN);
        assert_eq!(live.recent_house_load_w(ts(130), window), None);
    }

    #[test]
    fn detector_requires_same_direction() {
        let mut d = NetLoadChangeDetector::new(&ReplanOptions::default());
        d.reset(Some(0.0));
        assert!(!d.observe(ts(0), 1000.0));
        assert!(!d.observe(ts(10), -1000.0));
        assert!(!d.observe(ts(20), -1000.0));
        assert!(d.observe(ts(25), -1000.0));
    }
}
