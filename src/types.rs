use jiff::Zoned;
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub struct SolarForecast {
    pub start: Zoned,
    pub end: Zoned,
    pub forecast_w: f64,
}

#[derive(Debug, Deserialize)]
pub struct ElectricityPrice {
    pub start: Zoned,
    pub end: Zoned,
    pub price_per_kwh: f64,
}

#[derive(Debug, Clone)]
pub struct InputData {
    pub battery_parameters: BatteryParameters,
    pub intervals: Vec<InputInterval>,
    pub electricity_price_parameters: ElectricityPriceParameters,
    pub battery_current_soc_percent: f64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ElectricityPriceParameters {
    pub supplier_cost_import_eur_per_kwh: f64,
    pub supplier_cost_export_eur_per_kwh: f64,
    pub energy_tax_import_eur_per_kwh: f64,
    pub energy_tax_export_eur_per_kwh: f64,
    pub vat_import: f64,
    pub vat_export: f64,
}

#[derive(Debug, Clone)]
pub struct InputInterval {
    pub start: Zoned,
    pub end: Zoned,
    pub solar_forecast_w: f64,
    pub base_load_forecast_w: f64,
    pub electricity_price_eur_per_kwh_take: f64,
    pub electricity_price_eur_per_kwh_feed: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BatteryParameters {
    pub capacity_wh: f64,
    pub lifetime_cycles: u64,
    pub purchase_cost_eur: f64,
    pub max_discharge_power_w: f64,
    pub max_charge_power_w: f64,
    pub charge_efficiency: f64,
    pub discharge_efficiency: f64,
    pub min_soc_percent: f64,
    pub max_soc_percent: f64,
}

impl BatteryParameters {
    pub fn cycle_cost_eur_per_wh(&self) -> f64 {
        self.purchase_cost_eur / (self.capacity_wh * self.lifetime_cycles as f64)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Planning {
    pub planned_at: Zoned,
    pub intervals: Vec<PlanningInterval>,
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
    pub solar_production_w: f64,
    pub consumption_w: f64,
    pub shadow_price_eur_per_kwh: f64,
    pub battery_intent: BatteryIntent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
