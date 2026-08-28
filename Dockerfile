# syntax=docker/dockerfile:1

FROM rust:1-slim AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        binutils \
        ca-certificates \
        cmake \
        libssl-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && printf 'fn main() {}\n' > src/main.rs \
    && cargo fetch --locked \
    && rm -rf src

COPY src ./src
RUN cargo build --release --locked \
    && strip target/release/s3-proxy

FROM gcr.io/distroless/cc-debian12 AS runtime

WORKDIR /app
COPY --from=builder /app/target/release/s3-proxy /usr/local/bin/s3-proxy

USER nonroot:nonroot
EXPOSE 8080
CMD ["/usr/local/bin/s3-proxy"]
