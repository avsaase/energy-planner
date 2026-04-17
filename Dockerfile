FROM lukemathwalker/cargo-chef:latest-rust-latest AS chef
WORKDIR /app
RUN apt-get update \
	&& apt-get install -y --no-install-recommends \
		build-essential \
		clang \
		cmake \
		libclang-dev \
		pkg-config \
	&& rm -rf /var/lib/apt/lists/*

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked

FROM ghcr.io/home-assistant/base:3.23
COPY run.sh /run.sh
COPY --from=builder /app/target/release/energy-planner /usr/bin/energy-planner

RUN chmod a+x /run.sh /usr/bin/energy-planner

CMD ["/run.sh"]
