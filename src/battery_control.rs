//! Applying the planned battery intent through Home Assistant scripts.

use tracing::{error, info};

use crate::{home_assistant::client::HaClient, types::BatteryIntent};

/// Battery power differences smaller than this don't matter: fixed setpoints closer than this
/// to the applied one are not resent, and the optimizer ignores scenario differences below it.
pub const POWER_TOLERANCE_W: f64 = 100.0;

/// Sends the battery intent to Home Assistant, only when it changes.
pub struct BatteryController {
    ha_client: HaClient,
    applied: Option<BatteryIntent>,
}

impl BatteryController {
    pub fn new(ha_client: HaClient) -> Self {
        Self {
            ha_client,
            applied: None,
        }
    }

    fn is_unchanged(&self, intent: &BatteryIntent) -> bool {
        use BatteryIntent::*;
        match (&self.applied, intent) {
            (Some(FixedCharge { power_w: a }), FixedCharge { power_w: b })
            | (Some(FixedDischarge { power_w: a }), FixedDischarge { power_w: b }) => {
                (a - b).abs() < POWER_TOLERANCE_W
            }
            (Some(applied), intent) => applied == intent,
            (None, _) => false,
        }
    }

    pub async fn apply(&mut self, intent: &BatteryIntent) {
        if self.is_unchanged(intent) {
            return;
        }
        if !cfg!(feature = "control-battery") {
            info!(?intent, "Battery control disabled, not applying intent");
            self.applied = Some(intent.clone());
            return;
        }
        let result = async {
            let mut ws = self.ha_client.connect_websocket().await?;
            ws.apply_battery_intent(intent).await
        }
        .await;
        match result {
            Ok(()) => self.applied = Some(intent.clone()),
            Err(e) => error!(error = format!("{e:#}"), "Failed to apply battery intent"),
        }
    }
}
