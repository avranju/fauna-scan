# syntax=docker/dockerfile:1

FROM node:24-bookworm-slim AS frontend
WORKDIR /app/web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web ./
RUN npm run format:check && npm run build

FROM rust:1.97-slim-bookworm AS builder

WORKDIR /app

RUN apt-get update \
    && apt-get install --no-install-recommends --yes \
        cmake \
        g++ \
        make \
        perl \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock build.rs ./
COPY --from=frontend /app/web/dist ./web/dist/
COPY migrations ./migrations/
COPY migrations-postgres ./migrations-postgres/
COPY src ./src/

RUN cargo build --locked --release --bin fauna-scan

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install --no-install-recommends --yes ca-certificates sqlite3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 fauna-scan \
    && useradd --system --uid 10001 --gid fauna-scan \
        --home-dir /var/lib/fauna-scan --shell /usr/sbin/nologin fauna-scan \
    && install -d -o fauna-scan -g fauna-scan \
        /var/lib/fauna-scan /var/lib/fauna-scan/images

COPY --from=builder /app/target/release/fauna-scan /usr/local/bin/fauna-scan

USER fauna-scan
WORKDIR /var/lib/fauna-scan

ENTRYPOINT ["/usr/local/bin/fauna-scan"]
CMD ["--config", "/etc/fauna-scan/config.toml", "run"]
