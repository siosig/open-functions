fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

lint:
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

test:
    cargo nextest run --workspace || cargo test --workspace
    cargo test --doc --workspace

examples-build:
    cd examples/hello-http && cargo build --release
    cd examples/hello-pubsub && cargo build --release

qa: fmt-check lint test examples-build
