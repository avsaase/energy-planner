use good_lp::{
    Expression, ProblemVariables, Solution, SolverModel, constraint, default_solver, variable,
};
use jiff::{Unit, Zoned};

use crate::types::{InputData, Planning, PlanningInterval};

const CYCLE_COST_PER_WH: f64 = 1200.0 / (6000.0 * 5.12 * 0.9) / 1000.0; // 1200 EUR for 6000 full cycles at 5.12 kWh capacity, converted to EUR/Wh
const IMPORT_EXPORT_PENALTY_PER_WH: f64 = 0.05 / 1000.0;

pub fn solve(input_data: InputData, now: Zoned) -> anyhow::Result<Planning> {
    let n = input_data.intervals.len();

    let mut problem = ProblemVariables::new();

    // Continuous decision variables for each time interval
    let battery_charge = problem.add_vector(
        variable()
            .min(0.0)
            .max(input_data.battery_parameters.max_charge_power_w),
        n,
    );
    let battery_discharge = problem.add_vector(
        variable()
            .min(0.0)
            .max(input_data.battery_parameters.max_discharge_power_w),
        n,
    );
    let grid_import = problem.add_vector(variable().min(0.0), n);
    let grid_export = problem.add_vector(variable().min(0.0), n);
    let soc = problem.add_vector(
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
        objective +=
            (battery_charge[t] + battery_discharge[t]) * CYCLE_COST_PER_WH * duration_hours;

        // Penalty for grid import/export to encourage self-consumption
        objective +=
            (grid_import[t] + grid_export[t]) * IMPORT_EXPORT_PENALTY_PER_WH * duration_hours;
    }

    // Subtract terminal value of remaining energy in battery (assumed at fixed value)
    const TERMINAL_VALUE_PER_WH: f64 = 0.22 / 1000.0;
    objective -= soc[n - 1] * input_data.battery_parameters.capacity_wh * TERMINAL_VALUE_PER_WH;

    // Constraints
    let mut constraints = Vec::new();

    for (t, interval) in input_data.intervals.iter().enumerate() {
        let duration_hours = (&interval.end - &interval.start).total(Unit::Hour)?;

        // Power balance
        constraints.push(constraint!(
            interval.solar_forecast_w + grid_import[t] + battery_discharge[t]
                == interval.base_load_forecast_w + grid_export[t] + battery_charge[t]
        ));

        // SOC evolution
        if t == 0 {
            constraints.push(constraint!(
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
            constraints.push(constraint!(
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
        constraints.push(constraint!(
            battery_charge[t] <= input_data.battery_parameters.max_charge_power_w
        ));
        constraints.push(constraint!(
            battery_discharge[t] <= input_data.battery_parameters.max_discharge_power_w
        ));
    }

    // Build and solve the problem
    let formulation = problem.minimise(objective).using(default_solver);
    let solution = formulation.with_all(constraints).solve()?;

    // Extract solution and build planning
    let mut intervals = Vec::new();
    for (t, interval) in input_data.intervals.iter().enumerate() {
        let battery_charge_power_w = solution.value(battery_charge[t]);
        let battery_discharge_power_w = solution.value(battery_discharge[t]);
        let grid_import_w = solution.value(grid_import[t]);
        let grid_export_w = solution.value(grid_export[t]);
        let battery_soc_end = solution.value(soc[t]);

        intervals.push(PlanningInterval {
            start: interval.start.clone(),
            end: interval.end.clone(),
            battery_charge_power_w,
            battery_discharge_power_w,
            battery_soc_end,
            grid_import_w,
            grid_export_w,
            electricity_price_eur_per_kwh_take: interval.electricity_price_eur_per_kwh_take,
            electricity_price_eur_per_kwh_feed: interval.electricity_price_eur_per_kwh_feed,
            solar_production_w: interval.solar_forecast_w,
            consumption_w: interval.base_load_forecast_w,
        });
    }

    Ok(Planning {
        planned_at: now,
        intervals,
    })
}
