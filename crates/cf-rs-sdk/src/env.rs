//! Resolution of `PORT` / `FUNCTION_TARGET` / `FUNCTION_SIGNATURE_TYPE` per the
//! Functions Framework Contract.

use crate::Error;

/// Which kind of handler `FUNCTION_SIGNATURE_TYPE` selects, per the
/// Functions Framework contract's `http` / `cloudevent` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureType {
    /// `FUNCTION_SIGNATURE_TYPE=http` (or unset): the target must have been
    /// registered via [`Functions::http`](crate::Functions::http).
    Http,
    /// `FUNCTION_SIGNATURE_TYPE=cloudevent`: the target must have been
    /// registered via [`Functions::cloud_event`](crate::Functions::cloud_event).
    CloudEvent,
}

/// Reads `FUNCTION_TARGET`, defaulting to `"function"` (the Functions
/// Framework's own default) when unset.
pub fn function_target() -> String {
    std::env::var("FUNCTION_TARGET").unwrap_or_else(|_| "function".to_string())
}

/// Reads `FUNCTION_SIGNATURE_TYPE`. Any value other than exactly
/// `"cloudevent"` — including unset, empty, or unrecognized values — is
/// treated as [`SignatureType::Http`], matching the Functions Framework's
/// own permissive default.
pub fn signature_type() -> SignatureType {
    match std::env::var("FUNCTION_SIGNATURE_TYPE").as_deref() {
        Ok("cloudevent") => SignatureType::CloudEvent,
        _ => SignatureType::Http,
    }
}

/// Reads `PORT`, defaulting to `8080` when unset.
///
/// # Errors
///
/// Returns [`Error::InvalidPort`] if `PORT` is set but does not parse as a
/// `u16` (e.g. non-numeric, negative, or greater than 65535).
pub fn port() -> Result<u16, Error> {
    let raw = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    raw.parse::<u16>()
        .map_err(|_| Error::InvalidPort { value: raw })
}
