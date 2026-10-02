# syntax=docker/dockerfile:1.7
# runway CLI image for CI pipelines.
#
#   docker build -t runway .
#   docker run --rm -v "$PWD:/workspace" -e GOOGLE_APPLICATION_CREDENTIALS=/workspace/creds.json runway runway plan --stage dev
#
# The image has no ENTRYPOINT so CI systems (GitLab `image:`, GitHub `container:`)
# can run shell scripts in it; `runway` is on PATH.

FROM rust:1-bookworm AS build
# lld: used as the linker on aarch64 (.cargo/config.toml); x86_64 already uses
# Rust's bundled lld.
RUN apt-get update \
 && apt-get install -y --no-install-recommends lld \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
COPY benches ./benches
COPY examples ./examples
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --profile dist --locked --bin runway \
 && cp target/dist/runway /usr/local/bin/runway

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 10001 runway
COPY --from=build /usr/local/bin/runway /usr/local/bin/runway
USER runway
WORKDIR /workspace
ENV NO_COLOR=1
CMD ["runway", "--help"]
