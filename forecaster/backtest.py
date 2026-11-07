"""Backtest the exported forecast model on history from `cargo run --bin export_history`.

    uv run python backtest.py long                       # data/models/chronos2.onnx
    uv run python backtest.py long --model a.onnx --model b.onnx
    uv run python backtest.py nowcast

The model is run exactly like the planner runs it (`src/forecaster/onnx.rs`): the ONNX file from
`export_onnx.py` through ONNX Runtime, a fixed history length padded with NaN, quantiles sorted,
and the mean taken over the levels 0.05..0.95. To compare another model, export it to ONNX first.

- long:    the long-range forecast: 15 min steps, ~21 days of context, 48 h ahead, origins every
           6 h. Compared with the seasonal-naive fallback.
- nowcast: the next 30 minutes at 1-minute resolution: the planner's measurement fading into the
           long-range forecast, against alternatives such as running the model on 1-minute data.
           Also checks how often the first 5 minutes fall within the planner's scenario spread.

Reports MAE, bias (mean forecast - actual; negative = underestimate) and P10-P90 coverage
(should be ~0.8), per horizon bucket.
"""

from __future__ import annotations

import argparse
import json
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import onnxruntime as ort
import pandas as pd

ROOT = Path(__file__).parent.parent
DEFAULT_MODEL = ROOT / "data" / "models" / "chronos2.onnx"
# Chronos-2's levels; also used for the seasonal-naive fallback.
DEFAULT_LEVELS = [0.01] + [round(0.05 * i, 2) for i in range(1, 20)] + [0.99]
# The expected value is the average of the equally spaced levels 0.05..0.95 (as in the planner).
MEAN_LEVELS = (0.05, 0.95)


@dataclass(frozen=True)
class Tier:
    file: str
    step_minutes: int
    context: int
    horizon: int
    origin_every: int
    buckets: list[tuple[int, int, str]]  # (start step, end step, label)


TIERS = {
    "long": Tier(
        file="history_15min.csv",
        step_minutes=15,
        context=21 * 96,
        horizon=48 * 4,
        origin_every=6 * 4,
        buckets=[(0, 24, "0-6h"), (24, 96, "6-24h"), (96, 192, "24-48h")],
    ),
    "short": Tier(
        file="history_1min.csv",
        step_minutes=1,
        context=24 * 60,
        horizon=30,
        origin_every=20,
        buckets=[(0, 5, "0-5m"), (5, 15, "5-15m"), (15, 30, "15-30m")],
    ),
}


def fit_context(history: np.ndarray, steps: int) -> np.ndarray:
    """Exactly `steps` values: the most recent ones, padded with NaN in front (as in the planner)."""
    context = np.full(steps, np.nan, dtype=np.float32)
    recent = np.asarray(history, dtype=np.float32)[-steps:]
    if recent.size:
        context[steps - recent.size :] = recent
    return context


def mean_of(levels: list[float], quantiles: np.ndarray) -> np.ndarray:
    """Mean per step from quantiles (levels, steps), averaging the levels 0.05..0.95."""
    rows = [i for i, level in enumerate(levels) if MEAN_LEVELS[0] - 1e-9 <= level <= MEAN_LEVELS[1] + 1e-9]
    return quantiles[rows].mean(axis=0)


class OnnxModel:
    """The exported model, run with ONNX Runtime like `OnnxForecaster` in the planner."""

    def __init__(self, path: Path):
        self.name = path.name
        self.session = ort.InferenceSession(str(path), providers=["CPUExecutionProvider"])
        meta = self.session.get_modelmeta().custom_metadata_map
        self.levels: list[float] = json.loads(meta["quantile_levels"])
        self.context_steps = int(meta["context_steps"])
        self.output_steps = int(meta["output_steps"])

    def quantiles(self, history: np.ndarray, horizon: int) -> np.ndarray:
        """Quantiles (levels, horizon), sorted over the levels."""
        if horizon > self.output_steps:
            raise ValueError(f"horizon {horizon} exceeds the model's {self.output_steps} steps")
        context = fit_context(history, self.context_steps)
        (output,) = self.session.run(None, {"context": context[None, :]})
        return np.sort(output[0][:, :horizon].astype(float), axis=0)


def seasonal_naive(history: np.ndarray, horizon: int, step_minutes: int) -> np.ndarray:
    """The planner's fallback: quantiles (DEFAULT_LEVELS, horizon) from the same slot over the
    last 7 days."""
    season = 1440 // step_minutes
    n = len(history)
    out = np.empty((len(DEFAULT_LEVELS), horizon))
    for h in range(horizon):
        idx = [n + h - k * season for k in range(1, 8) if 0 <= n + h - k * season < n]
        samples = history[idx] if idx else history[-season:]
        samples = samples[np.isfinite(samples)]
        if samples.size == 0:
            samples = np.array([0.0])
        out[:, h] = np.quantile(samples, DEFAULT_LEVELS)
    return out


def load_series(data_dir: Path, tier: Tier) -> tuple[pd.DatetimeIndex, np.ndarray]:
    df = pd.read_csv(data_dir / tier.file, parse_dates=["timestamp"]).set_index("timestamp")
    return pd.DatetimeIndex(df.index).tz_convert("UTC"), df["house_load_w"].to_numpy(float)


def run_long(data_dir: Path, model_paths: list[Path], max_origins: int | None):
    tier = TIERS["long"]
    _, house_load = load_series(data_dir, tier)
    origins = list(range(tier.context, len(house_load) - tier.horizon, tier.origin_every))
    if max_origins:
        origins = origins[-max_origins:]
    if not origins:
        raise SystemExit(f"Not enough history in {tier.file} for one origin")

    # name -> (levels, forecast function)
    forecasters = {
        "seasonal-naive": (
            DEFAULT_LEVELS,
            lambda history: seasonal_naive(history, tier.horizon, tier.step_minutes),
        )
    }
    for path in model_paths:
        model = OnnxModel(path)
        forecasters[model.name] = (model.levels, lambda history, m=model: m.quantiles(history, tier.horizon))

    rows = []
    for name, (levels, forecast) in forecasters.items():
        p10, p90 = levels.index(0.1), levels.index(0.9)
        errors, inside = [], []
        for origin in origins:
            q = np.maximum(forecast(house_load[origin - tier.context : origin]), 0.0)  # planner clamps at 0
            actual = house_load[origin : origin + tier.horizon]
            errors.append(mean_of(levels, q) - actual)
            inside.append((actual >= q[p10]) & (actual <= q[p90]))

        errors = np.array(errors)
        inside = np.array(inside, dtype=float)
        for lo, hi, label in tier.buckets:
            e = errors[:, lo:hi]
            ok = np.isfinite(e)
            rows.append(
                {
                    "model": name,
                    "horizon": label,
                    "MAE (W)": np.abs(e[ok]).mean(),
                    "bias (W)": e[ok].mean(),
                    "P10-P90 coverage": inside[:, lo:hi][ok].mean(),
                }
            )

    print(f"long tier, {len(origins)} origins, {tier.horizon} x {tier.step_minutes} min ahead")
    print(pd.DataFrame(rows).to_string(index=False, float_format=lambda v: f"{v:.2f}"))


def blend_weight_linear(minute: np.ndarray) -> np.ndarray:
    """Full weight until minute 15, linear to 0 at minute 30 (an earlier planner version)."""
    return np.clip((30 - minute) / 15, 0.0, 1.0)


PLANNER_TAU_MINUTES = 20  # nowcast::NOWCAST_TAU_MINUTES
SCENARIO_QUANTILE = 0.9  # replanning.scenario_quantile
SCENARIO_MIN_SPREAD_W = 300.0  # replanning.scenario_min_spread_w
FIRST_INTERVAL_MINUTES = 5


def change_spread(history: np.ndarray, minutes: int, quantile: float) -> tuple[float, float]:
    """`nowcast::house_load_change_spread`: quantiles of (mean of next `minutes`) - (value now)."""
    k = max(minutes, 1)
    n = len(history)
    if n <= k:
        return 0.0, 0.0
    windows = np.lib.stride_tricks.sliding_window_view(history[1:], k)[: n - k]
    changes = windows.mean(axis=1) - history[: n - k]
    changes = changes[np.isfinite(changes)]
    if changes.size < 60:
        return 0.0, 0.0
    return max(-np.quantile(changes, 1 - quantile), 0.0), max(np.quantile(changes, quantile), 0.0)


def run_nowcast(data_dir: Path, model_path: Path, max_origins: int | None):
    """The planner's nowcast (measurement fading into the long-range forecast) vs. alternatives.

    Everything is scored on 1-minute house load over the next 30 minutes. The long-range forecast
    is made like the planner does: from 15-minute history up to the last complete quarter hour.
    """
    short = TIERS["short"]
    long = TIERS["long"]
    index1, load1 = load_series(data_dir, short)
    index15, load15 = load_series(data_dir, long)
    model = OnnxModel(model_path)
    H = short.horizon
    minutes = np.arange(H)

    origins = list(range(short.context, len(load1) - H, short.origin_every))
    if max_origins:
        origins = origins[-max_origins:]

    long_cache: dict[int, np.ndarray] = {}
    spread_inside: list[bool] = []
    spread_widths: list[float] = []

    def long_forecast_per_minute(origin: int) -> np.ndarray | None:
        t = index1[origin]
        slot_start = t.floor("15min")
        s = index15.get_indexer([slot_start])[0]
        if s < long.context:
            return None
        if s not in long_cache:
            q = np.maximum(model.quantiles(load15[s - long.context : s], 4), 0.0)
            long_cache[s] = mean_of(model.levels, q)
        offset = int((t - slot_start).total_seconds() // 60)
        return long_cache[s][(offset + minutes) // 15]

    variants: dict[str, list[np.ndarray]] = {}

    def add(name: str, forecast: np.ndarray, actual: np.ndarray):
        variants.setdefault(name, []).append(forecast - actual)

    for origin in origins:
        long_1min = long_forecast_per_minute(origin)
        if long_1min is None:
            continue
        actual = load1[origin : origin + H]
        current = load1[origin - 1]
        q = np.maximum(model.quantiles(load1[origin - short.context : origin], H), 0.0)
        short_mean = mean_of(model.levels, q)

        w = blend_weight_linear(minutes)
        add("1-min model -> long (linear 15-30 min)", w * short_mean + (1 - w) * long_1min, actual)
        add("1-min model only", short_mean, actual)
        add("long-range only", long_1min, actual)
        add("measurement -> long (linear 15-30 min)", w * current + (1 - w) * long_1min, actual)
        for tau in (5, 10, 20, 30):
            w = np.exp(-(minutes + 0.5) / tau)
            forecast = w * current + (1 - w) * long_1min
            planner = " (planner)" if tau == PLANNER_TAU_MINUTES else ""
            add(f"measurement -> long (exp, tau={tau} min){planner}", forecast, actual)
            if tau == PLANNER_TAU_MINUTES:
                # Scenario spread for the first interval, from the last 24 h of measurements.
                below, above = change_spread(
                    load1[origin - short.context : origin], FIRST_INTERVAL_MINUTES, SCENARIO_QUANTILE
                )
                below, above = max(below, SCENARIO_MIN_SPREAD_W), max(above, SCENARIO_MIN_SPREAD_W)
                expected = forecast[:FIRST_INTERVAL_MINUTES].mean()
                realised = actual[:FIRST_INTERVAL_MINUTES].mean()
                if np.isfinite(realised):
                    spread_inside.append(expected - below <= realised <= expected + above)
                    spread_widths.append(below + above)

    rows = []
    for name, errors in variants.items():
        errors = np.array(errors)
        for lo, hi, label in short.buckets:
            e = errors[:, lo:hi]
            ok = np.isfinite(e)
            rows.append({"variant": name, "horizon": label, "MAE (W)": np.abs(e[ok]).mean(), "bias (W)": e[ok].mean()})
    table = pd.DataFrame(rows)
    n = len(next(iter(variants.values())))
    columns = ["0-5m", "5-15m", "15-30m"]
    print(f"nowcast comparison, {n} origins, 30 x 1 min ahead, model {model.name}")
    print(
        table.pivot(index="variant", columns="horizon", values="MAE (W)")[columns]
        .sort_values("5-15m")
        .to_string(float_format=lambda v: f"{v:.1f}")
    )
    print()
    print("bias (W):")
    print(
        table.pivot(index="variant", columns="horizon", values="bias (W)")[columns].to_string(
            float_format=lambda v: f"{v:.1f}"
        )
    )
    print()
    print(
        f"scenario spread (first {FIRST_INTERVAL_MINUTES} min, quantile {SCENARIO_QUANTILE}, "
        f"min {SCENARIO_MIN_SPREAD_W:.0f} W): actual within low..high in "
        f"{np.mean(spread_inside):.0%} of {len(spread_inside)} origins "
        f"(target ~{2 * SCENARIO_QUANTILE - 1:.0%}), mean width {np.mean(spread_widths):.0f} W"
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "tier",
        choices=["long", "nowcast"],
        help="long: the long-range forecast; nowcast: the first 30 minutes (see above)",
    )
    parser.add_argument(
        "--model",
        action="append",
        type=Path,
        help=f"exported ONNX model, repeatable for long (default: {DEFAULT_MODEL.relative_to(ROOT)})",
    )
    parser.add_argument("--data-dir", type=Path, default=ROOT / "data")
    parser.add_argument("--max-origins", type=int, help="only use the most recent N origins")
    args = parser.parse_args()
    models = args.model or [DEFAULT_MODEL]
    if args.tier == "nowcast":
        if len(models) != 1:
            parser.error("nowcast takes one --model")
        run_nowcast(args.data_dir, models[0], args.max_origins)
    else:
        run_long(args.data_dir, models, args.max_origins)


if __name__ == "__main__":
    main()
