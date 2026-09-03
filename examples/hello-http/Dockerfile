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
#   docker build -f examples/hello-http/Dockerfile -t hello-http:dev .
#
# (run from the repository root; a plain `docker build examples/hello-http`
# fails with "failed to read `/crates/open-functions-sdk/Cargo.toml`" since
# the sibling `crates/` directory never makes it into the build context).
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
WORKDIR /src/examples/hello-http
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/examples/hello-http/target/release/hello-http /usr/local/bin/hello-http
ENV FUNCTION_TARGET=hello
ENV FUNCTION_SIGNATURE_TYPE=http
ENV PORT=8080
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/hello-http"]
