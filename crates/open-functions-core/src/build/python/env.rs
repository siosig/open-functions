//! Install-step environment allowlisting (T027):
//! `contracts/python-function-contract.md`'s "Virtual environment" step 3
//! env-variable table -- pass through a narrow allowlist of the host's own process env to
//! `uv`/`pip`, then apply the host's own overrides on top so builds are
//! reproducible regardless of what a user has set locally.

use std::collections::BTreeMap;
use std::path::Path;

const ALLOWLIST_PREFIXES: &[&str] = &["UV_", "PIP_"];
const ALLOWLIST_EXACT: &[&str] = &[
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NETRC",
];

/// Builds the env map passed to `uv`/`pip` during dependency install:
/// `host_env`'s `UV_*`/`PIP_*`/proxy/cert/`NETRC` vars are passed through
/// (`UV_PYTHON` is dropped even though it matches the `UV_` prefix -- it
/// would fight the builder's own explicit `--python` selection), then the
/// host's own cache-dir and determinism overrides are applied on top,
/// unconditionally replacing any value `host_env` set for the same key.
pub fn passthrough_env(
    host_env: &BTreeMap<String, String>,
    cache_root: &Path,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for (key, value) in host_env {
        if key == "UV_PYTHON" {
            continue;
        }
        let allowed = ALLOWLIST_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
            || ALLOWLIST_EXACT.contains(&key.as_str());
        if allowed {
            env.insert(key.clone(), value.clone());
        }
    }
    env.insert(
        "UV_CACHE_DIR".to_string(),
        cache_root.join("uv").to_string_lossy().into_owned(),
    );
    env.insert(
        "PIP_CACHE_DIR".to_string(),
        cache_root.join("pip").to_string_lossy().into_owned(),
    );
    env.insert("UV_PYTHON_DOWNLOADS".to_string(), "never".to_string());
    env.insert("UV_NO_MANAGED_PYTHON".to_string(), "1".to_string());
    env.insert("PIP_DISABLE_PIP_VERSION_CHECK".to_string(), "1".to_string());
    env
}
