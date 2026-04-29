use energy_planner::{home_assistant::client::HaClient, init_tracing};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let ha_client = HaClient::new()?;
    let mut websocket = ha_client.connect_websocket().await?;

    websocket
        .subscribe_to_entities(&["sensor.net_grid_power", "sensor.solar_power_produced"])
        .await?;

    loop {
        websocket.next_state_change().await?;
    }

    Ok(())
}
