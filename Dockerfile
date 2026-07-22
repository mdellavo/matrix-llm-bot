# syntax=docker/dockerfile:1
FROM rust:1-slim-bookworm AS builder
WORKDIR /app

# build-essential (a C compiler) is all matrix-sdk's bundled-sqlite feature needs
# to build SQLite from source. Our own HTTP client is rustls, not OpenSSL — but
# anthropic-sdk-rust pulls in openssl-sys transitively via its own reqwest 0.12
# dependency (default-tls), so `perl` is needed too: Cargo.toml's `openssl`
# dependency (feature "vendored") makes openssl-sys build OpenSSL from source
# instead of requiring a system OpenSSL + pkg-config, and that source build's
# Configure script is Perl.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        perl \
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
