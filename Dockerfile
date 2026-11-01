FROM rust:1.99.1-bookworm AS build

WORKDIR /build
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Limit compilation concurrency for smaller NAS hosts.
ARG BUILD_JOBS=2
RUN cargo build --release --locked --jobs "${BUILD_JOBS}"

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 app \
    && useradd --uid 10001 --gid app --no-create-home --shell /usr/sbin/nologin app \
    && install -d -o app -g app /app/data
COPY --from=build /build/target/release/boannews-incident-brief /usr/local/bin/boannews-incident-brief
WORKDIR /app
USER 10001:10001
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/boannews-incident-brief", "--data-dir", "/app/data"]
# A bare docker run is preview-only. compose.yaml explicitly enables delivery.
CMD ["run"]
