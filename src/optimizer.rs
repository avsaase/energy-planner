use anyhow::Context;
use good_lp::{
    DualValues, Expression, ProblemVariables, Solution, SolutionWithDual, SolverModel, constraint,
    default_solver, variable,
};
use jiff::{Unit, Zoned};

use crate::types::{BatteryIntent, InputData, Planning, PlanningInterval};

pub fn solve(input_data: InputData, now: Zoned) -> anyhow::Result<Planning> {
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
    let soc = problem_variables.add_vector(
        variable()
            .min(input_data.battery_parameters.min_soc_percent)
            .max(input_data.battery_parameters.max_soc_percent),
        n,
    );

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
    }

    // Subtract terminal value of remaining energy in battery (assumed at fixed value)
    // const TERMINAL_VALUE_PER_WH: f64 = 0.25 / 1000.0;
    // objective -= soc[n - 1] * input_data.battery_parameters.capacity_wh * TERMINAL_VALUE_PER_WH;

    // Create the problem
    let mut problem = problem_variables.minimise(objective).using(default_solver);

    // Constraints
    let mut power_balance_constraints = Vec::new();

    for (t, interval) in input_data.intervals.iter().enumerate() {
        let duration_hours = (&interval.end - &interval.start).total(Unit::Hour)?;

        // Power balance
        let pb_constraint = constraint!(
            interval.base_load_forecast_w + grid_export[t] + battery_charge[t]
                == interval.solar_forecast_w + grid_import[t] + battery_discharge[t]
        );
        power_balance_constraints.push(problem.add_constraint(pb_constraint.clone()));

        // SOC evolution
        if t == 0 {
            problem.add_constraint(constraint!(
                soc[0]
                    == input_data.battery_current_soc_percent
                        + (battery_charge[0]
                            * input_data.battery_parameters.charge_efficiency
                            * duration_hours
                            / input_data.battery_parameters.capacity_wh)
                        - (battery_discharge[0]
                            / input_data.battery_parameters.discharge_efficiency
                            * duration_hours
                            / input_data.battery_parameters.capacity_wh)
            ));
        } else {
            problem.add_constraint(constraint!(
                soc[t]
                    == soc[t - 1]
                        + (battery_charge[t]
                            * input_data.battery_parameters.charge_efficiency
                            * duration_hours
                            / input_data.battery_parameters.capacity_wh)
                        - (battery_discharge[t]
                            / input_data.battery_parameters.discharge_efficiency
                            * duration_hours
                            / input_data.battery_parameters.capacity_wh)
            ));
        }

        // Battery power limits
        problem.add_constraint(constraint!(
            battery_charge[t] <= input_data.battery_parameters.max_charge_power_w
        ));
        problem.add_constraint(constraint!(
            battery_discharge[t] <= input_data.battery_parameters.max_discharge_power_w
        ));
    }

    // Constraint to avoid always discharging the battery to zero at the end of the horizon
    problem.add_constraint(constraint!(
        soc[n - 1] >= input_data.battery_current_soc_percent
    ));

    // Build and solve the problem
    let mut solution = problem.solve().context("Failed to solve problem")?;

    let solution = solution.compute_dual();

    // Extract solution and build planning
    let mut intervals = Vec::new();
    for (t, interval) in input_data.intervals.iter().enumerate() {
        let duration_hours = (&interval.end - &interval.start).total(Unit::Hour)?;

        let battery_charge_w = solution.value(battery_charge[t]);
        let battery_discharge_w = solution.value(battery_discharge[t]);
        let grid_import_w = solution.value(grid_import[t]);
        let grid_export_w = solution.value(grid_export[t]);
        let battery_soc_end = solution.value(soc[t]);

        let shadow_price =
            -solution.dual(power_balance_constraints[t].clone()) / duration_hours * 1000.0; // convert from EUR/W to EUR/kWh

        let battery_intent = determine_intent(
            grid_import_w,
            grid_export_w,
            battery_charge_w,
            battery_discharge_w,
            input_data.intervals[t].electricity_price_eur_per_kwh_take,
            input_data.intervals[t].electricity_price_eur_per_kwh_feed,
            shadow_price,
            input_data.battery_parameters.cycle_cost_eur_per_wh() / 1000.0,
        );

        intervals.push(PlanningInterval {
            start: interval.start.clone(),
            end: interval.end.clone(),
            battery_charge_w,
            battery_discharge_w,
            battery_soc_end,
            grid_import_w,
            grid_export_w,
            electricity_price_eur_per_kwh_take: interval.electricity_price_eur_per_kwh_take,
            electricity_price_eur_per_kwh_feed: interval.electricity_price_eur_per_kwh_feed,
            solar_production_w: interval.solar_forecast_w,
            consumption_w: interval.base_load_forecast_w,
            shadow_price_eur_per_kwh: shadow_price,
            battery_intent,
        });
    }

    Ok(Planning {
        planned_at: now,
        intervals,
    })
}

#[allow(clippy::too_many_arguments)]
fn determine_intent(
    grid_import_w: f64,
    grid_export_w: f64,
    battery_charge_w: f64,
    battery_discharge_w: f64,
    import_price_eur_per_kwh: f64,
    export_price_eur_per_kwh: f64,
    shadow_price_eur_per_kwh: f64,
    cycle_cost_per_kwh: f64,
) -> BatteryIntent {
    const FIXED_MIN_W: f64 = 200.0;
    const ZERO_W: f64 = 10.0;

    if battery_charge_w > ZERO_W && grid_import_w > FIXED_MIN_W {
        return BatteryIntent::FixedCharge {
            power_w: battery_charge_w,
        };
    }
    if battery_discharge_w > ZERO_W && grid_export_w > FIXED_MIN_W {
        return BatteryIntent::FixedDischarge {
            power_w: battery_discharge_w,
        };
    }

    let plan_mixes_charge = battery_charge_w > ZERO_W && grid_import_w > ZERO_W;
    let plan_mixes_discharge = battery_discharge_w > ZERO_W && grid_export_w > ZERO_W;

    let charge_ok = plan_mixes_charge
        || plan_mixes_discharge
        || shadow_price_eur_per_kwh > export_price_eur_per_kwh + cycle_cost_per_kwh;
    let discharge_ok = plan_mixes_charge
        || plan_mixes_discharge
        || shadow_price_eur_per_kwh < import_price_eur_per_kwh - cycle_cost_per_kwh;

    match (charge_ok, discharge_ok) {
        (true, true) => BatteryIntent::Balance,
        (true, false) => BatteryIntent::BalanceChargeOnly,
        (false, true) => BatteryIntent::BalanceDischargeOnly,
        (false, false) => BatteryIntent::Idle,
    }
}
