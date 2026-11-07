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
