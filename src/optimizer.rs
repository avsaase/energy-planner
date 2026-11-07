use anyhow::Context;
use good_lp::{
    Expression, ProblemVariables, Solution, SolverModel, constraint, solvers::highs::highs,
    variable,
};
use jiff::{ToSpan, Unit, Zoned};

use crate::{
    battery_control::POWER_TOLERANCE_W,
    types::{
        BatteryIntent, InputData, Planning, PlanningInterval, ScenarioOutcome, SolverParameters,
    },
};

/// Keeps a replan (three solves in parallel) well within the replan debounce interval.
const SOLVE_TIME_LIMIT_SECS: f64 = 10.0;
const MIP_REL_GAP: f32 = 1e-4;

/// Optimal flows per interval (W) and SOC at the end of each interval (fraction).
#[derive(Debug, Clone)]
struct Flows {
    charge: Vec<f64>,
    discharge: Vec<f64>,
    import: Vec<f64>,
    export: Vec<f64>,
    soc: Vec<f64>,
}

/// Plans the battery over the input horizon.
///
/// The first interval is solved for the expected net load and for a high and low scenario
/// (`input_data.scenarios`). Its battery intent is the mode whose behaviour best matches what
/// the optimizer would do across those scenarios, so it stays right when the forecast is off.
pub fn solve(input_data: InputData, now: Zoned) -> anyhow::Result<Planning> {
    if input_data.intervals.is_empty() {
        anyhow::bail!("No intervals to plan for");
    }

    let first = &input_data.intervals[0];
    // House load can't go negative: without solar (at night) the low scenario never has a
    // surplus.
    let below_w = input_data
        .scenarios
        .below_w
        .min(first.house_load_forecast_w.max(0.0));
    let above_w = input_data.scenarios.above_w;
    let shifted = |shift: f64| {
        let mut input = input_data.clone();
        input.intervals[0].house_load_forecast_w += shift;
        input
    };
    let low_input = shifted(-below_w);
    let high_input = shifted(above_w);

    let (mid, low, high) = std::thread::scope(|scope| {
        let low = scope.spawn(|| build_and_solve(&low_input, &now));
        let high = scope.spawn(|| build_and_solve(&high_input, &now));
        let mid = build_and_solve(&input_data, &now);
        (
            mid,
            low.join().expect("solver thread panicked"),
            high.join().expect("solver thread panicked"),
        )
    });
    let mid = mid.context("Expected scenario")?;

    let bp = &input_data.battery_parameters;
    let net_load_mid = first.house_load_forecast_w - first.solar_forecast_w;
    let battery = |f: &Flows| f.discharge[0] - f.charge[0];
    // If a scenario fails, assume the battery would do what it does in the expected case.
    let scenario = ScenarioOutcome {
        net_load_low_w: net_load_mid - below_w,
        net_load_mid_w: net_load_mid,
        net_load_high_w: net_load_mid + above_w,
        battery_low_w: low.as_ref().map_or(battery(&mid), battery),
        battery_mid_w: battery(&mid),
        battery_high_w: high.as_ref().map_or(battery(&mid), battery),
    };
    if let Err(e) = low.and(high) {
        tracing::warn!(error = %e, "Scenario solve failed, using expected case");
    }

    let mut intervals = Vec::new();
    for (t, interval) in input_data.intervals.iter().enumerate() {
        let battery_intent = if t == 0 {
            choose_mode(
                &scenario,
                mid.charge[0],
                mid.discharge[0],
                bp.max_charge_power_w,
                bp.max_discharge_power_w,
            )
        } else {
            determine_intent(
                mid.import[t],
                mid.export[t],
                mid.charge[t],
                mid.discharge[t],
                bp.max_charge_power_w,
                bp.max_discharge_power_w,
            )
        };

        intervals.push(PlanningInterval {
            start: interval.start.clone(),
            end: interval.end.clone(),
            battery_charge_w: mid.charge[t],
            battery_discharge_w: mid.discharge[t],
            battery_soc_end: mid.soc[t],
            grid_import_w: mid.import[t],
            grid_export_w: mid.export[t],
            electricity_price_eur_per_kwh_take: interval.electricity_price_eur_per_kwh_take,
            electricity_price_eur_per_kwh_feed: interval.electricity_price_eur_per_kwh_feed,
            electricity_price_is_forecast: interval.electricity_price_is_forecast,
            solar_production_w: interval.solar_forecast_w,
            consumption_w: interval.house_load_forecast_w,
            battery_intent,
            scenario: (t == 0).then_some(scenario),
        });
    }

    Ok(Planning {
        planned_at: now,
        initial_soc: Some(input_data.battery_current_soc_percent),
        intervals,
    })
}

/// Picks the battery mode whose response to net load best fits the optimal battery power in
/// the three scenarios (least squares). On ties the simpler mode (earlier in the list) wins.
fn choose_mode(
    s: &ScenarioOutcome,
    mid_charge_w: f64,
    mid_discharge_w: f64,
    max_charge_w: f64,
    max_discharge_w: f64,
) -> BatteryIntent {
    const ZERO_W: f64 = 1.0;
    let clip = |b: f64| b.clamp(-max_charge_w, max_discharge_w);

    let mut candidates = vec![
        BatteryIntent::Idle,
        BatteryIntent::Balance,
        BatteryIntent::BalanceChargeOnly,
        BatteryIntent::BalanceDischargeOnly,
    ];
    if mid_charge_w > ZERO_W {
        candidates.push(BatteryIntent::FixedCharge {
            power_w: mid_charge_w,
        });
    }
    if mid_discharge_w > ZERO_W {
        candidates.push(BatteryIntent::FixedDischarge {
            power_w: mid_discharge_w,
        });
    }

    // Battery power (+ discharge) a mode produces at net load `n`.
    let response = |intent: &BatteryIntent, n: f64| match intent {
        BatteryIntent::Balance => clip(n),
        BatteryIntent::BalanceChargeOnly => clip(n.min(0.0)),
        BatteryIntent::BalanceDischargeOnly => clip(n.max(0.0)),
        BatteryIntent::FixedCharge { power_w } => -power_w,
        BatteryIntent::FixedDischarge { power_w } => *power_w,
        BatteryIntent::Idle | BatteryIntent::Other => 0.0,
    };
    // Scenario differences below the controller's tolerance are solver noise (the MIP stops at
    // a relative gap), not a reason to follow net load.
    let denoise = |b: f64| {
        if (b - s.battery_mid_w).abs() < POWER_TOLERANCE_W {
            s.battery_mid_w
        } else {
            b
        }
    };
    let points = [
        (s.net_load_low_w, denoise(s.battery_low_w)),
        (s.net_load_mid_w, s.battery_mid_w),
        (s.net_load_high_w, denoise(s.battery_high_w)),
    ];

    let mut best: Option<(BatteryIntent, f64)> = None;
    for intent in candidates {
        let error: f64 = points
            .iter()
            .map(|&(n, b)| (response(&intent, n) - b).powi(2))
            .sum();
        // A later (more specific) mode must be clearly better, not just equal within rounding.
        if best
            .as_ref()
            .is_none_or(|(_, best_error)| error < best_error - 3.0 * ZERO_W * ZERO_W)
        {
            best = Some((intent, error));
        }
    }
    best.map(|(intent, _)| intent)
        .unwrap_or(BatteryIntent::Idle)
}

fn build_and_solve(input_data: &InputData, now: &Zoned) -> anyhow::Result<Flows> {
    if input_data.intervals.is_empty() {
        anyhow::bail!("No intervals to plan for");
    }

    let n = input_data.intervals.len();

    let mut problem_variables = ProblemVariables::new();

    // Continuous decision variables for each time interval
    let battery_charge = problem_variables.add_vector(
        variable()
            .min(0.0)
            .max(input_data.battery_parameters.max_charge_power_w),
        n,
    );
    let battery_discharge = problem_variables.add_vector(
        variable()
            .min(0.0)
            .max(input_data.battery_parameters.max_discharge_power_w),
        n,
    );
    let grid_import = problem_variables.add_vector(variable().min(0.0), n);
    let grid_export = problem_variables.add_vector(variable().min(0.0), n);
    let battery_charge_active = problem_variables.add_vector(variable().binary(), n);
    let battery_discharge_active = problem_variables.add_vector(variable().binary(), n);

    // Stretch the SOC limits to the current SOC to avoid infeasibility or battery actions to fix
    // current exceedance of the limits.
    let current_soc = input_data.battery_current_soc_percent;
    let min_soc = input_data
        .battery_parameters
        .min_soc_percent
        .min(current_soc);
    let max_soc = input_data
        .battery_parameters
        .max_soc_percent
        .max(current_soc);
    let soc = problem_variables.add_vector(variable().min(min_soc).max(max_soc), n);

    // Objective: minimize electricity cost
    let mut objective = Expression::default();
    for (t, interval) in input_data.intervals.iter().enumerate() {
        let duration_hours = (&interval.end - &interval.start).total(Unit::Hour)?;
        let import_price_per_wh = interval.electricity_price_eur_per_kwh_take / 1000.0;
        let export_price_per_wh = interval.electricity_price_eur_per_kwh_feed / 1000.0;

        // Cost of grid import/export
        objective += (grid_import[t] * import_price_per_wh - grid_export[t] * export_price_per_wh)
            * duration_hours;

        // Cycle costs for battery usage
        objective += (battery_charge[t] + battery_discharge[t])
            * input_data.battery_parameters.cycle_cost_eur_per_wh()
            * duration_hours;

        // Small incentive for charging early to account for uncertain forecasts. Per Wh charged,
        // so scaled with the interval length like every other term.
        objective -= battery_charge[t]
            * early_charging_bonus(now, &interval.start, &input_data.solver_parameters)
            * duration_hours
    }

    // Create the problem
    let mut problem = problem_variables
        .minimise(objective)
        .using(highs)
        .set_time_limit(SOLVE_TIME_LIMIT_SECS)
        .set_mip_rel_gap(MIP_REL_GAP)
        .map_err(|e| anyhow::anyhow!("Invalid MIP gap: {e:?}"))?;

    // Constraints

    for (t, interval) in input_data.intervals.iter().enumerate() {
        let duration_hours = (&interval.end - &interval.start).total(Unit::Hour)?;

        // Power balance
        problem.add_constraint(constraint!(
            interval.house_load_forecast_w + grid_export[t] + battery_charge[t]
                == interval.solar_forecast_w + grid_import[t] + battery_discharge[t]
        ));

        let bp = &input_data.battery_parameters;
        let charge_gain = bp.charge_conversion_efficiency * duration_hours / bp.capacity_wh;
        let discharge_loss = duration_hours / (bp.discharge_conversion_efficiency * bp.capacity_wh);
        let charge_fixed_loss = bp.charge_fixed_power_loss_w * duration_hours / bp.capacity_wh;
        let discharge_fixed_loss =
            bp.discharge_fixed_power_loss_w * duration_hours / bp.capacity_wh;
        let idle_loss = bp.idle_power_loss_w * duration_hours / bp.capacity_wh;

        // SOC evolution
        let soc_prev = if t == 0 {
            Expression::from(input_data.battery_current_soc_percent)
        } else {
            soc[t - 1].into()
        };
        problem.add_constraint(constraint!(
            soc[t]
                == soc_prev + battery_charge[t] * charge_gain
                    - battery_charge_active[t] * charge_fixed_loss
                    - battery_discharge[t] * discharge_loss
                    - battery_discharge_active[t] * discharge_fixed_loss
                    - idle_loss
        ));

        // Battery power limits
        problem.add_constraint(constraint!(
            battery_charge[t] <= input_data.battery_parameters.max_charge_power_w
        ));
        problem.add_constraint(constraint!(
            battery_discharge[t] <= input_data.battery_parameters.max_discharge_power_w
        ));

        // Prevent simultaneous battery charging and discharging.
        problem.add_constraint(constraint!(
            battery_charge[t]
                <= input_data.battery_parameters.max_charge_power_w * battery_charge_active[t]
        ));
        problem.add_constraint(constraint!(
            battery_discharge[t]
                <= input_data.battery_parameters.max_discharge_power_w
                    * battery_discharge_active[t]
        ));
        problem.add_constraint(constraint!(
            battery_charge_active[t] + battery_discharge_active[t] <= 1.0
        ));
    }

    // Constraint to avoid always discharging the battery to zero at the end of the horizon
    problem.add_constraint(constraint!(
        soc[n - 1] >= input_data.battery_current_soc_percent
    ));

    // Build and solve the problem
    let solution = problem.solve().context("Failed to solve problem")?;

    let values = |vars: &[good_lp::Variable]| vars.iter().map(|v| solution.value(*v)).collect();
    Ok(Flows {
        charge: values(&battery_charge),
        discharge: values(&battery_discharge),
        import: values(&grid_import),
        export: values(&grid_export),
        soc: values(&soc),
    })
}

/// Computes a small incentive for charging early to account for uncertain forecasts.
///
/// The bonus decays linearly to zero over the next 12 hours
fn early_charging_bonus(
    now: &Zoned,
    current_interval_start: &Zoned,
    solver_parameters: &SolverParameters,
) -> f64 {
    let zero_bonus_time = now + (solver_parameters.early_charge_bonus_decay_hours as i16).hours();
    let hours_till_zero_bonus = current_interval_start
        .until(&zero_bonus_time)
        .expect("Valid span")
        .total(Unit::Hour)
        .expect("Valid duration");

    let early_charge_bonus_per_wh = solver_parameters.early_charge_bonus_per_kwh / 1000.0;

    if hours_till_zero_bonus <= 0.0 {
        0.0
    } else {
        early_charge_bonus_per_wh
            * (hours_till_zero_bonus / solver_parameters.early_charge_bonus_decay_hours as f64)
    }
}

/// Labels a planned (later) interval with the battery mode that reproduces its planned flows.
/// The first interval gets its mode from the scenarios instead (`choose_mode`).
///
/// A balance mode only fits when the battery covers *all* of the net load (or surplus) or runs
/// at its limit; planning less than that (e.g. spreading the last stored energy over the
/// expensive hours) is a fixed setpoint.
fn determine_intent(
    grid_import_w: f64,
    grid_export_w: f64,
    battery_charge_w: f64,
    battery_discharge_w: f64,
    battery_charge_limit_w: f64,
    battery_discharge_limit_w: f64,
) -> BatteryIntent {
    const ZERO_W: f64 = 1.0;

    let is_importing = grid_import_w > ZERO_W;
    let is_exporting = grid_export_w > ZERO_W;

    if battery_discharge_w > ZERO_W {
        let covers_net_load = !is_importing && !is_exporting;
        let at_limit = battery_discharge_w >= battery_discharge_limit_w - ZERO_W && !is_exporting;
        return if covers_net_load || at_limit {
            BatteryIntent::BalanceDischargeOnly
        } else {
            BatteryIntent::FixedDischarge {
                power_w: battery_discharge_w,
            }
        };
    }

    if battery_charge_w > ZERO_W {
        let absorbs_surplus = !is_importing && !is_exporting;
        let at_limit = battery_charge_w >= battery_charge_limit_w - ZERO_W && !is_importing;
        return if absorbs_surplus || at_limit {
            BatteryIntent::BalanceChargeOnly
        } else {
            BatteryIntent::FixedCharge {
                power_w: battery_charge_w,
            }
        };
    }

    BatteryIntent::Idle
}

#[cfg(test)]
mod tests {
    use jiff::{ToSpan, civil::date, tz::TimeZone};

    use super::*;
    use crate::types::{
        BatteryParameters, ElectricityPriceParameters, InputInterval, NetLoadScenarios,
    };

    fn scenario(points: [(f64, f64); 3]) -> ScenarioOutcome {
        ScenarioOutcome {
            net_load_low_w: points[0].0,
            battery_low_w: points[0].1,
            net_load_mid_w: points[1].0,
            battery_mid_w: points[1].1,
            net_load_high_w: points[2].0,
            battery_high_w: points[2].1,
        }
    }

    fn mode(points: [(f64, f64); 3]) -> BatteryIntent {
        let mid = points[1].1;
        choose_mode(
            &scenario(points),
            (-mid).max(0.0),
            mid.max(0.0),
            2500.0,
            2500.0,
        )
    }

    #[test]
    fn follows_net_load_both_ways_is_balance() {
        // Oven on earlier than forecast while the plan expected a surplus.
        assert_eq!(
            mode([(-3000.0, -2500.0), (-1000.0, -1000.0), (1000.0, 1000.0)]),
            BatteryIntent::Balance
        );
    }

    #[test]
    fn covers_only_deficits_is_discharge_only() {
        assert_eq!(
            mode([(-1000.0, 0.0), (500.0, 500.0), (1500.0, 1500.0)]),
            BatteryIntent::BalanceDischargeOnly
        );
    }

    #[test]
    fn constant_partial_charge_is_fixed() {
        // The plan deliberately charges only 1000 W whatever the surplus.
        assert_eq!(
            mode([(-3000.0, -1000.0), (-2000.0, -1000.0), (-1000.0, -1000.0)]),
            BatteryIntent::FixedCharge { power_w: 1000.0 }
        );
    }

    #[test]
    fn solver_noise_between_scenarios_is_ignored() {
        // The low scenario has a small surplus and charges a few tens of W the mid plan
        // doesn't: without the tolerance, charge-only would fit best.
        assert_eq!(
            mode([(-50.0, -89.0), (250.0, 0.0), (550.0, 0.0)]),
            BatteryIntent::Idle
        );
    }

    #[test]
    fn no_battery_use_is_idle() {
        assert_eq!(
            mode([(-300.0, 0.0), (0.0, 0.0), (300.0, 0.0)]),
            BatteryIntent::Idle
        );
    }

    /// 16 quarter hours. `prices` are (import, export) per interval.
    fn input(soc: f64, house_load_w: f64, solar: &[f64], prices: &[(f64, f64)]) -> InputData {
        let tz = TimeZone::get("Europe/Amsterdam").unwrap();
        let start = date(2026, 6, 1).at(0, 0, 0, 0).to_zoned(tz).unwrap();
        let intervals = prices
            .iter()
            .zip(solar)
            .enumerate()
            .map(|(i, (&(take, feed), &solar))| InputInterval {
                start: &start + (15 * i as i64).minutes(),
                end: &start + (15 * (i as i64 + 1)).minutes(),
                house_load_forecast_w: house_load_w,
                solar_forecast_w: solar,
                electricity_price_eur_per_kwh_take: take,
                electricity_price_eur_per_kwh_feed: feed,
                electricity_price_is_forecast: false,
            })
            .collect();
        InputData {
            battery_parameters: BatteryParameters {
                capacity_wh: 5120.0,
                lifetime_cycles: 6000,
                purchase_cost_eur: 1345.0,
                max_discharge_power_w: 2500.0,
                max_charge_power_w: 2500.0,
                charge_conversion_efficiency: 0.9,
                charge_fixed_power_loss_w: 10.0,
                discharge_conversion_efficiency: 0.9,
                discharge_fixed_power_loss_w: 10.0,
                idle_power_loss_w: 5.0,
                min_soc_percent: 0.12,
                max_soc_percent: 1.0,
            },
            intervals,
            electricity_price_parameters: ElectricityPriceParameters {
                supplier_cost_import_eur_per_kwh: 0.0,
                supplier_cost_export_eur_per_kwh: 0.0,
                energy_tax_import_eur_per_kwh: 0.0,
                energy_tax_export_eur_per_kwh: 0.0,
                vat_import: 0.0,
                vat_export: 0.0,
            },
            solver_parameters: SolverParameters {
                early_charge_bonus_per_kwh: 0.0,
                early_charge_bonus_decay_hours: 12,
            },
            battery_current_soc_percent: soc,
            scenarios: NetLoadScenarios {
                below_w: 1000.0,
                above_w: 1000.0,
            },
        }
    }

    fn first_intent(input: InputData) -> BatteryIntent {
        let now = input.intervals[0].start.clone();
        solve(input, now).unwrap().intervals[0]
            .battery_intent
            .clone()
    }

    /// `first` for the first hour, `rest` for the following three.
    fn prices(first: (f64, f64), rest: (f64, f64)) -> Vec<(f64, f64)> {
        let mut p = vec![first; 4];
        p.extend([rest; 12]);
        p
    }

    #[test]
    fn flat_prices_without_export_discount_leave_battery_idle() {
        let input = input(0.5, 500.0, &[0.0; 16], &[(0.25, 0.25); 16]);
        assert_eq!(first_intent(input), BatteryIntent::Idle);
    }

    #[test]
    fn export_discount_stores_unexpected_surplus_only() {
        // Storing surplus avoids importing later at a higher price than exporting pays now,
        // but discharging now gains nothing over discharging later. Solar covers most of the
        // house load, so less load than expected means a surplus.
        let input = input(0.5, 1200.0, &[1000.0; 16], &[(0.25, 0.05); 16]);
        assert_eq!(first_intent(input), BatteryIntent::BalanceChargeOnly);
    }

    #[test]
    fn price_peak_now_covers_load_from_battery() {
        let prices = prices((0.60, 0.05), (0.30, 0.05));
        let intent = first_intent(input(0.9, 500.0, &[0.0; 16], &prices));
        assert!(
            matches!(
                intent,
                BatteryIntent::Balance | BatteryIntent::BalanceDischargeOnly
            ),
            "{intent:?}"
        );
    }

    #[test]
    fn midday_surplus_before_expensive_evening_charges_from_surplus() {
        let mut solar = vec![2000.0; 4];
        solar.extend([0.0; 12]);
        // Grid charging now (0.35) is not worth it for 0.40 later, storing surplus (0.05) is.
        let prices = prices((0.35, 0.05), (0.40, 0.05));
        let intent = first_intent(input(0.2, 500.0, &solar, &prices));
        assert!(
            matches!(
                intent,
                BatteryIntent::Balance | BatteryIntent::BalanceChargeOnly
            ),
            "{intent:?}"
        );
    }

    #[test]
    fn short_first_interval_does_not_buy_expensive_energy_for_the_bonus() {
        // A replan at 15:47: a 3-minute first interval at the price peak, slightly lower prices
        // afterwards, and the early charge bonus switched on (as in the add-on options). Without
        // idle and fixed losses the plan needs no top-up, so only the bonus could make charging
        // now attractive: it was not scaled by the interval length, so 20x too large here.
        let prices = prices((0.539, 0.357), (0.52, 0.34));
        let mut input = input(0.555, 300.0, &[0.0; 16], &prices);
        input.solver_parameters.early_charge_bonus_per_kwh = 0.01;
        let battery = &mut input.battery_parameters;
        battery.idle_power_loss_w = 0.0;
        battery.charge_fixed_power_loss_w = 0.0;
        battery.discharge_fixed_power_loss_w = 0.0;
        let start = input.intervals[0].start.clone();
        for (i, interval) in input.intervals.iter_mut().enumerate() {
            let offset = if i == 0 { 0 } else { 3 + 15 * (i as i64 - 1) };
            interval.start = &start + offset.minutes();
            interval.end = &start + (3 + 15 * i as i64).minutes();
        }
        let intent = first_intent(input);
        assert!(
            !matches!(intent, BatteryIntent::FixedCharge { .. }),
            "{intent:?}"
        );
    }

    #[test]
    fn low_scenario_never_has_negative_house_load() {
        // Night: 289 W house load, no solar, a 1000 W scenario spread.
        let input = input(0.5, 289.0, &[0.0; 16], &prices((0.41, 0.25), (0.40, 0.24)));
        let now = input.intervals[0].start.clone();
        let scenario = solve(input, now).unwrap().intervals[0].scenario.unwrap();
        assert!(scenario.net_load_low_w.abs() < 1e-9, "{scenario:?}");
        assert!(
            (scenario.net_load_high_w - 1289.0).abs() < 1e-9,
            "{scenario:?}"
        );
    }

    /// `determine_intent(import, export, charge, discharge, charge limit, discharge limit)`
    fn label(import: f64, export: f64, charge: f64, discharge: f64) -> BatteryIntent {
        determine_intent(import, export, charge, discharge, 2500.0, 2500.0)
    }

    #[test]
    fn later_intervals_are_labelled_with_the_mode_that_reproduces_the_plan() {
        use BatteryIntent::*;
        // Battery covers the whole house load / takes the whole surplus.
        assert_eq!(label(0.0, 0.0, 0.0, 731.0), BalanceDischargeOnly);
        assert_eq!(label(0.0, 0.0, 900.0, 0.0), BalanceChargeOnly);
        // At its limit, balancing does the same as the plan.
        assert_eq!(label(500.0, 0.0, 0.0, 2500.0), BalanceDischargeOnly);
        assert_eq!(label(0.0, 500.0, 2500.0, 0.0), BalanceChargeOnly);
        // Partial: the 17:00 case (530 W of a 731 W load), and the reverse case that was Idle.
        assert_eq!(
            label(201.0, 0.0, 0.0, 530.0),
            FixedDischarge { power_w: 530.0 }
        );
        assert_eq!(
            label(600.0, 0.0, 0.0, 100.0),
            FixedDischarge { power_w: 100.0 }
        );
        assert_eq!(
            label(0.0, 400.0, 600.0, 0.0),
            FixedCharge { power_w: 600.0 }
        );
        // Grid arbitrage.
        assert_eq!(
            label(1300.0, 0.0, 1000.0, 0.0),
            FixedCharge { power_w: 1000.0 }
        );
        assert_eq!(
            label(0.0, 1500.0, 0.0, 2000.0),
            FixedDischarge { power_w: 2000.0 }
        );
        assert_eq!(label(400.0, 0.0, 0.0, 0.0), Idle);
    }

    #[test]
    fn very_cheap_now_charges_from_grid() {
        let prices = prices((0.00, 0.00), (0.50, 0.05));
        let intent = first_intent(input(0.2, 300.0, &[0.0; 16], &prices));
        assert!(
            matches!(intent, BatteryIntent::FixedCharge { .. }),
            "{intent:?}"
        );
    }
}
