"""Export Chronos-2 to ONNX for the planner's long-range forecast, and check it against PyTorch.

    uv run --group export python export_onnx.py

The exported model is univariate with fixed shapes: one forecast of exactly CONTEXT_STEPS
history steps (NaN = missing, so pad a shorter history with NaN in front).
- input  `context`:   float32 (1, CONTEXT_STEPS)
- output `quantiles`: float32 (1, 21, output_steps), levels in the metadata `quantile_levels`
The metadata also holds `context_steps` and `output_steps`.

Scaling, patching and missing-value masks are part of `Chronos2Model.forward` and so part of the
graph; only selecting the forecast horizon is left to the caller.

The check runs the ONNX model with onnxruntime on windows of `data/history_15min.csv` (from
`cargo run --bin export_history`), or of a synthetic house load series if that file does not
exist (Docker build), and compares it with the PyTorch model and with the Chronos pipeline.
It also writes `<model>.check.json` (a context window and the PyTorch quantiles) for the Rust
test `forecaster::onnx::tests::matches_pytorch_reference`.
"""

from __future__ import annotations

import argparse
import json
import math
import time
from pathlib import Path

import numpy as np
import onnx
import onnxruntime as ort
import pandas as pd
import torch
from chronos import Chronos2Pipeline
from chronos.chronos2.model import Chronos2Encoder

# Must cover forecasts::LONG_HORIZON_SLOTS (3 days of 15 minutes + 2).
HORIZON_STEPS = 3 * 96 + 2
# forecasts::LONG_HISTORY_SLOTS
CONTEXT_STEPS = 21 * 96


def _group_time_mask_for_onnx(
    group_ids: torch.Tensor, attention_mask: torch.Tensor, floating_type: torch.dtype
) -> torch.Tensor:
    """`Chronos2Encoder._construct_and_invert_group_time_mask` without the boolean `einsum`, which
    ONNX does not allow. For 0/1 masks the outer product is the same as the einsum."""
    group_mask = (group_ids[:, None] == group_ids[None, :]).to(floating_type)
    time_mask = attention_mask.to(floating_type)
    group_time_mask = group_mask[:, :, None] * time_mask[None, :, :]  # (q, b, t)
    group_time_mask = group_time_mask.permute(2, 0, 1)[:, None, :, :]  # (t, 1, q, b)
    return (1.0 - group_time_mask) * torch.finfo(floating_type).min


class UnivariateChronos2(torch.nn.Module):
    """Every series in the batch is forecast independently, without covariates."""

    def __init__(self, model: torch.nn.Module, num_output_patches: int):
        super().__init__()
        self.model = model
        self.num_output_patches = num_output_patches

    def forward(self, context: torch.Tensor) -> torch.Tensor:
        group_ids = torch.arange(context.shape[0], device=context.device)
        output = self.model(
            context=context, group_ids=group_ids, num_output_patches=self.num_output_patches
        )
        return output.quantile_preds


def export(pipeline: Chronos2Pipeline, path: Path, example: torch.Tensor) -> list[float]:
    config = pipeline.model.chronos_config
    num_output_patches = math.ceil(HORIZON_STEPS / config.output_patch_size)
    assert num_output_patches <= config.max_output_patches
    wrapper = UnivariateChronos2(pipeline.model, num_output_patches).eval()

    # Keep the staticmethod object itself; reading it through the class would unwrap it.
    original = Chronos2Encoder.__dict__["_construct_and_invert_group_time_mask"]
    Chronos2Encoder._construct_and_invert_group_time_mask = staticmethod(_group_time_mask_for_onnx)
    try:
        return _export(wrapper, config, num_output_patches, path, example)
    finally:
        Chronos2Encoder._construct_and_invert_group_time_mask = original


def _export(wrapper, config, num_output_patches: int, path: Path, example: torch.Tensor) -> list[float]:
    with torch.no_grad():
        program = torch.onnx.export(
            wrapper,
            (example,),
            input_names=["context"],
            output_names=["quantiles"],
            dynamo=True,
        )
    program.save(str(path))

    levels = [float(q) for q in config.quantiles]
    model = onnx.load(str(path), load_external_data=False)
    for key, value in {
        "quantile_levels": json.dumps(levels),
        "output_steps": str(num_output_patches * config.output_patch_size),
        "context_steps": str(example.shape[1]),
        "source_model": "amazon/chronos-2",
    }.items():
        entry = model.metadata_props.add()
        entry.key, entry.value = key, value
    onnx.save(model, str(path))
    return levels


def synthetic_history(days: int = 60, seed: int = 0) -> np.ndarray:
    """House-load-like series at 15 minutes: base load, morning and evening peaks, random spikes."""
    rng = np.random.default_rng(seed)
    t = np.arange(days * 96)
    hour = (t % 96) / 4
    load = 250 + 300 * np.exp(-((hour - 7.5) ** 2) / 2) + 600 * np.exp(-((hour - 18.5) ** 2) / 3)
    load += 80 * (t // 96 % 7 >= 5)  # weekends
    load += rng.normal(0, 40, t.size)
    spikes = rng.random(t.size) < 0.03
    load[spikes] += rng.uniform(1000, 2500, spikes.sum())
    return np.maximum(load, 0.0)


def check(pipeline: Chronos2Pipeline, path: Path, levels: list[float], history: np.ndarray):
    session = ort.InferenceSession(str(path), providers=["CPUExecutionProvider"])
    config = pipeline.model.chronos_config
    num_output_patches = math.ceil(HORIZON_STEPS / config.output_patch_size)
    wrapper = UnivariateChronos2(pipeline.model, num_output_patches).eval()

    # Windows spread over the history, the last one ending at the most recent data.
    ends = np.linspace(CONTEXT_STEPS, len(history), 8, dtype=int)
    windows = [history[end - CONTEXT_STEPS : end].astype(np.float32) for end in ends]
    with_gaps = windows[-1].copy()
    with_gaps[100:150] = np.nan  # a half-day outage
    with_gaps[-3:] = np.nan  # statistics not compiled yet
    windows.append(with_gaps)

    max_torch_diff = max_pipeline_diff = 0.0
    times = []
    reference = None
    for context in windows:
        start = time.perf_counter()
        (onnx_q,) = session.run(None, {"context": context[None, :]})
        times.append(time.perf_counter() - start)
        with torch.no_grad():
            torch_q = wrapper(torch.from_numpy(context)[None, :]).numpy()
        max_torch_diff = max(max_torch_diff, float(np.nanmax(np.abs(onnx_q - torch_q))))
        reference = (context, torch_q[0])  # last one: the window with gaps

        # The pipeline as the planner called it: quantiles 0.05..0.95, HORIZON_STEPS ahead.
        planner_levels = [round(0.05 * i, 2) for i in range(1, 20)]
        pipeline_q, _ = pipeline.predict_quantiles(
            [torch.from_numpy(context)],
            prediction_length=HORIZON_STEPS,
            quantile_levels=planner_levels,
        )
        pipeline_q = pipeline_q[0][0].numpy().T  # (levels, steps)
        index = [levels.index(q) for q in planner_levels]
        onnx_selected = onnx_q[0][index, :HORIZON_STEPS]
        max_pipeline_diff = max(max_pipeline_diff, float(np.abs(onnx_selected - pipeline_q).max()))

    print(f"ONNX output shape: {onnx_q.shape}, all finite: {np.isfinite(onnx_q).all()}")
    print(f"max |ONNX - PyTorch model|:    {max_torch_diff:.6f} W")
    print(f"max |ONNX - Chronos pipeline|: {max_pipeline_diff:.6f} W")
    print(f"onnxruntime time per forecast: {1000 * np.median(times):.0f} ms (median of {len(times)})")

    context, quantiles = reference
    check_path = path.with_name(path.stem + ".check.json")
    check_path.write_text(
        json.dumps(
            {
                "context": [None if np.isnan(v) else float(v) for v in context],
                "quantiles": quantiles.astype(float).tolist(),
            }
        )
    )
    print(f"wrote {check_path}")
    return max_torch_diff, max_pipeline_diff


def main():
    root = Path(__file__).parent.parent
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--model", default="amazon/chronos-2")
    parser.add_argument("--output", type=Path, default=root / "data" / "models" / "chronos2.onnx")
    parser.add_argument("--history", type=Path, default=root / "data" / "history_15min.csv")
    parser.add_argument("--tolerance-w", type=float, default=0.5, help="max allowed deviation")
    args = parser.parse_args()

    if args.history.exists():
        history = pd.read_csv(args.history)["house_load_w"].to_numpy(float)
        print(f"checking on {args.history}")
    else:
        history = synthetic_history()
        print(f"{args.history} not found, checking on a synthetic series")
    pipeline = Chronos2Pipeline.from_pretrained(args.model, device_map="cpu")
    pipeline.model.eval()

    args.output.parent.mkdir(parents=True, exist_ok=True)
    example = torch.from_numpy(history[-CONTEXT_STEPS:].astype(np.float32))[None, :]
    levels = export(pipeline, args.output, example)
    size_mb = sum(p.stat().st_size for p in args.output.parent.glob(args.output.name + "*")) / 1e6
    print(f"exported {args.output} ({size_mb:.0f} MB)")

    torch_diff, pipeline_diff = check(pipeline, args.output, levels, history)
    if max(torch_diff, pipeline_diff) > args.tolerance_w:
        raise SystemExit(f"FAILED: deviation above {args.tolerance_w} W")
    print("OK")


if __name__ == "__main__":
    main()
