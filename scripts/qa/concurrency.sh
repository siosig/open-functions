#!/usr/bin/env bash
#
# scripts/qa/concurrency.sh
#
# QA check for spec.md's overload behavior: under heavy concurrent load, a
# function may legitimately reject excess requests, but ONLY with 429
# (RESOURCE_EXHAUSTED) -- never a 5xx, never a dropped/refused connection --
# and open-functions itself must stay alive and responsive throughout.
#
# This script starts a real `open-functions serve` process against a throwaway
# --data-dir, deploys examples/hello-http as a real (source-mode) function,
# then fires a load run at it with `oha` (this repo's chosen load-test tool,
# per quickstart.md's prerequisites). oha's JSON output mode is used because
# its `statusCodeDistribution` / `errorDistribution` objects are far easier
# to parse reliably than its human-readable table. Afterwards it asserts:
#   - every status code oha saw is either 2xx or exactly 429
#   - oha recorded no connection-level errors (refused/reset/timeout)
#   - open-functions (the same pid) is still running and /readyz still returns 200
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/qa/concurrency.sh [FUNCTION_NAME] [CONCURRENCY]

Deploys FUNCTION_NAME (default: hello) from examples/hello-http as a
throwaway function, then runs `oha -c CONCURRENCY` (default: 1000) against
it. Asserts every non-2xx status code oha observed is exactly 429, that oha
recorded no connection-level errors, and that open-functions itself is still alive
and responsive after the load run.

Environment variables:
  OPEN_FUNCTIONS_BIN             Path to the open-functions binary. Defaults to
                        $CARGO_TARGET_DIR/release/open-functions if CARGO_TARGET_DIR
                        is set, else <repo_root>/target/release/open-functions. Built
                        via `cargo build --release -p open-functions` first if
                        missing.
  OHA_BIN               Path to the oha binary. Defaults to `oha` on PATH,
                        installed via `cargo install oha --locked` first if
                        missing.
  ADMIN_PORT             Admin API port to bind (default 19183).
  INVOKE_PORT            Invoke listener port to bind (default 19182).
  DEPLOY_TIMEOUT_SECS    Seconds to wait for the initial deploy to reach
                        state "ready" (default 600; a cold build of
                        examples/hello-http can take a few minutes).
  READY_TIMEOUT_SECS     Seconds to wait for /readyz, both before the load
                        run and again afterwards (default 30).
  OHA_REQUESTS_MULTIPLIER
                        Total request count sent by oha is
                        CONCURRENCY * this multiplier (default 10, per
                        quickstart.md's `scripts/qa/concurrency.sh hello
                        1000` example).
  OHA_REQUEST_TIMEOUT_SECS
                        Per-request timeout given to oha via `-t` (default
                        60; a safety net comfortably above open-functions's default
                        30s queue_max_wait_secs).

Exit codes:
  0  every non-2xx status was exactly 429, no connection errors occurred,
     and open-functions stayed alive and responsive
  1  a disallowed status code or connection error was observed, open-functions did
     not survive the load run, or a broken invariant/failed HTTP
     call/setup step
  2  usage error (bad arguments)
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if [[ $# -gt 2 ]]; then
  echo "error: too many arguments" >&2
  usage >&2
  exit 2
fi

FN_NAME="${1:-hello}"
CONCURRENCY="${2:-1000}"
if ! [[ "$CONCURRENCY" =~ ^[0-9]+$ ]] || [[ "$CONCURRENCY" -lt 1 ]]; then
  echo "error: CONCURRENCY must be a positive integer, got: ${CONCURRENCY}" >&2
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

if [[ -n "${OHA_BIN:-}" ]]; then
  : # explicit override wins
elif command -v oha >/dev/null 2>&1; then
  OHA_BIN="$(command -v oha)"
else
  log "oha not found on PATH, installing it first (cargo install oha --locked)..."
  cargo install oha --locked || fail "cargo install oha --locked failed"
  if command -v oha >/dev/null 2>&1; then
    OHA_BIN="$(command -v oha)"
  else
    fail "oha still not found on PATH after installing it"
  fi
fi
log "Using oha binary: $OHA_BIN"

HELLO_HTTP_DIR="$REPO_ROOT/examples/hello-http"
[[ -d "$HELLO_HTTP_DIR" ]] || fail "examples/hello-http not found at $HELLO_HTTP_DIR"

ADMIN_PORT="${ADMIN_PORT:-19183}"
INVOKE_PORT="${INVOKE_PORT:-19182}"
DEPLOY_TIMEOUT_SECS="${DEPLOY_TIMEOUT_SECS:-600}"
READY_TIMEOUT_SECS="${READY_TIMEOUT_SECS:-30}"
OHA_REQUESTS_MULTIPLIER="${OHA_REQUESTS_MULTIPLIER:-10}"
OHA_REQUEST_TIMEOUT_SECS="${OHA_REQUEST_TIMEOUT_SECS:-60}"
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

DATA_DIR=$(mktemp -d "${TMPDIR:-/tmp}/open-functions-concurrency.XXXXXX")
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

log "Starting open-functions serve: data-dir=$DATA_DIR invoke=$INVOKE_URL admin=$ADMIN_URL"
start_serve
wait_for_ready "$READY_TIMEOUT_SECS" \
  || fail "open-functions serve (pid $SERVE_PID) did not become ready within ${READY_TIMEOUT_SECS}s. Log: $DATA_DIR/serve.log"
log "open-functions serve is ready (pid $SERVE_PID)"

log "Deploying $FN_NAME from $HELLO_HTTP_DIR (source-mode; this triggers a real cold build and may take a few minutes)"
DEPLOY_BODY=$(jq -n --arg path "$HELLO_HTTP_DIR" \
  '{trigger: {type: "http"}, source: {kind: "dir", path: $path}, entry_point: "hello"}')
http_call PUT "$ADMIN_URL/v1/functions/$FN_NAME" "$DEPLOY_BODY"
expect_status "$HTTP_STATUS" 202 "PUT /v1/functions/$FN_NAME" "$HTTP_BODY"

wait_for_function_ready "$FN_NAME" "$DEPLOY_TIMEOUT_SECS"
log "$FN_NAME is ready"

N_REQUESTS=$(( CONCURRENCY * OHA_REQUESTS_MULTIPLIER ))
OHA_OUTPUT="$DATA_DIR/oha.json"
OHA_LOG="$DATA_DIR/oha.log"
log "Running oha: -c $CONCURRENCY -n $N_REQUESTS -t ${OHA_REQUEST_TIMEOUT_SECS}s against $INVOKE_URL/$FN_NAME"
if ! "$OHA_BIN" -c "$CONCURRENCY" -n "$N_REQUESTS" -t "${OHA_REQUEST_TIMEOUT_SECS}s" \
    --no-tui --output-format json "$INVOKE_URL/$FN_NAME" \
    >"$OHA_OUTPUT" 2>"$OHA_LOG"; then
  fail "oha load run did not complete successfully; see $OHA_LOG"
fi

log "Status code distribution: $(jq -c '.statusCodeDistribution' "$OHA_OUTPUT")"
log "Error distribution: $(jq -c '.errorDistribution' "$OHA_OUTPUT")"

BAD_STATUSES=$(jq -r '
  .statusCodeDistribution
  | to_entries[]
  | select((.key | startswith("2") | not) and .key != "429")
  | "\(.key)=\(.value)"
' "$OHA_OUTPUT")
if [[ -n "$BAD_STATUSES" ]]; then
  fail "oha observed non-2xx status code(s) other than 429: $BAD_STATUSES"
fi

CONN_ERRORS=$(jq -r '.errorDistribution | to_entries[] | "\(.key)=\(.value)"' "$OHA_OUTPUT")
if [[ -n "$CONN_ERRORS" ]]; then
  fail "oha observed connection-level error(s) (refused/reset/timeout, not a real HTTP response): $CONN_ERRORS"
fi

log "Load run OK: all responses were 2xx or 429, no connection errors"

if ! kill -0 "$SERVE_PID" 2>/dev/null; then
  fail "open-functions serve (pid $SERVE_PID) is no longer running after the load run"
fi
wait_for_ready "$READY_TIMEOUT_SECS" \
  || fail "open-functions serve (pid $SERVE_PID) did not respond 200 on /readyz within ${READY_TIMEOUT_SECS}s after the load run"

log "SUCCESS: open-functions (pid $SERVE_PID) survived the load run and is still responsive; every non-2xx status seen was exactly 429"
exit 0
