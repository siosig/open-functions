# open-functions

[Japanese README](README.ja.md)

## Table of Contents

- [Overview](#overview)
- [Quickstart](#quickstart)
- [Writing a function](#writing-a-function)
- [Installation](#installation)
- [Configuration](#configuration)
- [URL scheme](#url-scheme)
- [Managing functions](#managing-functions)
- [Pub/Sub integration](#pubsub-integration)
- [Observability](#observability)
- [Troubleshooting](#troubleshooting)
- [License](#license)

## Overview

`open-functions` is a Rust service that runs a Google Cloud Run functions
(Cloud Functions 2nd gen) compatible function-hosting environment locally or
on-premises, with no dependency on any cloud connection. Functions are
written in Rust against the same Functions Framework contract Cloud Run
functions itself uses, so a function hosted on `open-functions` deploys to
real Cloud Run functions completely unmodified.

The workspace has three crates:

| Crate | Role |
|---|---|
| `open-functions` | The host binary: two listeners (invoke + admin), the `open-functions fn ...` CLI |
| `open-functions-core` | Domain layer: registry, build, runtime, pool, forwarding, and Pub/Sub integration |
| `open-functions-sdk` | The SDK function authors write against |

## Quickstart

```bash
cargo run -p open-functions -- serve --data-dir ./tmp/data
cargo run -p open-functions -- fn deploy hello --source ./examples/hello-http --entry-point hello
curl http://127.0.0.1:8080/hello/world
```

The first command starts the host with two listeners: `:8080` for function
invocations and `:8081` for the admin API. The second builds
`examples/hello-http` (a real `cargo build --release`) and registers it as
`hello`. The `curl` reaches the running instance through the invoke
listener's path-prefix routing.

## Writing a function

See [`crates/open-functions-sdk/README.md`](crates/open-functions-sdk/README.md)
for complete worked examples (HTTP and CloudEvent/Pub-Sub functions),
structured logging, and the exact steps to deploy the same source unmodified
to real Cloud Run functions.

## Installation

Three deployment paths exist. All of them end with `open-functions serve`
running two listeners: invoke (default `:8080`) and admin (default `:8081`).

### systemd (binary release)

Download the archive and checksum for your architecture (`x86_64` /
`aarch64`; the binary is musl-static, so no glibc is required to run it)
from GitHub Releases and verify it:

```bash
curl -LO https://github.com/siosig/open-functions/releases/download/<ver>/open-functions-<ver>-<target>.tar.gz
curl -LO https://github.com/siosig/open-functions/releases/download/<ver>/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf open-functions-<ver>-<target>.tar.gz
sudo install -m 0755 open-functions /usr/local/bin/open-functions
```

If you hand-roll a systemd unit, set `Type=notify` and `Delegate=yes` (the
latter is needed for the cgroup v2 memory limit). The Ansible role below
deploys a complete unit and config file, which is more reliable than writing
one by hand.

### Docker

```bash
docker network create open-functions   # once, so function containers share it
docker run -d --name open-functions \
  -p 8080:8080 -p 8081:8081 \
  -v open-functions-data:/data \
  -v /var/run/docker.sock:/var/run/docker.sock \
  --group-add "$(getent group docker | cut -d: -f3)" \
  --network open-functions \
  ghcr.io/siosig/open-functions:<ver>
```

Image-mode deployments and container-mode builds need the Docker socket
mounted, plus its GID explicitly granted via `--group-add` (the base image
runs as `nonroot`, so without that GID it can't reach the socket).
Source-mode-only deployments don't need the socket mount at all.

### Ansible (recommended — idempotent)

```bash
cd ansible
ansible-galaxy collection install -r requirements.yml
ansible-playbook -i inventory/hosts.yml site.yml -e open_functions_deploy_mode=systemd   # or docker
```

See [`ansible/README.md`](ansible/README.md) for the full variable
reference (`open_functions_deploy_mode`: `systemd` | `docker`,
`open_functions_build_mode`: `auto` | `host` | `container`, and more) and
`ansible/inventory/hosts.example.yml` for a host layout that co-locates
open-functions with a Pub/Sub-compatible sidecar service. A second run
reports `changed=0`.

## Configuration

Configuration layers, in increasing priority: built-in defaults → TOML
config file → `OPEN_FUNCTIONS__*` environment variables (double underscore
separates config sections, e.g. `OPEN_FUNCTIONS__ADMIN__LISTEN=0.0.0.0:8081`)
→ CLI flags. Unknown keys fail startup rather than being silently ignored.

Key sections: `[invoke]` / `[admin]` (listen addresses, host suffix, admin
token), `[storage]` (`data_dir`), `[build]` (`mode`: `auto` | `host` |
`container`, `cargo_bin`, `timeout_secs`), `[runtime]` (`docker_socket`,
`cgroup`, `max_total_instances`, `stop_grace_secs`), `[pubsub]` (`enabled`,
`base_url`, `project`), `[log]` (`format`, `level`,
`function_ring_buffer_lines`), `[metrics]` (`enabled`), and `[defaults]`
(per-function defaults: `timeout_secs`, `concurrency`, `memory_mib`,
`min_instances`, `max_instances`, `queue_policy`, ...).

Run `open-functions check-config` to validate a config file (and any
environment overrides) without starting the server.

## URL scheme

The invoke listener (default `:8080`) resolves a function name one of two
ways; a matching `Host` header wins over path-prefix routing when both are
possible.

| Scheme | Example | Behavior |
|---|---|---|
| Path prefix | `/hello/world` | Forwarded to function `hello` as `/world` |
| Host header (when `invoke.host_suffix` is set) | `Host: hello.fn.local` | Forwarded to function `hello`, path unchanged |
| Pub/Sub push | `POST /_cf/push/hello` | A push delivery converted to a CloudEvent and forwarded (the only reserved prefix is `_cf`) |

The admin listener (default `:8081`) exposes `/v1/functions/*` (register,
list, describe, delete, build/function logs, stop) and `/healthz`,
`/readyz`, `/metrics`. `/v1/*` requires `Authorization: Bearer <token>`
whenever `admin.listen` is bound to a non-loopback address.

## Managing functions

```bash
open-functions fn deploy <name> --source <dir> | --image <ref> [--trigger-http | --trigger-topic <topic>] [--entry-point <fn>] [...]
open-functions fn list
open-functions fn describe <name>
open-functions fn delete <name> [--wait]
open-functions fn logs <name> [--tail <n>] [--follow]
open-functions fn build-log <name> [--build <id>] [--follow]
open-functions fn stop <name>
```

`--source` builds from a local directory (host `cargo` or a containerized
build, per `build.mode`); `--image` runs a pre-built container image
directly, requiring a reachable Docker daemon. `deploy` follows the build to
completion by default; pass `--no-wait` to return as soon as it's accepted.
Output is a table on a TTY and JSON otherwise (or force either with
`--output json|table`). `OPEN_FUNCTIONS_ADMIN_URL` (default
`http://127.0.0.1:8081`) and `OPEN_FUNCTIONS_ADMIN_TOKEN` configure which
admin API these commands talk to.

## Pub/Sub integration

A `--trigger-topic` function is invoked via Push delivery from a
Pub/Sub-compatible REST service (the sibling project `ps-rs` implements
this locally, with no cloud dependency); `open-functions` converts each Push
delivery into a `google.cloud.pubsub.topic.v1.messagePublished` CloudEvent
before forwarding it to the function, and acks or lets the delivery retry
based on the function's response — identical to how Eventarc delivers
Pub/Sub events to a real Cloud Run function.

## Observability

Structured logs (`format = "json"`) carry `severity`, `time`, `message`, and
for function-originated lines: `source="function"`, `function`, `revision`,
`instance_id`, `execution_id`. `/metrics` exposes Prometheus metrics under
the `open_functions_` prefix: invocation counts/durations, forwarding
overhead, per-function instance counts and cold-start latency, build
results, and Pub/Sub binding state.

## Troubleshooting

- **cgroup memory-limit warning on startup**: cgroup v2 isn't writable in
  this environment (inside Docker without the equivalent of `Delegate=`, or
  a systemd unit without `Delegate=yes`), so memory limiting is deliberately
  disabled. Startup continues; only the per-instance `memory_mib` cap stops
  being enforced.
- **Docker socket permission errors**: image-mode and container-mode builds
  need the running process (or, when open-functions itself runs in Docker,
  the open-functions container) to reach the Docker socket. The Ansible
  Docker deploy mode grants the target host's `docker` group GID
  automatically; a manual Docker run needs the `--group-add` shown above.
- **glibc version mismatches**: a source-mode (host-built) artifact is tied
  to the glibc on the machine that built it — copying it to a host with a
  different glibc generation fails to start. Container-mode builds
  deliberately pair a `rust:1-bookworm` build image with a
  `distroless/cc-debian12` runtime image from the same glibc generation, so
  this doesn't happen. If you need to reuse one build artifact across
  multiple hosts, use container-mode builds or image-mode deployment
  instead.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
