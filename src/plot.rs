use crate::types::{BatteryIntent, Planning};
use jiff::Zoned;
use plotly::{
    Bar, Configuration, Plot, Scatter,
    common::{Anchor, DashType, Line, LineShape, Marker, Mode, Orientation, TickMode},
    configuration::DisplayModeBar,
    layout::{Axis, AxisType, BarMode, Layout, Legend, Margin},
};

pub fn generate_plot(planning: &Planning) -> String {
    let start_times: Vec<&Zoned> = planning.intervals.iter().map(|i| &i.start).collect();
    let soc_hover: Vec<String> = planning
        .intervals
        .iter()
        .map(|i| i.end.time().strftime("%H:%M").to_string())
        .collect();
    let x_values: Vec<f64> = (0..start_times.len()).map(|i| i as f64).collect();

    let interval_labels: Vec<String> = planning
        .intervals
        .iter()
        .map(|interval| {
            format!(
                "{}-{}",
                interval.start.time().strftime("%H:%M"),
                interval.end.time().strftime("%H:%M"),
            )
        })
        .collect();

    // Split each price series into actual (solid) and forecast (faded) traces.
    // Where a segment doesn't apply, use f64::NAN so plotly leaves a gap.
    let mut import_actual: Vec<f64> = Vec::new();
    let mut import_forecast: Vec<f64> = Vec::new();
    let mut export_actual: Vec<f64> = Vec::new();
    let mut export_forecast: Vec<f64> = Vec::new();
    for interval in &planning.intervals {
        if interval.electricity_price_is_forecast {
            import_actual.push(f64::NAN);
            import_forecast.push(interval.electricity_price_eur_per_kwh_take);
            export_actual.push(f64::NAN);
            export_forecast.push(interval.electricity_price_eur_per_kwh_feed);
        } else {
            import_actual.push(interval.electricity_price_eur_per_kwh_take);
            import_forecast.push(f64::NAN);
            export_actual.push(interval.electricity_price_eur_per_kwh_feed);
            export_forecast.push(f64::NAN);
        }
    }
    // Extend by one for the step-line trailing segment.
    if let Some(&v) = import_actual.last() {
        import_actual.push(v);
    }
    if let Some(&v) = import_forecast.last() {
        import_forecast.push(v);
    }
    if let Some(&v) = export_actual.last() {
        export_actual.push(v);
    }
    if let Some(&v) = export_forecast.last() {
        export_forecast.push(v);
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
        base_layout("Battery intent", "", &start_times)
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
            planning.intervals.iter().map(|i| i.grid_import_w).collect(),
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
                .map(|i| -i.grid_export_w)
                .collect(),
        )
        .offset(0.1)
        .width(0.8)
        .name("Grid export")
        .hover_text_array(interval_labels.clone())
        .hover_template(power_hover_template),
    );
    grid_plot
        .set_layout(base_layout("Grid", "Power (W)", &start_times).bar_mode(BarMode::Relative));
    grid_plot.set_configuration(base_configuration());
    html_sections.push(grid_plot.to_inline_html(Some("planning-plot-grid")));

    let mut battery_power_plot = Plot::new();
    battery_power_plot.add_trace(
        Bar::new(
            x_values.clone(),
            planning
                .intervals
                .iter()
                .map(|i| i.battery_charge_w)
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
                .map(|i| -i.battery_discharge_w)
                .collect(),
        )
        .offset(0.1)
        .width(0.8)
        .name("Battery discharge")
        .hover_text_array(interval_labels.clone())
        .hover_template(power_hover_template),
    );
    battery_power_plot
        .set_layout(base_layout("Battery", "Power (W)", &start_times).bar_mode(BarMode::Relative));
    battery_power_plot.set_configuration(base_configuration());
    html_sections.push(battery_power_plot.to_inline_html(Some("planning-plot-battery-power")));

    let mut soc_plot = Plot::new();
    soc_plot.add_trace(
        Scatter::new(
            x_values.clone(),
            planning
                .intervals
                .iter()
                .map(|i| i.battery_soc_end * 100.0)
                .collect(),
        )
        .mode(Mode::LinesMarkers)
        .name("Battery SOC")
        .hover_text_array(soc_hover.clone())
        .hover_template(soc_hover_template),
    );
    soc_plot.set_layout(
        base_layout("Battery SOC", "SOC (%)", &start_times)
            .y_axis(Axis::new().title("SOC (%)").range(vec![0.0, 100.0])),
    );
    soc_plot.set_configuration(base_configuration());
    html_sections.push(soc_plot.to_inline_html(Some("planning-plot-battery-soc")));

    let mut consumption_plot = Plot::new();
    consumption_plot.add_trace(
        Bar::new(
            x_values.clone(),
            planning.intervals.iter().map(|i| i.consumption_w).collect(),
        )
        .offset(0.1)
        .width(0.8)
        .name("Consumption")
        .hover_text_array(interval_labels.clone())
        .hover_template(power_hover_template),
    );
    consumption_plot.set_layout(base_layout("Consumption", "Power (W)", &start_times));
    consumption_plot.set_configuration(base_configuration());
    html_sections.push(consumption_plot.to_inline_html(Some("planning-plot-consumption")));

    let mut solar_plot = Plot::new();
    solar_plot.add_trace(
        Bar::new(
            x_values.clone(),
            planning
                .intervals
                .iter()
                .map(|i| i.solar_production_w)
                .collect(),
        )
        .offset(0.1)
        .width(0.8)
        .name("Solar production")
        .hover_text_array(interval_labels.clone())
        .hover_template(power_hover_template),
    );
    solar_plot.set_layout(base_layout("Solar production", "Power (W)", &start_times));
    solar_plot.set_configuration(base_configuration());
    html_sections.push(solar_plot.to_inline_html(Some("planning-plot-solar")));

    let mut price_plot = Plot::new();
    price_plot.add_trace(
        Scatter::new(x_values.clone(), import_actual)
            .mode(Mode::Lines)
            .line(Line::new().shape(LineShape::Hv).color("#1d4ed8"))
            .name("Electricity import price")
            .hover_text_array(interval_labels.clone())
            .hover_template(price_hover_template),
    );
    price_plot.add_trace(
        Scatter::new(x_values.clone(), import_forecast)
            .mode(Mode::Lines)
            .line(
                Line::new()
                    .shape(LineShape::Hv)
                    .color("#93c5fd")
                    .dash(DashType::Dot),
            )
            .name("Electricity import price (forecast)")
            .show_legend(true)
            .hover_text_array(interval_labels.clone())
            .hover_template(price_hover_template),
    );
    price_plot.add_trace(
        Scatter::new(x_values.clone(), export_actual)
            .mode(Mode::Lines)
            .line(Line::new().shape(LineShape::Hv).color("#15803d"))
            .name("Electricity export price")
            .hover_text_array(interval_labels.clone())
            .hover_template(price_hover_template),
    );
    price_plot.add_trace(
        Scatter::new(x_values.clone(), export_forecast)
            .mode(Mode::Lines)
            .line(
                Line::new()
                    .shape(LineShape::Hv)
                    .color("#86efac")
                    .dash(DashType::Dot),
            )
            .name("Electricity export price (forecast)")
            .show_legend(true)
            .hover_text_array(interval_labels.clone())
            .hover_template(price_hover_template),
    );
    price_plot.set_layout(base_layout("Electricity price", "EUR/kWh", &start_times));
    price_plot.set_configuration(base_configuration());
    html_sections.push(price_plot.to_inline_html(Some("planning-plot-price")));

    html_sections
        .join("\n<div style=\"height:1px;background:#d9d9d9;margin:0.5rem 0 0.9rem 0;\"></div>\n")
}

fn base_layout(title: &str, y_axis_label: &str, start_times: &[&Zoned]) -> Layout {
    let max_x = start_times.len() as f64;
    let (tick_values, tick_text) = hourly_ticks(start_times);

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

/// Returns tick positions at every hour, with labels only for hours divisible
/// by 3 and a date+time label at midnight.  Hours not divisible by 3 get an
/// empty label so the tick mark appears but no text is drawn.
fn hourly_ticks(start_times: &[&Zoned]) -> (Vec<f64>, Vec<String>) {
    start_times
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            if t.minute() != 0 {
                return None;
            }
            let label = if t.hour() == 0 {
                t.date().strftime("%Y-%m-%d").to_string()
            } else if t.hour() % 3 == 0 {
                t.time().strftime("%H:%M").to_string()
            } else {
                String::new()
            };
            Some((i as f64, label))
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
