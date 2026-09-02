#!/usr/bin/env bash
#
# scripts/qa/crash.sh
#
# QA check for spec.md's crash-isolation and self-healing requirements:
# a crashing function must be reported as a clean 500 to its caller, must
# not affect any other function's success rate, and the pool must be able
# to spawn a fresh, working instance the next time the crashed function is
# called.
#
# This script starts a real `cf-rs serve` process against a throwaway
# --data-dir and deploys TWO functions from examples/hello-http: "stable"
# (deployed normally) and "crasher" (deployed with CRASH=1, which per
# examples/hello-http/src/main.rs makes the handler call
# `std::process::exit(1)` on EVERY request, not just the first one).
#
# Sequence:
#   1. call crasher once -- the instance accepts the connection (spawn
#      succeeds; CRASH=1 only fires inside the request handler, per
#      hello-http's own doc comment) and then dies mid-response. Per
#      crates/cf-rs-core/src/forward/mod.rs's ForwardFailure -> status
#      mapping (confirmed against crates/cf-rs-core/src/pool/instance.rs's
#      "Crash detection design" doc and crates/cf-rs/src/forward.rs's
#      is_connect()-based classification), a connection that was accepted
#      but dropped mid-response classifies as ConnectionReset, which maps to
#      HTTP 500 (INTERNAL) -- NOT 502 (502/ConnectionRefused is reserved for
#      a connection that was never accepted at all, e.g. still starting).
#   2. call the unrelated "stable" function 10 times and assert 100% of
#      those calls return 200 -- the crash of one function must not affect
#      another.
#   3. redeploy "crasher" WITHOUT CRASH=1 (CRASH=1 crashes on every request
#      forever, so simply calling it again would just crash it again --
#      redeploying gives new instances a clean env, per the registry's
#      version-switch design), then call it again and assert 200, proving
#      the pool spawns a fresh working instance on demand after a crash was
#      reported dead (InstancePool::report_dead's design).
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/qa/crash.sh

Deploys two functions from examples/hello-http: "stable" (normal) and
"crasher" (CRASH=1, crashes on every request). Calls crasher once and
asserts HTTP 500. Calls stable 10 times and asserts 100% success (200),
proving the crash did not affect it. Redeploys crasher without CRASH=1 and
calls it again, asserting 200 -- proving the pool restarts a fresh instance
on the next call after a crash.

Environment variables:
  CF_RS_BIN             Path to the cf-rs binary. Defaults to
                        $CARGO_TARGET_DIR/release/cf-rs if CARGO_TARGET_DIR
                        is set, else <repo_root>/target/release/cf-rs. Built
                        via `cargo build --release -p cf-rs` first if
                        missing.
  ADMIN_PORT            Admin API port to bind (default 19185).
  INVOKE_PORT           Invoke listener port to bind (default 19184).
  DEPLOY_TIMEOUT_SECS   Seconds to wait for each deploy (initial and
                        redeploy) to reach state "ready" (default 600; a
                        cold build of examples/hello-http can take a few
                        minutes -- the redeploy is normally much faster
                        thanks to cargo's incremental build cache).
  READY_TIMEOUT_SECS    Seconds to wait for /readyz after start (default 30).

Exit codes:
  0  crasher returned 500 on its first call, stable returned 200 on all 10
     calls, and crasher returned 200 after being redeployed clean
  1  any of the above assertions failed, or a broken invariant/failed HTTP
     call/setup step
  2  usage error (bad arguments)
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if [[ $# -gt 0 ]]; then
  echo "error: crash.sh takes no arguments" >&2
  usage >&2
  exit 2
fi

STABLE_FN="stable"
CRASHER_FN="crasher"

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

ADMIN_PORT="${ADMIN_PORT:-19185}"
INVOKE_PORT="${INVOKE_PORT:-19184}"
DEPLOY_TIMEOUT_SECS="${DEPLOY_TIMEOUT_SECS:-600}"
READY_TIMEOUT_SECS="${READY_TIMEOUT_SECS:-30}"
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

DATA_DIR=$(mktemp -d "${TMPDIR:-/tmp}/cf-rs-crash.XXXXXX")
SERVE_PID=""

cleanup() {
  local ec=$?
  set +e
  if [[ -n "$SERVE_PID" ]] && kill -0 "$SERVE_PID" 2>/dev/null; then
    log "cleanup: stopping cf-rs serve (pid $SERVE_PID)"
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
  # Note: this is a call to cf-rs's own listener, which always terminates the
  # request with a real HTTP response (even a crashed backend instance is
  # translated into a status code by cf-rs's forwarder) -- so this only fires
  # for genuine cf-rs-level connectivity problems, not backend crashes.
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

deploy_json() {
  # deploy_json ENV_JSON
  # Builds the PUT /v1/functions/{name} body for examples/hello-http,
  # entry_point "hello", with the given env map (e.g. '{}' or
  # '{"CRASH": "1"}').
  local env_json="$1"
  jq -n --arg path "$HELLO_HTTP_DIR" --argjson env "$env_json" \
    '{trigger: {type: "http"}, source: {kind: "dir", path: $path}, entry_point: "hello", env: $env}'
}

deploy_function() {
  local fn_name="$1" env_json="$2"
  http_call PUT "$ADMIN_URL/v1/functions/$fn_name" "$(deploy_json "$env_json")"
  expect_status "$HTTP_STATUS" 202 "PUT /v1/functions/$fn_name" "$HTTP_BODY"
  wait_for_function_ready "$fn_name" "$DEPLOY_TIMEOUT_SECS"
}

log "Starting cf-rs serve: data-dir=$DATA_DIR invoke=$INVOKE_URL admin=$ADMIN_URL"
start_serve
wait_for_ready "$READY_TIMEOUT_SECS" \
  || fail "cf-rs serve (pid $SERVE_PID) did not become ready within ${READY_TIMEOUT_SECS}s. Log: $DATA_DIR/serve.log"
log "cf-rs serve is ready (pid $SERVE_PID)"

log "Deploying $STABLE_FN from $HELLO_HTTP_DIR (normal env; this triggers a real cold build and may take a few minutes)"
deploy_function "$STABLE_FN" '{}'
log "$STABLE_FN is ready"

log "Deploying $CRASHER_FN from $HELLO_HTTP_DIR (env CRASH=1)"
deploy_function "$CRASHER_FN" '{"CRASH": "1"}'
log "$CRASHER_FN is ready"

log "Calling $CRASHER_FN once (expected to crash mid-request -> connection reset -> HTTP 500)"
http_call GET "$INVOKE_URL/$CRASHER_FN"
expect_status "$HTTP_STATUS" 500 "first call to $CRASHER_FN (crash-during-request)" "$HTTP_BODY"
log "$CRASHER_FN's crashing call correctly returned HTTP 500"

log "Calling $STABLE_FN 10 times, asserting 100% success (crasher's crash must not affect it)"
for i in $(seq 1 10); do
  http_call GET "$INVOKE_URL/$STABLE_FN"
  expect_status "$HTTP_STATUS" 200 "call #$i to $STABLE_FN (after $CRASHER_FN crashed)" "$HTTP_BODY"
done
log "$STABLE_FN: 10/10 calls returned 200 -- unaffected by $CRASHER_FN's crash"

log "Redeploying $CRASHER_FN without CRASH=1 (CRASH=1 crashes on every request, not just once)"
deploy_function "$CRASHER_FN" '{}'
log "$CRASHER_FN redeployed clean and ready"

log "Calling $CRASHER_FN again, expecting the pool to spawn a fresh working instance"
http_call GET "$INVOKE_URL/$CRASHER_FN"
expect_status "$HTTP_STATUS" 200 "second call to $CRASHER_FN (after crash + clean redeploy)" "$HTTP_BODY"
log "$CRASHER_FN's next call succeeded with HTTP 200 -- pool restarted a fresh instance"

log "SUCCESS: crasher returned 500 on crash, stable stayed 100% healthy, and crasher recovered to 200 on its next call"
exit 0
