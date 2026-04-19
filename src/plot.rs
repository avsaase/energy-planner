use crate::types::{BatteryIntent, Planning};
use plotly::{
    Bar, Configuration, Plot, Scatter,
    common::{Anchor, Line, LineShape, Marker, Mode, Orientation, TickMode},
    configuration::DisplayModeBar,
    layout::{Axis, AxisType, BarMode, Layout, Legend, Margin},
};

pub fn generate_plot(planning: &Planning) -> String {
    let labels: Vec<String> = planning
        .intervals
        .iter()
        .map(|interval| interval.start.time().strftime("%H:%M").to_string())
        .collect();
    let soc_labels: Vec<String> = planning
        .intervals
        .iter()
        .map(|interval| interval.end.time().strftime("%H:%M").to_string())
        .collect();
    let x_values: Vec<f64> = (0..labels.len()).map(|index| index as f64).collect();

    let interval_labels: Vec<String> = planning
        .intervals
        .iter()
        .map(|interval| {
            format!(
                "{}-{}",
                interval.start.time().strftime("%H:%M"),
                interval.end.time().strftime("%H:%M")
            )
        })
        .collect();
    let mut import_price_step_values: Vec<f64> = planning
        .intervals
        .iter()
        .map(|interval| interval.electricity_price_eur_per_kwh_take)
        .collect();
    if let Some(last_value) = import_price_step_values.last().copied() {
        import_price_step_values.push(last_value);
    }
    let mut export_price_step_values: Vec<f64> = planning
        .intervals
        .iter()
        .map(|interval| interval.electricity_price_eur_per_kwh_feed)
        .collect();
    if let Some(last_value) = export_price_step_values.last().copied() {
        export_price_step_values.push(last_value);
    }
    let mut shadow_price_step_values: Vec<f64> = planning
        .intervals
        .iter()
        .map(|interval| interval.shadow_price_eur_per_kwh)
        .collect();
    if let Some(last_value) = shadow_price_step_values.last().copied() {
        shadow_price_step_values.push(last_value);
    }
    let power_hover_template = "%{hovertext}: %{y:.0f}W<extra></extra>";
    let soc_hover_template = "%{hovertext}: %{y:.1f}%<extra></extra>";
    let price_hover_template = "%{hovertext}: %{y:.4f} EUR/kWh<extra></extra>";
    let intent_hover_template = "%{hovertext}<extra></extra>";

    let (intent_labels, intent_colors) = planning
        .intervals
        .iter()
        .zip(interval_labels.iter())
        .map(|(interval, label)| {
            (
                format!("{label}: {}", intent_label(&interval.battery_intent)),
                intent_color(&interval.battery_intent),
            )
        })
        .unzip();

    let mut html_sections = Vec::new();

    let mut intent_plot = Plot::new();
    intent_plot.add_trace(
        Bar::new(x_values.clone(), vec![1.0; planning.intervals.len()])
            .offset(0.1)
            .width(0.8)
            .name("Battery intent")
            .show_legend(false)
            .marker(Marker::new().color_array(intent_colors))
            .hover_text_array(intent_labels)
            .hover_template(intent_hover_template),
    );
    intent_plot.set_layout(
        base_layout("Battery intent", "", &labels)
            .height(130)
            .margin(Margin::new().left(72).right(24).top(30).bottom(85))
            .y_axis(
                Axis::new()
                    .range(vec![0.0, 1.0])
                    .show_grid(false)
                    .show_tick_labels(false)
                    .zero_line(false)
                    .title(""),
            ),
    );
    intent_plot.set_configuration(base_configuration());
    html_sections.push(intent_plot.to_inline_html(Some("planning-plot-battery-intent")));

    let mut grid_plot = Plot::new();
    grid_plot.add_trace(
        Bar::new(
            x_values.clone(),
            planning
                .intervals
                .iter()
                .map(|interval| interval.grid_import_w)
                .collect(),
        )
        .offset(0.1)
        .width(0.8)
        .name("Grid import")
        .hover_text_array(interval_labels.clone())
        .hover_template(power_hover_template),
    );
    grid_plot.add_trace(
        Bar::new(
            x_values.clone(),
            planning
                .intervals
                .iter()
                .map(|interval| -interval.grid_export_w)
                .collect(),
        )
        .offset(0.1)
        .width(0.8)
        .name("Grid export")
        .hover_text_array(interval_labels.clone())
        .hover_template(power_hover_template),
    );
    grid_plot.set_layout(base_layout("Grid", "Power (W)", &labels).bar_mode(BarMode::Relative));
    grid_plot.set_configuration(base_configuration());
    html_sections.push(grid_plot.to_inline_html(Some("planning-plot-grid")));

    let mut battery_power_plot = Plot::new();
    battery_power_plot.add_trace(
        Bar::new(
            x_values.clone(),
            planning
                .intervals
                .iter()
                .map(|interval| interval.battery_charge_w)
                .collect(),
        )
        .offset(0.1)
        .width(0.8)
        .name("Battery charge")
        .hover_text_array(interval_labels.clone())
        .hover_template(power_hover_template),
    );
    battery_power_plot.add_trace(
        Bar::new(
            x_values.clone(),
            planning
                .intervals
                .iter()
                .map(|interval| -interval.battery_discharge_w)
                .collect(),
        )
        .offset(0.1)
        .width(0.8)
        .name("Battery discharge")
        .hover_text_array(interval_labels.clone())
        .hover_template(power_hover_template),
    );
    battery_power_plot
        .set_layout(base_layout("Battery", "Power (W)", &labels).bar_mode(BarMode::Relative));
    battery_power_plot.set_configuration(base_configuration());
    html_sections.push(battery_power_plot.to_inline_html(Some("planning-plot-battery-power")));

    let mut soc_plot = Plot::new();
    soc_plot.add_trace(
        Scatter::new(
            x_values.clone(),
            planning
                .intervals
                .iter()
                .map(|interval| interval.battery_soc_end * 100.0)
                .collect(),
        )
        .mode(Mode::LinesMarkers)
        .name("Battery SOC")
        .hover_text_array(soc_labels.clone())
        .hover_template(soc_hover_template),
    );
    soc_plot.set_layout(
        base_layout("Battery SOC", "SOC (%)", &soc_labels)
            .y_axis(Axis::new().title("SOC (%)").range(vec![0.0, 100.0])),
    );
    soc_plot.set_configuration(base_configuration());
    html_sections.push(soc_plot.to_inline_html(Some("planning-plot-battery-soc")));

    let mut consumption_plot = Plot::new();
    consumption_plot.add_trace(
        Bar::new(
            x_values.clone(),
            planning
                .intervals
                .iter()
                .map(|interval| interval.consumption_w)
                .collect(),
        )
        .offset(0.1)
        .width(0.8)
        .name("Consumption")
        .hover_text_array(interval_labels.clone())
        .hover_template(power_hover_template),
    );
    consumption_plot.set_layout(base_layout("Consumption", "Power (W)", &labels));
    consumption_plot.set_configuration(base_configuration());
    html_sections.push(consumption_plot.to_inline_html(Some("planning-plot-consumption")));

    let mut solar_plot = Plot::new();
    solar_plot.add_trace(
        Bar::new(
            x_values.clone(),
            planning
                .intervals
                .iter()
                .map(|interval| interval.solar_production_w)
                .collect(),
        )
        .offset(0.1)
        .width(0.8)
        .name("Solar production")
        .hover_text_array(interval_labels.clone())
        .hover_template(power_hover_template),
    );
    solar_plot.set_layout(base_layout("Solar production", "Power (W)", &labels));
    solar_plot.set_configuration(base_configuration());
    html_sections.push(solar_plot.to_inline_html(Some("planning-plot-solar")));

    let mut price_plot = Plot::new();
    price_plot.add_trace(
        Scatter::new(x_values.clone(), import_price_step_values)
            .mode(Mode::Lines)
            .line(Line::new().shape(LineShape::Hv))
            .name("Electricity import price")
            .hover_text_array(interval_labels.clone())
            .hover_template(price_hover_template),
    );
    price_plot.add_trace(
        Scatter::new(x_values.clone(), export_price_step_values)
            .mode(Mode::Lines)
            .line(Line::new().shape(LineShape::Hv))
            .name("Electricity export price")
            .hover_text_array(interval_labels.clone())
            .hover_template(price_hover_template),
    );
    price_plot.add_trace(
        Scatter::new(x_values.clone(), shadow_price_step_values)
            .mode(Mode::Lines)
            .line(Line::new().shape(LineShape::Hv))
            .name("Shadow price")
            .hover_text_array(interval_labels.clone())
            .hover_template(price_hover_template),
    );
    price_plot.set_layout(base_layout("Electricity price", "EUR/kWh", &labels));
    price_plot.set_configuration(base_configuration());
    html_sections.push(price_plot.to_inline_html(Some("planning-plot-price")));

    html_sections
        .join("\n<div style=\"height:1px;background:#d9d9d9;margin:0.5rem 0 0.9rem 0;\"></div>\n")
}

fn base_layout(title: &str, y_axis_label: &str, labels: &[String]) -> Layout {
    let max_x = labels.len() as f64;
    let (tick_values, tick_text) = hourly_ticks(labels);

    Layout::new()
        .title(title)
        .height(360)
        .auto_size(true)
        .margin(Margin::new().left(72).right(24).top(30).bottom(100))
        .x_axis(
            Axis::new()
                .type_(AxisType::Linear)
                .show_grid(true)
                .tick_mode(TickMode::Array)
                .tick_values(tick_values)
                .tick_text(tick_text)
                .range(vec![0.0, max_x])
                .tick_angle(-45.0),
        )
        .y_axis(Axis::new().title(y_axis_label))
        .legend(
            Legend::new()
                .orientation(Orientation::Horizontal)
                .x(1.0)
                .x_anchor(Anchor::Right)
                .y(-0.2)
                .y_anchor(Anchor::Top),
        )
}

fn base_configuration() -> Configuration {
    Configuration::new()
        .responsive(true)
        .autosizable(true)
        .display_mode_bar(DisplayModeBar::False)
}

fn hourly_ticks(labels: &[String]) -> (Vec<f64>, Vec<String>) {
    labels
        .iter()
        .enumerate()
        .filter_map(|(index, label)| {
            if label.ends_with(":00") {
                Some((index as f64, label.clone()))
            } else {
                None
            }
        })
        .unzip()
}

fn intent_color(intent: &BatteryIntent) -> &'static str {
    match intent {
        BatteryIntent::Idle => "#9ca3af",
        BatteryIntent::Balance => "#2563eb",
        BatteryIntent::BalanceChargeOnly => "#06b6d4",
        BatteryIntent::BalanceDischargeOnly => "#f59e0b",
        BatteryIntent::FixedCharge { .. } => "#16a34a",
        BatteryIntent::FixedDischarge { .. } => "#dc2626",
        BatteryIntent::Other => "#6b7280",
    }
}

fn intent_label(intent: &BatteryIntent) -> String {
    match intent {
        BatteryIntent::Idle => "Idle".to_string(),
        BatteryIntent::Balance => "Balance".to_string(),
        BatteryIntent::BalanceChargeOnly => "Balance charge only".to_string(),
        BatteryIntent::BalanceDischargeOnly => "Balance discharge only".to_string(),
        BatteryIntent::FixedCharge { power_w } => format!("Fixed charge: {:.0} W", power_w),
        BatteryIntent::FixedDischarge { power_w } => format!("Fixed discharge: {:.0} W", power_w),
        BatteryIntent::Other => "Other".to_string(),
    }
}
