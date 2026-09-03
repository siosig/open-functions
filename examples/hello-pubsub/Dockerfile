# Builds the same binary two ways can produce: `cargo build --release` on a
# host with the Rust toolchain, or this image (directly deployable to Cloud
# Run functions as a container, per quickstart.md's "Cloud Run" section).
#
# This crate is a standalone (non-workspace) package that depends on
# `open-functions-sdk` via the relative path `../../crates/open-functions-sdk`
# (see this crate's own Cargo.toml) -- a real published crate would instead
# depend on a crates.io/git version of the SDK and this dependency would
# need nothing special, but since this repo's own SDK isn't published,
# building this Dockerfile REQUIRES the repository root as its build
# context (so that path resolves), not this directory:
#
#   docker build -f examples/hello-pubsub/Dockerfile -t hello-pubsub:dev .
#
# (run from the repository root; a plain `docker build examples/hello-pubsub`
# fails with "failed to read `/crates/open-functions-sdk/Cargo.toml`" since
# the sibling `crates/` directory never makes it into the build context).
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
WORKDIR /src/examples/hello-pubsub
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/examples/hello-pubsub/target/release/hello-pubsub /usr/local/bin/hello-pubsub
ENV FUNCTION_TARGET=on_msg
ENV FUNCTION_SIGNATURE_TYPE=cloudevent
ENV PORT=8080
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/hello-pubsub"]
