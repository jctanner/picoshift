FROM docker.io/library/rust:1.88-slim AS builder
RUN apt-get update && apt-get install -y pkg-config cmake gcc g++ perl make && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release

FROM docker.io/library/debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/ocp-sim /usr/local/bin/ocp-sim
ENTRYPOINT ["ocp-sim"]
