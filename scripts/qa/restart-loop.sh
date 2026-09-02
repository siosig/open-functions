#!/usr/bin/env bash
#
# scripts/qa/restart-loop.sh
#
# QA stress test for spec.md's SC-006: "restarting the host / recreating the
# container 100 times must not lose a single registered function's
# definition or build artifact, and must not trigger a rebuild."
#
# This script starts a real `cf-rs serve` process against a throwaway
# --data-dir, deploys examples/hello-http as a real (source-mode) function,
# then repeatedly:
#   1. sends SIGTERM to the running `cf-rs serve` process (the same signal
#      the graceful-shutdown contract expects) and waits for it to actually
#      exit,
#   2. relaunches `cf-rs serve` with the *same* --data-dir and ports,
#   3. waits for /readyz, then re-checks every invariant below against the
#      baseline captured right after the initial deploy:
#        - the number of registered functions (GET /v1/functions)
#        - the function's current_revision (GET /v1/functions/<name>)
#        - the SHA256 of the on-disk build artifact
#          (<data-dir>/artifacts/<name>/<revision>/function)
#        - the Prometheus cf_rs_builds_total counter, summed across all of
#          its label combinations (GET /metrics)
#
# Any mismatch fails the script immediately with a diagnostic naming the
# broken invariant and the iteration it broke on.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/qa/restart-loop.sh [ITERATIONS]

Repeatedly SIGTERMs a running `cf-rs serve` process and relaunches it with
the same --data-dir, asserting that across every restart:
  - the number of registered functions is unchanged
  - the deployed function's current_revision is unchanged (no rebuild)
  - the SHA256 of the function's build artifact on disk is unchanged
  - the cf_rs_builds_total Prometheus counter (summed across all label
    combinations) is unchanged

ITERATIONS defaults to 20 restarts, for a fast local sanity check. spec.md's
SC-006 calls for a 100-iteration stress run; run that explicitly with:

    scripts/qa/restart-loop.sh 100

Environment variables:
  CF_RS_BIN         Path to the cf-rs binary. Defaults to
                     $CARGO_TARGET_DIR/release/cf-rs if CARGO_TARGET_DIR is
                     set, else <repo_root>/target/release/cf-rs. Built via
                     `cargo build --release -p cf-rs` first if missing.
  ADMIN_PORT         Admin API port to bind (default 18081).
  INVOKE_PORT         Invoke listener port to bind (default 18080).
  DEPLOY_TIMEOUT_SECS  Seconds to wait for the initial deploy to reach
                       state "ready" (default 600; a cold build of
                       examples/hello-http can take a few minutes).
  READY_TIMEOUT_SECS   Seconds to wait for /readyz after each (re)start
                       (default 30).
  SHUTDOWN_TIMEOUT_SECS  Seconds to wait for the process to exit after
                         SIGTERM (default 45).

Exit codes:
  0  all N restarts completed with every invariant intact
  1  a broken invariant, a failed/unexpected HTTP call, or a process that
     did not exit or become ready within its timeout
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

ITERATIONS="${1:-20}"
if ! [[ "$ITERATIONS" =~ ^[0-9]+$ ]] || [[ "$ITERATIONS" -lt 1 ]]; then
  echo "error: ITERATIONS must be a positive integer, got: ${ITERATIONS}" >&2
  usage >&2
  exit 2
fi

log() {
  printf '[%s] %s\n' "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" "$*"
}

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)

if [[ -n "${CF_RS_BIN:-}" ]]; then
  : # explicit override wins
elif [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
  CF_RS_BIN="${CARGO_TARGET_DIR}/release/cf-rs"
else
  CF_RS_BIN="$REPO_ROOT/target/release/cf-rs"
fi

if [[ ! -x "$CF_RS_BIN" ]]; then
  log "cf-rs binary not found at $CF_RS_BIN, building it first (cargo build --release -p cf-rs)..."
  (cd "$REPO_ROOT" && cargo build --release -p cf-rs) \
    || fail "cargo build --release -p cf-rs failed"
  [[ -x "$CF_RS_BIN" ]] || fail "cf-rs binary still missing at $CF_RS_BIN after building"
fi
log "Using cf-rs binary: $CF_RS_BIN"

HELLO_HTTP_DIR="$REPO_ROOT/examples/hello-http"
[[ -d "$HELLO_HTTP_DIR" ]] || fail "examples/hello-http not found at $HELLO_HTTP_DIR"

ADMIN_PORT="${ADMIN_PORT:-18081}"
INVOKE_PORT="${INVOKE_PORT:-18080}"
DEPLOY_TIMEOUT_SECS="${DEPLOY_TIMEOUT_SECS:-600}"
READY_TIMEOUT_SECS="${READY_TIMEOUT_SECS:-30}"
SHUTDOWN_TIMEOUT_SECS="${SHUTDOWN_TIMEOUT_SECS:-45}"
CURL_TIMEOUT="${DEPLOY_TIMEOUT_SECS}"
FN_NAME="hello"
ADMIN_URL="http://127.0.0.1:${ADMIN_PORT}"
INVOKE_URL="http://127.0.0.1:${INVOKE_PORT}"

port_in_use() {
  local port="$1"
  if (exec 3<>"/dev/tcp/127.0.0.1/${port}") 2>/dev/null; then
    exec 3<&- 2>/dev/null || true
    exec 3>&- 2>/dev/null || true
    return 0
  fi
  return 1
}

if port_in_use "$ADMIN_PORT"; then
  fail "ADMIN_PORT ${ADMIN_PORT} is already in use on 127.0.0.1; set ADMIN_PORT to a free port"
fi
if port_in_use "$INVOKE_PORT"; then
  fail "INVOKE_PORT ${INVOKE_PORT} is already in use on 127.0.0.1; set INVOKE_PORT to a free port"
fi

DATA_DIR=$(mktemp -d "${TMPDIR:-/tmp}/cf-rs-restart-loop.XXXXXX")
SERVE_PID=""

cleanup() {
  local ec=$?
  set +e
  if [[ -n "$SERVE_PID" ]] && kill -0 "$SERVE_PID" 2>/dev/null; then
    log "cleanup: stopping leftover cf-rs serve (pid $SERVE_PID)"
    kill "$SERVE_PID" 2>/dev/null
    wait "$SERVE_PID" 2>/dev/null
  fi
  if [[ -n "${DATA_DIR:-}" && -d "$DATA_DIR" ]]; then
    rm -rf "$DATA_DIR"
  fi
  exit "$ec"
}
trap cleanup EXIT

# Used by wait_for_exit() below to implement a bounded `wait` on a specific
# child pid: bash's `wait <pid>` blocks until that exact process is reaped
# (the only reliable way to observe exit without racing zombie state), and
# a trapped signal makes a blocking `wait` return early, so a background
# watchdog process delivers SIGALRM after the timeout elapses.
ALRM_FIRED=0
on_alrm() { ALRM_FIRED=1; }
trap on_alrm ALRM

wait_for_exit() {
  local pid="$1" timeout_s="$2"
  ALRM_FIRED=0
  ( sleep "$timeout_s"; kill -ALRM $$ 2>/dev/null ) &
  local watchdog=$!
  wait "$pid" 2>/dev/null || true
  kill "$watchdog" 2>/dev/null || true
  wait "$watchdog" 2>/dev/null || true
  if (( ALRM_FIRED )); then
    return 1
  fi
  return 0
}

wait_for_ready() {
  local timeout_s="$1"
  local start=$SECONDS
  local code
  while true; do
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "$ADMIN_URL/readyz" 2>/dev/null || echo "000")
    if [[ "$code" == "200" ]]; then
      return 0
    fi
    if (( SECONDS - start >= timeout_s )); then
      return 1
    fi
    sleep 0.2
  done
}

http_call() {
  # http_call METHOD URL [JSON_BODY]
  # On return, sets HTTP_STATUS and HTTP_BODY. Fails loudly (does not return)
  # if curl itself cannot complete the request (connection error/timeout).
  local method="$1" url="$2" data="${3:-}"
  local tmp
  tmp=$(mktemp)
  if [[ -n "$data" ]]; then
    if ! HTTP_STATUS=$(curl -sS --max-time "$CURL_TIMEOUT" -X "$method" \
        -H 'Content-Type: application/json' -d "$data" \
        -o "$tmp" -w '%{http_code}' "$url"); then
      rm -f "$tmp"
      fail "curl $method $url did not complete (connection error or timeout after ${CURL_TIMEOUT}s)"
    fi
  else
    if ! HTTP_STATUS=$(curl -sS --max-time "$CURL_TIMEOUT" -X "$method" \
        -o "$tmp" -w '%{http_code}' "$url"); then
      rm -f "$tmp"
      fail "curl $method $url did not complete (connection error or timeout after ${CURL_TIMEOUT}s)"
    fi
  fi
  HTTP_BODY=$(cat "$tmp")
  rm -f "$tmp"
}

expect_status() {
  local actual="$1" expected="$2" context="$3" body="$4"
  if [[ "$actual" != "$expected" ]]; then
    fail "$context: expected HTTP $expected but got HTTP $actual. Response body: $body"
  fi
}

start_serve() {
  "$CF_RS_BIN" serve \
    --data-dir "$DATA_DIR" \
    --invoke-listen "127.0.0.1:${INVOKE_PORT}" \
    --admin-listen "127.0.0.1:${ADMIN_PORT}" \
    >>"$DATA_DIR/serve.log" 2>&1 &
  SERVE_PID=$!
}

wait_for_function_ready() {
  local timeout_s="$1"
  local start=$SECONDS
  while true; do
    http_call GET "$ADMIN_URL/v1/functions/$FN_NAME"
    if [[ "$HTTP_STATUS" == "200" ]]; then
      local state
      state=$(printf '%s' "$HTTP_BODY" | jq -r '.state // empty')
      if [[ "$state" == "ready" ]]; then
        return 0
      fi
      if [[ "$state" == "failed" ]]; then
        local err
        err=$(printf '%s' "$HTTP_BODY" | jq -r '.last_error // "unknown"')
        fail "function $FN_NAME entered state \"failed\" while deploying: $err"
      fi
    fi
    if (( SECONDS - start >= timeout_s )); then
      fail "function $FN_NAME did not reach state \"ready\" within ${timeout_s}s. Last response: HTTP $HTTP_STATUS $HTTP_BODY"
    fi
    sleep 1
  done
}

get_function_count() {
  http_call GET "$ADMIN_URL/v1/functions"
  expect_status "$HTTP_STATUS" 200 "GET /v1/functions" "$HTTP_BODY"
  local count
  count=$(printf '%s' "$HTTP_BODY" | jq '.functions | length')
  [[ -n "$count" ]] || fail "GET /v1/functions: could not parse .functions length from: $HTTP_BODY"
  printf '%s' "$count"
}

get_current_revision() {
  http_call GET "$ADMIN_URL/v1/functions/$FN_NAME"
  expect_status "$HTTP_STATUS" 200 "GET /v1/functions/$FN_NAME" "$HTTP_BODY"
  local rev
  rev=$(printf '%s' "$HTTP_BODY" | jq -r '.current_revision // empty')
  if [[ -z "$rev" || "$rev" == "null" ]]; then
    fail "GET /v1/functions/$FN_NAME: no current_revision in response: $HTTP_BODY"
  fi
  printf '%s' "$rev"
}

get_artifact_hash() {
  local revision="$1"
  local path="$DATA_DIR/artifacts/$FN_NAME/$revision/function"
  [[ -f "$path" ]] || fail "expected build artifact at $path but it does not exist"
  sha256sum "$path" | awk '{print $1}'
}

get_builds_total() {
  # cf_rs_builds_total is a labeled counter (function, mode, result per
  # ops-config.md); sum every label combination's value for one comparable
  # total. Prometheus text exposition format lines are "name{labels} value"
  # (two whitespace-separated fields), so summing field 2 across every
  # matching line gives that total. If the metric has not been emitted at
  # all yet (e.g. before it has a first sample), this correctly yields 0,
  # and 0 == 0 across restarts is still a valid "no rebuild" invariant.
  http_call GET "$ADMIN_URL/metrics"
  expect_status "$HTTP_STATUS" 200 "GET /metrics" "$HTTP_BODY"
  printf '%s' "$HTTP_BODY" | awk '/^cf_rs_builds_total/ {sum += $2} END {print sum + 0}'
}

log "Starting cf-rs serve: data-dir=$DATA_DIR invoke=$INVOKE_URL admin=$ADMIN_URL"
start_serve
wait_for_ready "$READY_TIMEOUT_SECS" \
  || fail "cf-rs serve (pid $SERVE_PID) did not become ready within ${READY_TIMEOUT_SECS}s on initial start. Log: $DATA_DIR/serve.log"
log "cf-rs serve is ready (pid $SERVE_PID)"

log "Deploying $FN_NAME from $HELLO_HTTP_DIR (source-mode; this triggers a real cold build and may take a few minutes)"
DEPLOY_BODY=$(jq -n --arg path "$HELLO_HTTP_DIR" \
  '{trigger: {type: "http"}, source: {kind: "dir", path: $path}, entry_point: "hello"}')
http_call PUT "$ADMIN_URL/v1/functions/$FN_NAME" "$DEPLOY_BODY"
expect_status "$HTTP_STATUS" 202 "PUT /v1/functions/$FN_NAME" "$HTTP_BODY"

wait_for_function_ready "$DEPLOY_TIMEOUT_SECS"
log "$FN_NAME is ready"

BASELINE_COUNT=$(get_function_count)
BASELINE_REVISION=$(get_current_revision)
BASELINE_HASH=$(get_artifact_hash "$BASELINE_REVISION")
BASELINE_BUILDS=$(get_builds_total)

log "Baseline captured: functions=$BASELINE_COUNT current_revision=$BASELINE_REVISION artifact_sha256=$BASELINE_HASH cf_rs_builds_total=$BASELINE_BUILDS"
log "Running $ITERATIONS restart iterations..."

for ((i = 1; i <= ITERATIONS; i++)); do
  log "Iteration $i/$ITERATIONS: sending SIGTERM to pid $SERVE_PID"
  kill -TERM "$SERVE_PID" || fail "iteration $i: failed to send SIGTERM to pid $SERVE_PID"

  wait_for_exit "$SERVE_PID" "$SHUTDOWN_TIMEOUT_SECS" \
    || fail "iteration $i: cf-rs serve (pid $SERVE_PID) did not exit within ${SHUTDOWN_TIMEOUT_SECS}s after SIGTERM"

  log "Iteration $i/$ITERATIONS: process exited cleanly, relaunching with the same --data-dir"
  start_serve
  wait_for_ready "$READY_TIMEOUT_SECS" \
    || fail "iteration $i: cf-rs serve (pid $SERVE_PID) did not become ready within ${READY_TIMEOUT_SECS}s after restart. Log: $DATA_DIR/serve.log"

  count=$(get_function_count)
  if [[ "$count" != "$BASELINE_COUNT" ]]; then
    fail "iteration $i: function count changed -- baseline=$BASELINE_COUNT now=$count (a function definition was lost or duplicated)"
  fi

  revision=$(get_current_revision)
  if [[ "$revision" != "$BASELINE_REVISION" ]]; then
    fail "iteration $i: current_revision changed -- baseline=$BASELINE_REVISION now=$revision (a rebuild appears to have been triggered by the restart)"
  fi

  hash=$(get_artifact_hash "$revision")
  if [[ "$hash" != "$BASELINE_HASH" ]]; then
    fail "iteration $i: build artifact sha256 changed -- baseline=$BASELINE_HASH now=$hash (the artifact was rebuilt, replaced, or corrupted)"
  fi

  builds=$(get_builds_total)
  if [[ "$builds" != "$BASELINE_BUILDS" ]]; then
    fail "iteration $i: cf_rs_builds_total changed -- baseline=$BASELINE_BUILDS now=$builds (a rebuild was triggered by the restart)"
  fi

  log "Iteration $i/$ITERATIONS: OK (functions=$count current_revision=$revision artifact_sha256=$hash cf_rs_builds_total=$builds)"
done

log "SUCCESS: $ITERATIONS restart(s) completed. All invariants held: function count, current_revision, build artifact sha256, and cf_rs_builds_total never changed."
exit 0
