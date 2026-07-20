# syntax=docker/dockerfile:1
FROM rust:1-slim-bookworm AS builder
WORKDIR /app

# A C compiler is all matrix-sdk's bundled-sqlite feature needs to build SQLite
# from source; TLS is rustls (see Cargo.toml), so no OpenSSL/pkg-config required.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
WORKDIR /app

# rustls-native-certs reads the OS trust store at runtime.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/matrix-llm-bot /usr/local/bin/matrix-llm-bot
# skills/ is user-authored source meant to ship with the bot (see README), not
# runtime state — bake it into the image; override with a volume mount to edit
# skills without rebuilding.
COPY skills ./skills

ENTRYPOINT ["matrix-llm-bot"]
