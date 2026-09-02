//! URL resolution: path-prefix and host-header dispatch to a function name.
//!
//! This module is transport-agnostic (no `axum` dependency): the `cf-rs` binary's
//! HTTP layer extracts the `Host` header and request path as plain strings and
//! passes them in. See contracts/admin-api.md's "Invoke listener (:8080)" and
//! plan.md's "Function name and URL" for the exact matching rules implemented here.

#[cfg(test)]
mod tests;

use crate::model::validate::validate_name;

/// Prefix reserved for cf-rs internal routes (e.g. `/_cf/push/<name>`).
const RESERVED_PATH_PREFIX: &str = "/_cf/";

/// Prefix for the Pub/Sub push-delivery route: `/_cf/push/<name>`.
const PUSH_PATH_PREFIX: &str = "/_cf/push/";

/// Result of resolving an inbound request (host header + path) to a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Matched via `/{name}` or `/{name}/{rest}`. `rest_path` is what gets
    /// forwarded to the function (`/` when there is no remainder).
    PathPrefix { function: String, rest_path: String },
    /// Matched via `Host: {name}.{host_suffix}`. The caller forwards the
    /// original path unchanged, so no `rest_path` is carried here.
    Host { function: String },
    /// Matched `/_cf/push/{name}` (Pub/Sub push delivery).
    Push { function: String },
    /// No route matched; caller maps this to an HTTP 404.
    NoMatch,
}

/// Resolves inbound invoke-listener requests to a function name, per the
/// priority order: host-header match > `/_cf/push/*` > other `/_cf/*` (reject)
/// > path-prefix match.
pub struct Resolver {
    /// `invoke.host_suffix` from config. `None` (or empty) disables host-based
    /// resolution entirely.
    pub host_suffix: Option<String>,
}

impl Resolver {
    /// Resolves a request given its `Host` header (if any) and request path
    /// (query string already stripped by the caller).
    pub fn resolve(&self, host_header: Option<&str>, path: &str) -> Resolved {
        if let Some(function) = self.resolve_host(host_header) {
            return Resolved::Host { function };
        }

        if let Some(rest) = path.strip_prefix(PUSH_PATH_PREFIX) {
            return match rest.split('/').next() {
                Some(name) if !name.is_empty() && is_valid_name(name) => Resolved::Push {
                    function: name.to_string(),
                },
                _ => Resolved::NoMatch,
            };
        }

        if path.starts_with(RESERVED_PATH_PREFIX) {
            return Resolved::NoMatch;
        }

        if path.is_empty() || path == "/" {
            return Resolved::NoMatch;
        }

        self.resolve_path_prefix(path)
    }

    /// Host-based match: `Host: {name}.{host_suffix}` (port suffix stripped).
    fn resolve_host(&self, host_header: Option<&str>) -> Option<String> {
        let host_suffix = self.host_suffix.as_deref()?;
        if host_suffix.is_empty() {
            return None;
        }
        let host = host_header?;
        // Strip an optional ":port" suffix. Host headers don't contain '/',
        // so the last ':' (if any) delimits the port.
        let host = match host.rsplit_once(':') {
            Some((h, port)) if port.bytes().all(|b| b.is_ascii_digit()) && !port.is_empty() => h,
            _ => host,
        };

        let suffix_with_dot = format!(".{host_suffix}");
        let name = host.strip_suffix(&suffix_with_dot)?;
        if !name.is_empty() && is_valid_name(name) {
            Some(name.to_string())
        } else {
            None
        }
    }

    /// Path-prefix match: `/{name}` or `/{name}/{rest}`.
    fn resolve_path_prefix(&self, path: &str) -> Resolved {
        // `path` is known to start with '/' and not be exactly "/" here.
        let without_leading_slash = &path[1..];
        let (name, rest_path) = match without_leading_slash.find('/') {
            Some(idx) => (
                &without_leading_slash[..idx],
                without_leading_slash[idx..].to_string(),
            ),
            None => (without_leading_slash, "/".to_string()),
        };

        if is_valid_name(name) {
            Resolved::PathPrefix {
                function: name.to_string(),
                rest_path,
            }
        } else {
            Resolved::NoMatch
        }
    }
}

/// Validates a candidate function name for routing purposes: must match
/// `^[a-z][a-z0-9-]{0,62}$` and must not be the reserved name `_cf`.
///
/// Note: the regex is lowercase-alphanumeric-hyphen only, so it structurally
/// cannot produce a name starting with `_`; the `_cf` check below is kept
/// only as an explicit, self-documenting safeguard, not because it can
/// actually trigger given a shape-valid name.
fn is_valid_name(s: &str) -> bool {
    validate_name(s).is_ok() && s != "_cf"
}
