use std::net::{IpAddr, Ipv4Addr};

use http::{HeaderMap, HeaderValue};

use super::*;

/// Builds a `HeaderMap` from name/value literals. `HeaderName::try_from`
/// normalizes case itself, so mixed-case fixture names (used to exercise
/// case-insensitive stripping) are handled correctly. Panics via `panic!`
/// (not `.unwrap()`/`.expect()`, which this crate's lints forbid) if a
/// fixture literal is malformed.
fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        let header_name = match HeaderName::try_from(*name) {
            Ok(n) => n,
            Err(_) => panic!("invalid header name in test fixture: {name}"),
        };
        let header_value = match HeaderValue::from_str(value) {
            Ok(v) => v,
            Err(_) => panic!("invalid header value in test fixture: {value}"),
        };
        headers.insert(header_name, header_value);
    }
    headers
}

fn test_ip() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))
}

mod strip_hop_by_hop_tests {
    use super::*;

    #[test]
    fn removes_static_list_case_insensitively() {
        let mut headers = header_map(&[
            ("Connection", "close"),
            ("KEEP-ALIVE", "timeout=5"),
            ("Transfer-Encoding", "chunked"),
            ("te", "trailers"),
            ("Trailer", "X-Foo"),
            ("Upgrade", "websocket"),
            ("Proxy-Authenticate", "Basic"),
            ("proxy-authorization", "Basic abc"),
            ("Content-Type", "application/json"),
        ]);

        strip_hop_by_hop(&mut headers);

        for name in HOP_BY_HOP_HEADERS {
            assert!(!headers.contains_key(*name), "{name} should be stripped");
        }
        assert_eq!(
            headers.get("content-type"),
            Some(&HeaderValue::from_static("application/json"))
        );
    }

    #[test]
    fn removes_header_dynamically_named_in_connection_value() {
        let mut headers = header_map(&[
            ("Connection", "X-Custom-Header"),
            ("X-Custom-Header", "should-be-removed"),
            ("X-Unrelated", "should-remain"),
        ]);

        strip_hop_by_hop(&mut headers);

        assert!(!headers.contains_key("connection"));
        assert!(!headers.contains_key("x-custom-header"));
        assert_eq!(
            headers.get("x-unrelated"),
            Some(&HeaderValue::from_static("should-remain"))
        );
    }

    #[test]
    fn removes_multiple_headers_named_in_connection_value() {
        let mut headers = header_map(&[
            ("Connection", "X-One, X-Two"),
            ("X-One", "a"),
            ("X-Two", "b"),
            ("X-Three", "c"),
        ]);

        strip_hop_by_hop(&mut headers);

        assert!(!headers.contains_key("x-one"));
        assert!(!headers.contains_key("x-two"));
        assert_eq!(headers.get("x-three"), Some(&HeaderValue::from_static("c")));
    }

    #[test]
    fn leaves_traceparent_and_ordinary_headers_alone() {
        let mut headers = header_map(&[
            (
                "traceparent",
                "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            ),
            ("X-Foo", "bar"),
        ]);

        strip_hop_by_hop(&mut headers);

        assert_eq!(
            headers.get("traceparent").and_then(|v| v.to_str().ok()),
            Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01")
        );
        assert_eq!(headers.get("x-foo"), Some(&HeaderValue::from_static("bar")));
    }
}

mod rewrite_request_headers_tests {
    use super::*;

    fn ctx() -> RequestRewriteContext {
        RequestRewriteContext {
            execution_id: "abc123".to_string(),
            client_addr: test_ip(),
            proto: "http",
            original_host: Some("api.example.com".to_string()),
        }
    }

    #[test]
    fn overwrites_client_supplied_execution_id() {
        let mut headers = header_map(&[("Function-Execution-Id", "client-forged-value")]);

        rewrite_request_headers(&mut headers, &ctx());

        assert_eq!(
            headers.get("function-execution-id"),
            Some(&HeaderValue::from_static("abc123"))
        );
    }

    #[test]
    fn sets_execution_id_when_absent() {
        let mut headers = HeaderMap::new();

        rewrite_request_headers(&mut headers, &ctx());

        assert_eq!(
            headers.get("function-execution-id"),
            Some(&HeaderValue::from_static("abc123"))
        );
    }

    #[test]
    fn appends_to_existing_x_forwarded_for() {
        let mut headers = header_map(&[("X-Forwarded-For", "198.51.100.1")]);

        rewrite_request_headers(&mut headers, &ctx());

        assert_eq!(
            headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()),
            Some("198.51.100.1, 203.0.113.7")
        );
    }

    #[test]
    fn sets_x_forwarded_for_when_absent() {
        let mut headers = HeaderMap::new();

        rewrite_request_headers(&mut headers, &ctx());

        assert_eq!(
            headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()),
            Some("203.0.113.7")
        );
    }

    #[test]
    fn sets_x_forwarded_proto_and_host() {
        let mut headers = HeaderMap::new();

        rewrite_request_headers(&mut headers, &ctx());

        assert_eq!(
            headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok()),
            Some("http")
        );
        assert_eq!(
            headers
                .get("x-forwarded-host")
                .and_then(|v| v.to_str().ok()),
            Some("api.example.com")
        );
    }

    #[test]
    fn omits_x_forwarded_host_when_original_host_absent() {
        let mut headers = HeaderMap::new();
        let mut c = ctx();
        c.original_host = None;

        rewrite_request_headers(&mut headers, &c);

        assert!(!headers.contains_key("x-forwarded-host"));
    }

    #[test]
    fn strips_connection_close_from_client() {
        let mut headers = header_map(&[("Connection", "close")]);

        rewrite_request_headers(&mut headers, &ctx());

        assert!(!headers.contains_key("connection"));
    }

    #[test]
    fn leaves_traceparent_untouched() {
        let mut headers = header_map(&[(
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
        )]);

        rewrite_request_headers(&mut headers, &ctx());

        assert_eq!(
            headers.get("traceparent").and_then(|v| v.to_str().ok()),
            Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01")
        );
    }
}

mod rewrite_response_headers_tests {
    use super::*;

    #[test]
    fn strips_hop_by_hop_and_sets_execution_id() {
        let mut headers = header_map(&[
            ("Connection", "keep-alive"),
            ("Keep-Alive", "timeout=5"),
            ("Content-Type", "application/json"),
        ]);

        rewrite_response_headers(&mut headers, "exec-42");

        assert!(!headers.contains_key("connection"));
        assert!(!headers.contains_key("keep-alive"));
        assert_eq!(
            headers.get("content-type"),
            Some(&HeaderValue::from_static("application/json"))
        );
        assert_eq!(
            headers.get("function-execution-id"),
            Some(&HeaderValue::from_static("exec-42"))
        );
    }

    #[test]
    fn overwrites_preexisting_execution_id() {
        let mut headers = header_map(&[("Function-Execution-Id", "stale")]);

        rewrite_response_headers(&mut headers, "fresh");

        assert_eq!(
            headers.get("function-execution-id"),
            Some(&HeaderValue::from_static("fresh"))
        );
    }
}

mod map_outcome_tests {
    use super::*;

    #[test]
    fn timeout_maps_to_504() {
        assert_eq!(
            map_outcome(ForwardFailure::Timeout),
            ErrorMapping {
                status: 504,
                code: "DEADLINE_EXCEEDED",
            }
        );
    }

    #[test]
    fn connection_refused_maps_to_502() {
        assert_eq!(
            map_outcome(ForwardFailure::ConnectionRefused),
            ErrorMapping {
                status: 502,
                code: "UNAVAILABLE",
            }
        );
    }

    #[test]
    fn connection_reset_maps_to_500() {
        assert_eq!(
            map_outcome(ForwardFailure::ConnectionReset),
            ErrorMapping {
                status: 500,
                code: "INTERNAL",
            }
        );
    }

    #[test]
    fn queue_rejected_maps_to_429() {
        assert_eq!(
            map_outcome(ForwardFailure::QueueRejected),
            ErrorMapping {
                status: 429,
                code: "RESOURCE_EXHAUSTED",
            }
        );
    }
}
