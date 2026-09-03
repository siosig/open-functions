#!/usr/bin/env bash
#
# scripts/qa/coldstart.sh
#
# QA check for spec.md's process-mode cold-start requirement: after a
# function has been scaled to zero, the very next invocation must get its
# first response byte back in under 1 second.
#
# This script starts a real `open-functions serve` process against a throwaway
# --data-dir, deploys examples/hello-http as a real (source-mode) function,
# warms it up with one request (proving it works, and giving `fn stop`
# something real to scale down from), stops it via `POST .../stop` (forcing
# instances_running to 0, confirmed via `GET /v1/functions/<name>`), then
# issues a single request and asserts curl's own `time_starttransfer` metric
# (time to first byte -- exactly the cold-start latency being measured) is
# under the configured threshold.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/qa/coldstart.sh [FUNCTION_NAME]

Deploys FUNCTION_NAME (default: hello) from SOURCE_DIR as a throwaway
function, scales it to zero, then measures the time-to-first-byte (TTFB) of
the single request that triggers its cold start. Fails if TTFB is not under
1 second.

Environment variables:
  OPEN_FUNCTIONS_BIN             Path to the open-functions binary. Defaults to
                        $CARGO_TARGET_DIR/release/open-functions if CARGO_TARGET_DIR
                        is set, else <repo_root>/target/release/open-functions. Built
                        via `cargo build --release -p open-functions` first if
                        missing.
  SOURCE_DIR             Source directory to deploy (default
                        <repo_root>/examples/hello-http). Set to
                        examples/hello-python-http (with ENTRY_POINT=hello)
                        to measure a Python host-mode cold start instead.
  ENTRY_POINT            entry_point to deploy with (default "hello").
  PYTHON_MODE             When set, exported as
                        OPEN_FUNCTIONS__PYTHON__MODE for the spawned `serve`
                        process (e.g. "container", to force the Python
                        build/run pipeline through Docker instead of
                        "auto"'s host-first preference -- see
                        specs/002-python-runtime/contracts/ops-config.md's
                        [python] section). Unset by default (server default
                        "auto" applies).
  ADMIN_PORT            Admin API port to bind (default 19181).
  INVOKE_PORT           Invoke listener port to bind (default 19180).
  DEPLOY_TIMEOUT_SECS   Seconds to wait for the initial deploy to reach
                        state "ready" (default 600; a cold build of
                        examples/hello-http can take a few minutes).
  READY_TIMEOUT_SECS    Seconds to wait for /readyz after start (default 30).
  STOP_TIMEOUT_SECS     Seconds to wait for instances_running to reach 0
                        after the stop call (default 15).
  COLDSTART_MAX_SECS    Maximum allowed TTFB, in seconds, for the cold-start
                        request (default 1.0).
  COLDSTART_CURL_TIMEOUT_SECS
                        --max-time given to the timed curl call itself, a
                        generous upper bound distinct from the pass/fail
                        threshold above (default 30).

Exit codes:
  0  the cold-start request's TTFB was under COLDSTART_MAX_SECS
  1  TTFB met or exceeded the threshold, or a broken invariant/failed HTTP
     call/setup step
  2  usage error (bad arguments)
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if [[ $# -gt 1 ]]; then
  echo "error: too many arguments" >&2
  usage >&2
  exit 2
fi

FN_NAME="${1:-hello}"

log() {
  printf '[%s] %s\n' "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" "$*"
}

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)

if [[ -n "${OPEN_FUNCTIONS_BIN:-}" ]]; then
  : # explicit override wins
elif [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
  OPEN_FUNCTIONS_BIN="${CARGO_TARGET_DIR}/release/open-functions"
else
  OPEN_FUNCTIONS_BIN="$REPO_ROOT/target/release/open-functions"
fi

if [[ ! -x "$OPEN_FUNCTIONS_BIN" ]]; then
  log "open-functions binary not found at $OPEN_FUNCTIONS_BIN, building it first (cargo build --release -p open-functions)..."
  (cd "$REPO_ROOT" && cargo build --release -p open-functions) \
    || fail "cargo build --release -p open-functions failed"
  [[ -x "$OPEN_FUNCTIONS_BIN" ]] || fail "open-functions binary still missing at $OPEN_FUNCTIONS_BIN after building"
fi
log "Using open-functions binary: $OPEN_FUNCTIONS_BIN"

SOURCE_DIR="${SOURCE_DIR:-$REPO_ROOT/examples/hello-http}"
[[ -d "$SOURCE_DIR" ]] || fail "SOURCE_DIR not found at $SOURCE_DIR"
ENTRY_POINT="${ENTRY_POINT:-hello}"
if [[ -n "${PYTHON_MODE:-}" ]]; then
  export OPEN_FUNCTIONS__PYTHON__MODE="$PYTHON_MODE"
fi

ADMIN_PORT="${ADMIN_PORT:-19181}"
INVOKE_PORT="${INVOKE_PORT:-19180}"
DEPLOY_TIMEOUT_SECS="${DEPLOY_TIMEOUT_SECS:-600}"
READY_TIMEOUT_SECS="${READY_TIMEOUT_SECS:-30}"
STOP_TIMEOUT_SECS="${STOP_TIMEOUT_SECS:-15}"
COLDSTART_MAX_SECS="${COLDSTART_MAX_SECS:-1.0}"
COLDSTART_CURL_TIMEOUT_SECS="${COLDSTART_CURL_TIMEOUT_SECS:-30}"
CURL_TIMEOUT="${DEPLOY_TIMEOUT_SECS}"
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

DATA_DIR=$(mktemp -d "${TMPDIR:-/tmp}/open-functions-coldstart.XXXXXX")
SERVE_PID=""

cleanup() {
  local ec=$?
  set +e
  if [[ -n "$SERVE_PID" ]] && kill -0 "$SERVE_PID" 2>/dev/null; then
    log "cleanup: stopping open-functions serve (pid $SERVE_PID)"
    kill "$SERVE_PID" 2>/dev/null
    wait "$SERVE_PID" 2>/dev/null
  fi
  if [[ -n "${DATA_DIR:-}" && -d "$DATA_DIR" ]]; then
    rm -rf "$DATA_DIR"
  fi
  exit "$ec"
}
trap cleanup EXIT

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
  "$OPEN_FUNCTIONS_BIN" serve \
    --data-dir "$DATA_DIR" \
    --invoke-listen "127.0.0.1:${INVOKE_PORT}" \
    --admin-listen "127.0.0.1:${ADMIN_PORT}" \
    >>"$DATA_DIR/serve.log" 2>&1 &
  SERVE_PID=$!
}

wait_for_function_ready() {
  local fn_name="$1" timeout_s="$2"
  local start=$SECONDS
  while true; do
    http_call GET "$ADMIN_URL/v1/functions/$fn_name"
    if [[ "$HTTP_STATUS" == "200" ]]; then
      local state
      state=$(printf '%s' "$HTTP_BODY" | jq -r '.state // empty')
      if [[ "$state" == "ready" ]]; then
        return 0
      fi
      if [[ "$state" == "failed" ]]; then
        local err
        err=$(printf '%s' "$HTTP_BODY" | jq -r '.last_error // "unknown"')
        fail "function $fn_name entered state \"failed\" while deploying: $err"
      fi
    fi
    if (( SECONDS - start >= timeout_s )); then
      fail "function $fn_name did not reach state \"ready\" within ${timeout_s}s. Last response: HTTP $HTTP_STATUS $HTTP_BODY"
    fi
    sleep 1
  done
}

wait_for_instances_running() {
  local fn_name="$1" want="$2" timeout_s="$3"
  local start=$SECONDS
  while true; do
    http_call GET "$ADMIN_URL/v1/functions/$fn_name"
    expect_status "$HTTP_STATUS" 200 "GET /v1/functions/$fn_name" "$HTTP_BODY"
    local running
    running=$(printf '%s' "$HTTP_BODY" | jq -r '.instances_running // empty')
    if [[ "$running" == "$want" ]]; then
      return 0
    fi
    if (( SECONDS - start >= timeout_s )); then
      fail "function $fn_name: instances_running did not reach $want within ${timeout_s}s (last seen: $running)"
    fi
    sleep 0.2
  done
}

log "Starting open-functions serve: data-dir=$DATA_DIR invoke=$INVOKE_URL admin=$ADMIN_URL"
start_serve
wait_for_ready "$READY_TIMEOUT_SECS" \
  || fail "open-functions serve (pid $SERVE_PID) did not become ready within ${READY_TIMEOUT_SECS}s. Log: $DATA_DIR/serve.log"
log "open-functions serve is ready (pid $SERVE_PID)"

log "Deploying $FN_NAME from $SOURCE_DIR (source-mode; this triggers a real cold build and may take a few minutes)"
DEPLOY_BODY=$(jq -n --arg path "$SOURCE_DIR" --arg entry "$ENTRY_POINT" \
  '{trigger: {type: "http"}, source: {kind: "dir", path: $path}, entry_point: $entry}')
http_call PUT "$ADMIN_URL/v1/functions/$FN_NAME" "$DEPLOY_BODY"
expect_status "$HTTP_STATUS" 202 "PUT /v1/functions/$FN_NAME" "$HTTP_BODY"

wait_for_function_ready "$FN_NAME" "$DEPLOY_TIMEOUT_SECS"
log "$FN_NAME is ready"

log "Warming up $FN_NAME with one request (also gives the stop call a real instance to scale down)"
http_call GET "$INVOKE_URL/$FN_NAME"
expect_status "$HTTP_STATUS" 200 "warm-up GET $INVOKE_URL/$FN_NAME" "$HTTP_BODY"

log "Stopping $FN_NAME (scale to zero)"
http_call POST "$ADMIN_URL/v1/functions/$FN_NAME/stop"
expect_status "$HTTP_STATUS" 200 "POST $ADMIN_URL/v1/functions/$FN_NAME/stop" "$HTTP_BODY"

wait_for_instances_running "$FN_NAME" 0 "$STOP_TIMEOUT_SECS"
log "$FN_NAME confirmed scaled to zero (instances_running == 0)"

log "Issuing single cold-start request to $INVOKE_URL/$FN_NAME"
CURL_RESULT=$(curl -sS -o /dev/null -w '%{http_code} %{time_starttransfer}' \
    --max-time "$COLDSTART_CURL_TIMEOUT_SECS" "$INVOKE_URL/$FN_NAME") \
  || fail "cold-start curl to $INVOKE_URL/$FN_NAME did not complete (connection error or timeout after ${COLDSTART_CURL_TIMEOUT_SECS}s)"
COLDSTART_STATUS="${CURL_RESULT%% *}"
COLDSTART_TTFB="${CURL_RESULT##* }"
expect_status "$COLDSTART_STATUS" 200 "cold-start GET $INVOKE_URL/$FN_NAME (TTFB=${COLDSTART_TTFB}s)" "(no body captured)"

log "Cold-start TTFB: ${COLDSTART_TTFB}s (threshold: < ${COLDSTART_MAX_SECS}s)"
if ! awk -v a="$COLDSTART_TTFB" -v b="$COLDSTART_MAX_SECS" 'BEGIN{exit !(a < b)}'; then
  fail "cold-start TTFB ${COLDSTART_TTFB}s did not meet the < ${COLDSTART_MAX_SECS}s threshold"
fi

log "SUCCESS: cold-start TTFB ${COLDSTART_TTFB}s < ${COLDSTART_MAX_SECS}s"
exit 0
