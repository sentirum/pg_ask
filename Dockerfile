# pg_ask — PostgreSQL AI extension
#
# Multi-stage build:
#   builder: compiles the pgrx Rust extension against PG dev headers
#   runtime: clean postgres base + the compiled .so + SQL files
#
# Usage:
#   docker build -t pg_ask:0.5.2-pg18 .
#   docker run -e POSTGRES_PASSWORD=secret \
#              -e POSTGRES_DB=demo \
#              -p 5432:5432 pg_ask:0.5.2-pg18
#
# Then configure in psql:
#   SELECT ask.config('provider', 'anthropic');
#   SELECT ask.config('api_key',  'sk-ant-...');
#   SELECT ask.ask('how many tables do we have?');

ARG PG_MAJOR=18
ARG PGRX_VERSION=0.18.0

# ──────────────────────────────────────────────────────────────────────────────
# Stage 1: builder
#   Inherits the PGDG apt configuration from the official postgres image so
#   postgresql-server-dev-${PG_MAJOR} is available without an extra repo step.
# ──────────────────────────────────────────────────────────────────────────────
FROM postgres:${PG_MAJOR}-bookworm AS builder

# Re-declare ARGs so they're visible inside this stage.
ARG PG_MAJOR
ARG PGRX_VERSION

# System build dependencies.
#   ca-certificates:      curl/rustup TLS root trust (missing from
#                         the slim postgres:18 base; without this
#                         the rustup-init download fails with
#                         "curl: (77) error setting certificate file").
#   libclang-dev / clang: bindgen (pgrx uses it to parse PG headers)
#   pkg-config:           still needed by some transitive build scripts
#   postgresql-server-dev-${PG_MAJOR}: PG header files + pg_config
#
# v0.5.3 dropped libssl-dev: ureq 3 uses rustls now, so we no longer
# need the OpenSSL headers in the builder image.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        build-essential \
        curl \
        pkg-config \
        libclang-dev \
        clang \
        postgresql-server-dev-${PG_MAJOR} \
    && rm -rf /var/lib/apt/lists/*

# Install Rust stable into a world-readable location so the build
# can proceed as a non-root user if needed.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --no-modify-path \
    && chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME"

# Install cargo-pgrx (version must match Cargo.toml exactly).
RUN cargo install cargo-pgrx --locked --version "=${PGRX_VERSION}"

# Initialise pgrx against the system PG installation.
# This records pg_config paths in ~/.pgrx/config.toml (no download).
RUN cargo pgrx init \
        --pg${PG_MAJOR} /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config

WORKDIR /build
COPY . .

# Compile and install into system paths. pgrx writes:
#   /usr/lib/postgresql/${PG_MAJOR}/lib/pg_ask.so
#   /usr/share/postgresql/${PG_MAJOR}/extension/pg_ask.control
#   /usr/share/postgresql/${PG_MAJOR}/extension/pg_ask--0.5.2.sql
# (and any upgrade scripts it finds via its sql-generator pass).
RUN cargo pgrx install \
        --release \
        --features pg${PG_MAJOR} \
        --pg-config /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config

# Collect only the pg_ask artefacts into a clean staging directory so
# the runtime COPY doesn't drag in all the default postgres extensions
# that are already present in the postgres:18 base image.
RUN mkdir -p /staging/lib /staging/extension \
    && cp /usr/lib/postgresql/${PG_MAJOR}/lib/pg_ask.so \
          /staging/lib/ \
    && cp /usr/share/postgresql/${PG_MAJOR}/extension/pg_ask.control \
          /usr/share/postgresql/${PG_MAJOR}/extension/pg_ask*.sql \
          /staging/extension/ \
    && cp sql/pg_ask--0.5.1--0.5.2.sql /staging/extension/ 2>/dev/null || true

# ──────────────────────────────────────────────────────────────────────────────
# Stage 2: runtime
#   Slim postgres base — none of the build tools or Rust artefacts.
# ──────────────────────────────────────────────────────────────────────────────
FROM postgres:${PG_MAJOR}-bookworm

ARG PG_MAJOR

LABEL org.opencontainers.image.title="pg_ask" \
      org.opencontainers.image.description="Ask your PostgreSQL database in natural language" \
      org.opencontainers.image.source="https://github.com/sentirum/pg_ask" \
      org.opencontainers.image.licenses="PostgreSQL"

# ── Extension .so ─────────────────────────────────────────────────────────────
COPY --from=builder \
    /staging/lib/pg_ask.so \
    /usr/lib/postgresql/${PG_MAJOR}/lib/pg_ask.so

# ── SQL files (control + pgrx-generated + upgrade scripts) ───────────────────
# Copies only the pg_ask artefacts; default postgres extensions that
# already exist in the runtime base image are left untouched.
COPY --from=builder \
    /staging/extension/ \
    /usr/share/postgresql/${PG_MAJOR}/extension/

# ── First-start hook ──────────────────────────────────────────────────────────
# Files in /docker-entrypoint-initdb.d/ run once when the data directory
# is first initialised (alphabetical order, .sql and .sh supported).
COPY docker/initdb/ /docker-entrypoint-initdb.d/

# ── Defaults ──────────────────────────────────────────────────────────────────
# Override these at runtime with -e POSTGRES_PASSWORD=… etc.
ENV POSTGRES_DB=pg_ask_demo

# Expose standard PG port.
EXPOSE 5432
