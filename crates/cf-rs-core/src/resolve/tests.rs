use super::*;

fn resolver_with_suffix() -> Resolver {
    Resolver {
        host_suffix: Some("fn.local".to_string()),
    }
}

fn resolver_without_suffix() -> Resolver {
    Resolver { host_suffix: None }
}

#[test]
fn path_prefix_without_rest() {
    let r = resolver_without_suffix();
    assert_eq!(
        r.resolve(None, "/hello"),
        Resolved::PathPrefix {
            function: "hello".to_string(),
            rest_path: "/".to_string(),
        }
    );
}

#[test]
fn path_prefix_with_rest() {
    let r = resolver_without_suffix();
    assert_eq!(
        r.resolve(None, "/hello/world"),
        Resolved::PathPrefix {
            function: "hello".to_string(),
            rest_path: "/world".to_string(),
        }
    );
}

#[test]
fn path_prefix_with_nested_rest() {
    let r = resolver_without_suffix();
    assert_eq!(
        r.resolve(None, "/hello/world/again"),
        Resolved::PathPrefix {
            function: "hello".to_string(),
            rest_path: "/world/again".to_string(),
        }
    );
}

#[test]
fn host_header_takes_priority_over_path() {
    // Host matches "hello.fn.local" while the path also looks like a
    // valid path-prefix match for a *different* function ("world"); the
    // host-based match must win.
    let r = resolver_with_suffix();
    assert_eq!(
        r.resolve(Some("hello.fn.local"), "/world"),
        Resolved::Host {
            function: "hello".to_string(),
        }
    );
}

#[test]
fn host_header_with_port_is_stripped() {
    let r = resolver_with_suffix();
    assert_eq!(
        r.resolve(Some("hello.fn.local:8080"), "/anything"),
        Resolved::Host {
            function: "hello".to_string(),
        }
    );
}

#[test]
fn host_suffix_none_disables_host_matching() {
    let r = resolver_without_suffix();
    // Looks exactly like a host-based match, but host_suffix is None, so it
    // must fall through to path-based resolution instead.
    assert_eq!(
        r.resolve(Some("hello.fn.local"), "/world"),
        Resolved::PathPrefix {
            function: "world".to_string(),
            rest_path: "/".to_string(),
        }
    );
}

#[test]
fn host_suffix_empty_string_disables_host_matching() {
    let r = Resolver {
        host_suffix: Some(String::new()),
    };
    assert_eq!(
        r.resolve(Some("hello."), "/world"),
        Resolved::PathPrefix {
            function: "world".to_string(),
            rest_path: "/".to_string(),
        }
    );
}

#[test]
fn host_header_mismatched_suffix_falls_back_to_path() {
    let r = resolver_with_suffix();
    assert_eq!(
        r.resolve(Some("hello.other.example"), "/world"),
        Resolved::PathPrefix {
            function: "world".to_string(),
            rest_path: "/".to_string(),
        }
    );
}

#[test]
fn cf_push_matches() {
    let r = resolver_without_suffix();
    assert_eq!(
        r.resolve(None, "/_cf/push/hello"),
        Resolved::Push {
            function: "hello".to_string(),
        }
    );
}

#[test]
fn cf_reserved_prefix_other_suffix_is_no_match() {
    let r = resolver_without_suffix();
    assert_eq!(r.resolve(None, "/_cf/whatever"), Resolved::NoMatch);
    assert_eq!(r.resolve(None, "/_cf/"), Resolved::NoMatch);
    assert_eq!(r.resolve(None, "/_cf"), Resolved::NoMatch);
}

#[test]
fn cf_push_with_invalid_name_is_no_match() {
    let r = resolver_without_suffix();
    assert_eq!(r.resolve(None, "/_cf/push/"), Resolved::NoMatch);
    assert_eq!(r.resolve(None, "/_cf/push/HELLO"), Resolved::NoMatch);
}

#[test]
fn invalid_name_uppercase_is_no_match() {
    let r = resolver_without_suffix();
    assert_eq!(r.resolve(None, "/Hello"), Resolved::NoMatch);
}

#[test]
fn invalid_name_empty_is_no_match() {
    let r = resolver_without_suffix();
    // "//rest" -> first segment after leading '/' is "" -> invalid.
    assert_eq!(r.resolve(None, "//rest"), Resolved::NoMatch);
}

#[test]
fn invalid_name_too_long_is_no_match() {
    let r = resolver_without_suffix();
    let long_name = "a".repeat(64);
    let path = format!("/{long_name}");
    assert_eq!(r.resolve(None, &path), Resolved::NoMatch);
}

#[test]
fn name_at_max_length_is_valid() {
    let r = resolver_without_suffix();
    let max_name = "a".repeat(63);
    let path = format!("/{max_name}");
    assert_eq!(
        r.resolve(None, &path),
        Resolved::PathPrefix {
            function: max_name,
            rest_path: "/".to_string(),
        }
    );
}

#[test]
fn root_path_is_no_match() {
    let r = resolver_without_suffix();
    assert_eq!(r.resolve(None, "/"), Resolved::NoMatch);
}

#[test]
fn empty_path_is_no_match() {
    let r = resolver_without_suffix();
    assert_eq!(r.resolve(None, ""), Resolved::NoMatch);
}

#[test]
fn no_host_header_falls_back_to_path_even_with_suffix_configured() {
    let r = resolver_with_suffix();
    assert_eq!(
        r.resolve(None, "/hello"),
        Resolved::PathPrefix {
            function: "hello".to_string(),
            rest_path: "/".to_string(),
        }
    );
}
