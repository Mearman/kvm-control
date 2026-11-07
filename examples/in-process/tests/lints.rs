//! The example is its own Cargo workspace, so it cannot inherit the repository workspace's lint tables and repeats them; this keeps the copies identical.

use std::{fs, path::Path};

/// The `[workspace.lints.*]` tables of a manifest, as their lines from the first such header to the next header outside them, without trailing blank lines.
fn workspace_lints(manifest: &Path) -> Vec<String> {
    let text = fs::read_to_string(manifest).unwrap();
    let mut lines: Vec<String> = text
        .lines()
        .skip_while(|line| !line.starts_with("[workspace.lints."))
        .take_while(|line| !line.starts_with('[') || line.starts_with("[workspace.lints."))
        .map(str::to_owned)
        .collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines
}

#[test]
fn lint_tables_match_the_repository_workspace() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR"));
    let own = workspace_lints(&example.join("Cargo.toml"));
    assert!(own.iter().any(|line| line == "[workspace.lints.rust]"));
    assert!(own.iter().any(|line| line == "[workspace.lints.clippy]"));
    assert_eq!(own, workspace_lints(&example.join("../../Cargo.toml")));
}
