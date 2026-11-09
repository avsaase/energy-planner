use jiff::Zoned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolarForecast {
    pub start: Zoned,
    pub end: Zoned,
    pub forecast_w: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolarForecasts {
    pub updated_at: Zoned,
    pub forecasts: Vec<SolarForecast>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumptionForecast {
    pub start: Zoned,
    pub end: Zoned,
    pub forecast_w: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElectricityPrice {
    pub start: Zoned,
    pub end: Zoned,
    pub price_per_kwh: f64,
    pub is_forecast: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElectricityPrices {
    pub updated_at: Zoned,
    pub prices: Vec<ElectricityPrice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputData {
    pub battery_parameters: BatteryParameters,
    pub intervals: Vec<InputInterval>,
    pub electricity_price_parameters: ElectricityPriceParameters,
    pub solver_parameters: SolverParameters,
    pub battery_current_soc_percent: f64,
    /// Uncertainty of the first interval's net load, used to choose the battery mode.
    pub scenarios: NetLoadScenarios,
}

/// High and low net-load scenarios for the first interval, relative to the expected value.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct NetLoadScenarios {
    /// Expected minus low scenario (≥ 0).
    pub below_w: f64,
    /// High scenario minus expected (≥ 0).
    pub above_w: f64,
}

/// What the optimizer would do with the battery in the first interval under each scenario.
/// Battery power is + discharge, − charge.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ScenarioOutcome {
    pub net_load_low_w: f64,
    pub net_load_mid_w: f64,
    pub net_load_high_w: f64,
    pub battery_low_w: f64,
    pub battery_mid_w: f64,
    pub battery_high_w: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ElectricityPriceParameters {
    pub supplier_cost_import_eur_per_kwh: f64,
    pub supplier_cost_export_eur_per_kwh: f64,
    pub energy_tax_import_eur_per_kwh: f64,
    pub energy_tax_export_eur_per_kwh: f64,
    pub vat_import: f64,
    pub vat_export: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputInterval {
    pub start: Zoned,
    pub end: Zoned,
    pub solar_forecast_w: f64,
    /// House load `L`: everything consumed, excluding battery and PV.
    pub house_load_forecast_w: f64,
    pub electricity_price_eur_per_kwh_take: f64,
    pub electricity_price_eur_per_kwh_feed: f64,
    pub electricity_price_is_forecast: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatteryParameters {
    pub capacity_wh: f64,
    pub lifetime_cycles: u64,
    pub purchase_cost_eur: f64,
    pub max_discharge_power_w: f64,
    pub max_charge_power_w: f64,
    pub charge_conversion_efficiency: f64,
    pub charge_fixed_power_loss_w: f64,
    pub discharge_conversion_efficiency: f64,
    pub discharge_fixed_power_loss_w: f64,
    pub idle_power_loss_w: f64,
    pub min_soc_percent: f64,
    pub max_soc_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolverParameters {
    pub early_charge_bonus_per_kwh: f64,
    pub early_charge_bonus_decay_hours: u8,
    #[serde(default)]
    pub battery_export_penalty_per_kwh: f64,
}

impl BatteryParameters {
    pub fn cycle_cost_eur_per_wh(&self) -> f64 {
        // Divide by 2 to get price per Wh charged or discharged
        self.purchase_cost_eur / (self.capacity_wh * self.lifetime_cycles as f64) / 2.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Planning {
    pub planned_at: Zoned,
    /// SOC (fraction) at `planned_at`.
    #[serde(default)]
    pub initial_soc: Option<f64>,
    pub intervals: Vec<PlanningInterval>,
}

impl Planning {
    /// Planned SOC at `t`, interpolated linearly within the interval containing it.
    pub fn planned_soc_at(&self, t: &Zoned) -> Option<f64> {
        let index = self
            .intervals
            .iter()
            .position(|i| &i.start <= t && *t < i.end)?;
        let interval = &self.intervals[index];
        let soc_start = match index {
            0 => self.initial_soc?,
            _ => self.intervals[index - 1].battery_soc_end,
        };
        let total = (&interval.end - &interval.start)
            .total(jiff::Unit::Second)
            .ok()?;
        let elapsed = (t - &interval.start).total(jiff::Unit::Second).ok()?;
        let w = if total > 0.0 { elapsed / total } else { 1.0 };
        Some(soc_start + (interval.battery_soc_end - soc_start) * w)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanningInterval {
    pub start: Zoned,
    pub end: Zoned,
    pub battery_charge_w: f64,
    pub battery_discharge_w: f64,
    pub battery_soc_end: f64,
    pub grid_import_w: f64,
    pub grid_export_w: f64,
    pub electricity_price_eur_per_kwh_take: f64,
    pub electricity_price_eur_per_kwh_feed: f64,
    pub electricity_price_is_forecast: bool,
    pub solar_production_w: f64,
    /// House load `L`.
    pub consumption_w: f64,
    pub battery_intent: BatteryIntent,
    /// Only for the first interval: the scenarios the battery intent was chosen from.
    #[serde(default)]
    pub scenario: Option<ScenarioOutcome>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BatteryIntent {
    Idle,
    Balance,
    BalanceChargeOnly,
    BalanceDischargeOnly,
    FixedCharge { power_w: f64 },
    FixedDischarge { power_w: f64 },
    Other,
}

pub enum PlanningState {
    NotPlanned,
    PlanningInProgress,
    Planned(Planning),
}
