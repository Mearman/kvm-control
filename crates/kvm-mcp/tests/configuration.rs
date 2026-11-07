//! How `kvm-mcp` reports configuration it cannot start with.

use std::process::{Command, Stdio};

/// Each required variable is named in a plain error, with what to set it to, instead of the bare `NotPresent` of `std::env::var`.
#[test]
fn missing_configuration_names_each_unset_variable() {
    let output = Command::new(env!("CARGO_BIN_EXE_kvm-mcp"))
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with("Error: KVM_ENDPOINT is not set (set it to the device HTTPS URL); "),
        "{stderr}"
    );
    for (name, expected) in [
        ("KVM_PROFILE", "pikvm or glkvm"),
        ("KVM_USERNAME", "the device account name"),
        (
            "KVM_PASSWORD_FD",
            "the number of an inherited pipe descriptor",
        ),
    ] {
        assert!(
            stderr.contains(&format!("{name} is not set (set it to {expected}")),
            "{stderr}"
        );
    }
    assert!(!stderr.contains("NotPresent"), "{stderr}");
}

/// A variable that is not valid Unicode is named, and its value, which may be sensitive, is not printed.
#[cfg(unix)]
#[test]
fn non_unicode_configuration_is_named_without_its_value() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
    let output = Command::new(env!("CARGO_BIN_EXE_kvm-mcp"))
        .env_clear()
        .env("KVM_ENDPOINT", "https://localhost")
        .env("KVM_PROFILE", "pikvm")
        .env("KVM_USERNAME", OsStr::from_bytes(b"synthetic-user-\xff"))
        .env("KVM_PASSWORD_FD", "3")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr, "Error: KVM_USERNAME is not valid Unicode\n");
}

/// An unreadable `KVM_CA_FILE` is reported naming the variable and path, before any password is read or connection made.
#[test]
fn unreadable_ca_file_names_the_variable_and_path() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("missing.pem");
    let output = Command::new(env!("CARGO_BIN_EXE_kvm-mcp"))
        .env_clear()
        .env("KVM_ENDPOINT", "https://localhost")
        .env("KVM_PROFILE", "pikvm")
        .env("KVM_USERNAME", "synthetic")
        .env("KVM_PASSWORD_FD", "3")
        .env("KVM_CA_FILE", &path)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with(&format!(
            "Error: cannot read KVM_CA_FILE {}: ",
            path.display()
        )),
        "{stderr}"
    );
}

/// Runs the server with only `envs` set, expecting it to fail at startup, and returns its standard error. `KVM_PASSWORD_FD` names a descriptor the child does not hold, so a server that read it would fail on that instead.
fn startup_error(envs: &[(&str, &str)]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_kvm-mcp"))
        .env_clear()
        .env("KVM_PROFILE", "pikvm")
        .env("KVM_USERNAME", "synthetic")
        .env("KVM_PASSWORD_FD", "99")
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A CA file is refused while verification is disabled, where it would be ignored unread, before the file or the password is read.
#[test]
fn a_ca_file_is_refused_with_insecure_tls() {
    let stderr = startup_error(&[
        ("KVM_ENDPOINT", "https://localhost:1"),
        ("KVM_CA_FILE", "/nonexistent/ca.pem"),
        ("KVM_INSECURE_TLS", "1"),
    ]);
    assert_eq!(
        stderr,
        "Error: KVM_CA_FILE cannot be used with KVM_INSECURE_TLS=1\n"
    );
}

/// An endpoint the adapter rejects is reported before the password descriptor is read.
#[test]
fn an_invalid_endpoint_is_reported_before_the_password_is_read() {
    let stderr = startup_error(&[("KVM_ENDPOINT", "http://localhost:1")]);
    assert!(stderr.contains("endpoint must be an HTTPS URL"), "{stderr}");
}
