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
# cross-compiles for the target, so a multi-platform build needs no emulation
# (and no arm64 runner). The final stage runs no commands either.
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
    rm -rf /var/lib/apt/lists/*; \
    useradd --create-home --uid 10001 runway
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

# git (for `runway preview prune`), curl and the CA bundle come from this
# official Debian image. No RUN here: a cross-platform build needs no
# emulation. The user entry and its home directory come from the build stage.
FROM buildpack-deps:trixie-scm
COPY --from=build /etc/passwd /etc/group /etc/
COPY --from=build --chown=10001:10001 /home/runway /home/runway
COPY --from=build /usr/local/bin/runway /usr/local/bin/runway
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
USER runway
WORKDIR /workspace
# A CLI image: runs a command and exits, nothing to health-check.
HEALTHCHECK NONE
CMD ["runway", "--help"]
