use crate::types::Planning;
use plotly::{
    Bar, Configuration, Plot, Scatter,
    common::{Anchor, Line, LineShape, Mode, Orientation, TickMode},
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
    // let mut price_hover_labels = interval_labels.clone();
    // if let Some(last_label) = interval_labels.last() {
    //     price_hover_labels.push(last_label.clone());
    // }
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
    let power_hover_template = "%{hovertext}: %{y:.0f}W<extra></extra>";
    let soc_hover_template = "%{hovertext}: %{y:.1f}%<extra></extra>";
    let price_hover_template = "%{hovertext}: %{y:.4f} EUR/kWh<extra></extra>";

    let mut html_sections = Vec::new();

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
                .map(|interval| interval.battery_charge_power_w)
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
                .map(|interval| -interval.battery_discharge_power_w)
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
