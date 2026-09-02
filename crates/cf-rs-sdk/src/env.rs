//! Resolution of `PORT` / `FUNCTION_TARGET` / `FUNCTION_SIGNATURE_TYPE` per the
//! Functions Framework Contract.

use crate::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureType {
    Http,
    CloudEvent,
}

pub fn function_target() -> String {
    std::env::var("FUNCTION_TARGET").unwrap_or_else(|_| "function".to_string())
}

pub fn signature_type() -> SignatureType {
    match std::env::var("FUNCTION_SIGNATURE_TYPE").as_deref() {
        Ok("cloudevent") => SignatureType::CloudEvent,
        _ => SignatureType::Http,
    }
}

pub fn port() -> Result<u16, Error> {
    let raw = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    raw.parse::<u16>()
        .map_err(|_| Error::InvalidPort { value: raw })
}
