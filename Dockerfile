# syntax=docker/dockerfile:1
#
# Multi-stage build for the open-functions binary, per
# specs/001-cloud-functions-local/contracts/ops-config.md ("Docker
# connection" section):
#
#   1. chef    - shared base image with cargo-chef installed.
#   2. planner - cargo-chef computes a dependency-only build recipe
#                (recipe.json) from the workspace manifests, so the
#                dependency layer below can be cached independently of
#                application source changes.
#   3. builder - cargo-chef cooks (builds) just the dependency graph from
#                the cached recipe, then cargo-zigbuild cross-compiles the
#                actual open-functions binary for the musl target matching the
#                image's requested TARGETARCH. The musl target produces a
#                statically linked binary with no glibc dependency, so it
#                runs unmodified on the distroless runtime image below.
#                This stage always runs on BUILDPLATFORM (the builder
#                host's own native architecture) regardless of TARGETARCH:
#                cargo-zigbuild cross-compiles natively (no QEMU emulation
#                needed for the actual Rust build), which is why a
#                multi-arch `docker buildx build --platform
#                linux/amd64,linux/arm64` stays fast even though only one
#                of those matches the build host.
#   4. runtime - gcr.io/distroless/cc-debian12:nonroot: no shell, no
#                package manager, runs as a non-root user by default.
#
# The examples/ directory holds standalone example function crates (each
# with its own [workspace] table) that are not part of this workspace and
# are not needed to build the open-functions binary; .dockerignore excludes it from
# the build context entirely.
#
# Runtime note (not something this Dockerfile does): source-mode container
# builds and image-mode functions need Docker-socket access from inside the
# open-functions container to launch function containers/builds, e.g.:
#   docker run \
#     -v /var/run/docker.sock:/var/run/docker.sock \
#     --group-add <docker gid on the host> \
#     --network open-functions \
#     -v <function source dir>:<function source dir>:ro \
#     ghcr.io/<org>/open-functions:<ver>
# The source directory bind mount must use the same path inside the
# container as on the host, since open-functions resolves function source paths as
# given to it (see docker/config.toml and ops-config.md for details).
#
# No Python in this image (002-python-runtime): the runtime stage below is
# gcr.io/distroless/cc-debian12:nonroot -- no shell, no package manager, so
# it cannot install python3.14/uv even if we wanted to, and doing so would
# also defeat the point of a distroless runtime image (minimal attack
# surface, no interpreter/toolchain to keep patched). Python functions are
# therefore always built and run via python.mode = "container" from this
# image (see docker/config.toml's [python] section and the Docker-socket
# note above) -- the same nested-build mechanism already used for
# source-mode Rust builds and image-mode functions.

########################################
# Stage: chef - shared base with cargo-chef installed
########################################
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS chef
WORKDIR /app
# Pin the active rustup toolchain to the one resolved by the workspace's
# rust-toolchain.toml *before* anything below adds components/targets to
# it. Without this, `rustup target add` (below) adds the musl target to
# whatever toolchain happens to be the image's default, and the later
# `COPY . .` (which brings rust-toolchain.toml into the build context for
# the first time) makes rustup switch to a *different*, lazily-installed
# "stable" toolchain that never got the target added - manifesting as a
# `can't find crate for std` error when cross-compiling.
COPY rust-toolchain.toml .
RUN cargo install cargo-chef --locked

########################################
# Stage: planner - compute the cargo-chef dependency recipe
########################################
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

########################################
# Stage: builder - cross-compile a static musl binary via cargo-zigbuild
########################################
FROM --platform=$BUILDPLATFORM chef AS builder

# Set by buildx to the requested output platform's arch (e.g. "amd64",
# "arm64") even though this stage itself runs on BUILDPLATFORM — mapped
# below to the matching Rust musl target triple.
ARG TARGETARCH
RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-musl > /tmp/rust_target ;; \
      arm64) echo aarch64-unknown-linux-musl > /tmp/rust_target ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac

# cargo-zigbuild uses the zig toolchain (installed here via its official
# PyPI package, which ships prebuilt zig binaries) to cross-compile and
# link against musl without needing a musl-gcc sysroot on the host.
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3-pip \
    && rm -rf /var/lib/apt/lists/* \
    && pip3 install --no-cache-dir --break-system-packages ziglang \
    && cargo install --locked cargo-zigbuild \
    && rustup target add "$(cat /tmp/rust_target)"

# Build (and cache) just the dependency graph first.
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json \
    --target "$(cat /tmp/rust_target)" --zigbuild

# Now build the actual open-functions binary against the already-cooked dependencies,
# then copy it to a target-triple-independent path so the runtime stage
# below doesn't need its own TARGETARCH branching.
COPY . .
RUN cargo zigbuild --release --target "$(cat /tmp/rust_target)" -p open-functions \
    && cp "target/$(cat /tmp/rust_target)/release/open-functions" /app/open-functions

########################################
# Stage: runtime - distroless, no shell, nonroot
########################################
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime

COPY --from=builder /app/open-functions /usr/local/bin/open-functions
COPY docker/config.toml /etc/open-functions/config.toml

USER nonroot
ENTRYPOINT ["/usr/local/bin/open-functions"]
CMD ["serve"]
EXPOSE 8080 8081
VOLUME /data
HEALTHCHECK CMD ["/usr/local/bin/open-functions", "health"]
STOPSIGNAL SIGTERM
