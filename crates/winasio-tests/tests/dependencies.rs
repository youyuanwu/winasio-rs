// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! SC-026 / FR-048: the library depends on no async runtime.
//!
//! This matters most right where it is asserted: the benchmark added
//! `criterion` to the workspace, and `criterion` pulls in a runtime of its own.
//! The guard proves it stayed a development dependency of the *test* crate and
//! did not reach `winasio`.

use std::process::Command;

/// Crates that would mean an async runtime had leaked into the library.
const RUNTIMES: &[&str] = &[
    "tokio",
    "async-std",
    "smol",
    "async-executor",
    "futures-executor",
    "compio",
    "monoio",
    "glommio",
];

/// Run `cargo tree` with the given extra arguments, returning its plain output.
///
/// `--color never` matters: CI sets `CARGO_TERM_COLOR=always`, which would
/// otherwise wrap every crate name in ANSI escapes and defeat the parsing below.
fn cargo_tree(extra: &[&str]) -> Option<String> {
    let mut args = vec![
        "tree", "-p", "winasio", "--edges", "normal", "--color", "never",
    ];
    args.extend_from_slice(extra);

    let output = Command::new(env!("CARGO"))
        .args(&args)
        .env("CARGO_TERM_COLOR", "never")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output();

    match output {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
        // `cargo tree` is unavailable or failed; do not fail the suite over
        // tooling that is not the subject of the test.
        _ => {
            eprintln!("skipping: `cargo tree` unavailable");
            None
        }
    }
}

/// The crate name a `cargo tree` line refers to, ignoring tree drawing.
fn crate_name(line: &str) -> Option<&str> {
    line.trim_start_matches(['│', '├', '└', '─', ' ', '\u{a0}'])
        .split_whitespace()
        .next()
        .filter(|n| !n.is_empty())
}

#[test]
fn the_library_pulls_in_no_async_runtime() {
    let Some(tree) = cargo_tree(&["--prefix", "none", "--no-dedupe"]) else {
        return;
    };
    assert!(
        tree.contains("winasio"),
        "cargo tree produced nothing useful:\n{tree}"
    );

    for runtime in RUNTIMES {
        let leaked = tree.lines().any(|line| crate_name(line) == Some(*runtime));
        assert!(
            !leaked,
            "`{runtime}` reached the library's dependency graph. \
             winasio must stay runtime-agnostic; check that any new dependency \
             is a dev-dependency of winasio-tests rather than of winasio.\n{tree}"
        );
    }
}

/// The library's direct dependencies should stay minimal.
#[test]
fn the_library_depends_only_on_windows() {
    let Some(tree) = cargo_tree(&["--depth", "1"]) else {
        return;
    };

    // Skip the root line; everything after it is a direct dependency.
    let direct: Vec<&str> = tree.lines().skip(1).filter_map(crate_name).collect();

    assert!(
        !direct.is_empty(),
        "expected at least one direct dependency:\n{tree}"
    );
    for dep in &direct {
        assert!(
            *dep == "windows",
            "unexpected direct dependency `{dep}`; the library should depend \
             only on `windows`.\n{tree}"
        );
    }
}
