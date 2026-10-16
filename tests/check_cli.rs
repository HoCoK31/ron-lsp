#![cfg(feature = "cli")]

use std::{fs, process::Command};

/// A file without a type annotation must still be syntax-checked.
#[test]
fn check_reports_syntax_errors_without_a_type() {
    let root = std::env::temp_dir().join(format!("ron-check-syntax-{}", std::process::id()));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("data")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "struct Unused { key: String }\n").unwrap();
    // A stray comma is what the old formatter used to leave behind
    fs::write(
        root.join("data/broken.ron"),
        "[\n    // header\n,\n    (key: \"a\"),\n]\n",
    )
    .unwrap();

    let binary = env!("CARGO_BIN_EXE_ron-lsp");
    let output = Command::new(binary)
        .arg("check")
        .arg("data")
        .current_dir(&root)
        .output()
        .unwrap();

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!output.status.success(), "{combined}");
    assert!(
        combined.contains("syntax") || combined.contains("Unexpected"),
        "{combined}"
    );

    fs::remove_dir_all(root).unwrap();
}

/// `ron-lsp check` with a relative path: the workspace root used to collapse to an
/// empty component, so the type scan found nothing and every file passed.
#[test]
fn check_finds_types_with_a_relative_path() {
    let root = std::env::temp_dir().join(format!("ron-check-cli-{}", std::process::id()));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("data")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "#[derive(serde::Deserialize)]\nstruct Entry { key: String }\n",
    )
    .unwrap();
    // The type for a file comes from a ron.toml mapping (as in real projects)
    fs::write(
        root.join("ron.toml"),
        "[[types]]\nglob = \"**/data/*.ron\"\ntype = \"crate::Entry\"\n",
    )
    .unwrap();
    fs::write(root.join("data/entries.ron"), "[\n    (key: 1),\n]\n").unwrap();

    let binary = env!("CARGO_BIN_EXE_ron-lsp");
    let output = Command::new(binary)
        .arg("check")
        .arg("data")
        .current_dir(&root)
        .output()
        .unwrap();

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !combined.contains("Found 0 types in workspace"),
        "{combined}"
    );
    // The type was found and validation ran: `key: 1` is a number, not a string
    assert!(!output.status.success(), "{combined}");
    assert!(combined.contains("Type mismatch"), "{combined}");

    fs::remove_dir_all(root).unwrap();
}
