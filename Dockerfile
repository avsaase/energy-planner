ARG BUILD_FROM=ghcr.io/hassio-addons/debian-base/amd64:latest
ARG UV_IMAGE=ghcr.io/astral-sh/uv:0.9

FROM ${UV_IMAGE} AS uv

# --- Export Chronos-2 to ONNX (Python only exists in this stage) ------------------------------
FROM python:3.12-slim-bookworm AS model
COPY --from=uv /uv /usr/local/bin/uv
ENV UV_PYTHON_DOWNLOADS=never \
    UV_LINK_MODE=copy \
    UV_PROJECT_ENVIRONMENT=/opt/venv
WORKDIR /forecaster
COPY forecaster/pyproject.toml forecaster/uv.lock forecaster/.python-version ./
RUN uv sync --frozen --no-dev --group export --no-install-project
COPY forecaster/export_onnx.py ./
# Downloads amazon/chronos-2, exports it and checks the export against PyTorch.
RUN uv run --no-sync python export_onnx.py --output /models/chronos2.onnx

# --- Build the planner (ONNX Runtime is linked in statically) ---------------------------------
# Same Debian release as the runtime base (trixie): the prebuilt ONNX Runtime needs glibc >= 2.38
# and libstdc++ from GCC >= 13.
FROM rust:1-trixie AS builder
# HiGHS (highs-sys) is built from source with cmake; bindgen needs libclang.
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake libclang-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY templates templates
RUN cargo build --release --locked --bin energy-planner

# --- Runtime -----------------------------------------------------------------------------------
FROM ${BUILD_FROM}
COPY --from=model /models/chronos2.onnx /opt/energy-planner/models/chronos2.onnx
ENV FORECAST_MODEL_PATH=/opt/energy-planner/models/chronos2.onnx
COPY --from=builder /src/target/release/energy-planner /usr/bin/energy-planner
COPY run.sh /run.sh
RUN chmod a+x /run.sh /usr/bin/energy-planner

CMD ["/run.sh"]
