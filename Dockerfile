# syntax=docker/dockerfile:1.7
# runway CLI image for CI pipelines (linux/amd64 and linux/arm64).
#
#   docker build -t runway .
#   docker buildx build --platform linux/amd64,linux/arm64 -t runway .
#   docker run --rm -v "$PWD:/workspace" -e GOOGLE_APPLICATION_CREDENTIALS=/workspace/creds.json runway runway plan --stage dev
#
# Colors need a terminal: `docker run -t` (or RUNWAY_COLOR=always).
#
# The build stage always runs on the build machine's own platform and
# cross-compiles for the target. Only the final stage's package install runs
# on the target platform: a multi-platform build needs QEMU for it (a minute or
# two), set up with `docker run --privileged --rm tonistiigi/binfmt --install all`
# locally or docker/setup-qemu-action in CI.
#
# The image has no ENTRYPOINT so CI systems (GitLab `image:`, GitHub `container:`)
# can run shell scripts in it; `runway` is on PATH.

FROM --platform=$BUILDPLATFORM rust:1-trixie AS build
ARG TARGETARCH
ARG BUILDARCH
# lld links aarch64 (see .cargo/config.toml); a cross C toolchain builds the
# C parts of dependencies (aws-lc) when the target differs from the build machine.
RUN set -eux; \
    packages="lld"; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
      case "$TARGETARCH" in \
        arm64) packages="$packages gcc-aarch64-linux-gnu libc6-dev-arm64-cross" ;; \
        amd64) packages="$packages gcc-x86-64-linux-gnu libc6-dev-amd64-cross" ;; \
        *) echo "unsupported target architecture: $TARGETARCH" >&2; exit 1 ;; \
      esac; \
    fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends $packages; \
    rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
COPY benches ./benches
COPY examples ./examples
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=runway-target-${TARGETARCH} \
    set -eux; \
    case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-gnu; gnu=x86_64-linux-gnu ;; \
      arm64) target=aarch64-unknown-linux-gnu; gnu=aarch64-linux-gnu ;; \
    esac; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
      env_target=$(echo "$target" | tr 'a-z-' 'A-Z_'); \
      export "CARGO_TARGET_${env_target}_LINKER=${gnu}-gcc"; \
      export "CC_$(echo "$target" | tr '-' '_')=${gnu}-gcc"; \
      export "AR_$(echo "$target" | tr '-' '_')=${gnu}-ar"; \
    fi; \
    rustup target add "$target"; \
    cargo build --profile dist --locked --bin runway --target "$target"; \
    cp "target/$target/dist/runway" /usr/local/bin/runway

# Debian slim with what runway and CI scripts need: git (for `runway preview
# prune`), curl and the CA bundle. No recommended packages (no Perl extras,
# no ssh: CI clones over HTTPS).
FROM debian:trixie-slim
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl git; \
    rm -rf /var/lib/apt/lists/*; \
    useradd --create-home --uid 10001 runway
COPY --from=build /usr/local/bin/runway /usr/local/bin/runway
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
USER runway
WORKDIR /workspace
# A CLI image: runs a command and exits, nothing to health-check.
HEALTHCHECK NONE
CMD ["runway", "--help"]
