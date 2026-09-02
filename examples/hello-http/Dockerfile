# Builds the same binary two ways can produce: `cargo build --release` on a
# host with the Rust toolchain, or this image (used by cf-rs's container
# build mode, and directly deployable to Cloud Run functions as a container).
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/hello-http /usr/local/bin/hello-http
ENV FUNCTION_TARGET=hello
ENV FUNCTION_SIGNATURE_TYPE=http
ENV PORT=8080
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/hello-http"]
