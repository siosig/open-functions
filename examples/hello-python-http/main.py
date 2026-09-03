"""Minimal HTTP function using the official `functions-framework` package.

Runs unmodified on open-functions (`open-functions fn deploy hello-py
--source examples/hello-python-http --entry-point hello`) and on Google
Cloud Run functions (deploy this directory as a container or via
`gcloud run deploy --source`). Byte-for-byte behavioral parity with the
Rust reference implementation in `examples/hello-http/src/main.rs`.

Test-only behavior, controlled by env vars (used by open-functions's own
test suite, harmless in normal use):
- `CRASH=1`: exits the process immediately (simulates an instance crash).
- `SLEEP_MS=<n>`: sleeps for `n` milliseconds before responding.
- `FAIL=1`: returns 500 instead of handling the request.
"""

import os
import time

import functions_framework
from flask import Request


@functions_framework.http
def hello(request: Request) -> tuple[str, int] | str:
    """Greets the caller with their request path and query string.

    Args:
        request: The incoming Flask-style HTTP request provided by
            functions-framework.

    Returns:
        A plain-text greeting, or a `(body, status)` tuple when
        simulating a failure via the `FAIL` env var.
    """
    if os.environ.get("CRASH"):
        os._exit(1)

    sleep_ms = os.environ.get("SLEEP_MS")
    if sleep_ms is not None:
        try:
            ms = int(sleep_ms)
        except ValueError:
            ms = None
        if ms is not None:
            time.sleep(ms / 1000)

    if os.environ.get("FAIL"):
        return "simulated failure", 500

    path = request.path
    query = request.query_string.decode("utf-8")
    print(f"handling request path={path} query={query}", flush=True)

    if not query:
        return f"Hello {path}"
    return f"Hello {path}?{query}"
