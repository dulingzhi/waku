//! Tests for the daemon's argument handling and bind guards.
//!
//! A module of its own rather than an inline `#[cfg(test)] mod tests` so the
//! daemon body stays about running a daemon.

use super::*;

#[test]
fn non_loopback_listener_requires_an_explicit_flag() {
    assert!(ensure_bind_allowed("127.0.0.1:3000".parse().unwrap(), false).is_ok());
    assert!(ensure_bind_allowed("[::1]:3000".parse().unwrap(), false).is_ok());
    assert!(ensure_bind_allowed("0.0.0.0:3000".parse().unwrap(), false).is_err());
    assert!(ensure_bind_allowed("[::]:3000".parse().unwrap(), false).is_err());
    assert!(ensure_bind_allowed("0.0.0.0:3000".parse().unwrap(), true).is_ok());
}

#[test]
fn parses_bind_parent_and_origins() {
    let arguments = Arguments::parse([
        "--bind".into(),
        "127.0.0.1:4312".into(),
        "--parent-pid".into(),
        "99".into(),
        "--allow-origin".into(),
        "https://app.waku.test".into(),
        "--allow-origin".into(),
        "http://localhost:3000".into(),
    ])
    .unwrap();

    assert_eq!(arguments.bind, "127.0.0.1:4312");
    assert_eq!(arguments.parent_pid, Some(99));
    assert_eq!(
        arguments.allowed_origins,
        ["https://app.waku.test", "http://localhost:3000"]
    );
    assert!(!arguments.allow_non_loopback);
}

#[test]
fn parses_explicit_non_loopback_opt_in() {
    let arguments = Arguments::parse(["--allow-non-loopback".into()]).unwrap();
    assert!(arguments.allow_non_loopback);
}
