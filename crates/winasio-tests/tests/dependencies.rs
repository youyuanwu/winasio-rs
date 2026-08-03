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

#[test]
fn the_library_pulls_in_no_async_runtime() {
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "-p",
            "winasio",
            "--edges",
            "normal",
            "--prefix",
            "none",
            "--no-dedupe",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        // `cargo tree` is unavailable or failed; do not fail the suite over
        // tooling that is not the subject of the test.
        _ => {
            eprintln!("skipping: `cargo tree` unavailable");
            return;
        }
    };

    let tree = String::from_utf8_lossy(&output.stdout);
    assert!(
        tree.contains("winasio"),
        "cargo tree produced nothing useful:\n{tree}"
    );

    for runtime in RUNTIMES {
        let leaked = tree
            .lines()
            .map(str::trim)
            .any(|line| line.split_whitespace().next() == Some(*runtime));
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
    let output = Command::new(env!("CARGO"))
        .args(["tree", "-p", "winasio", "--edges", "normal", "--depth", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => {
            eprintln!("skipping: `cargo tree` unavailable");
            return;
        }
    };

    let tree = String::from_utf8_lossy(&output.stdout);
    let direct: Vec<&str> = tree
        .lines()
        .skip(1) // the root
        .filter_map(|l| {
            let t = l.trim_start_matches(['│', '├', '└', '─', ' ']);
            t.split_whitespace().next()
        })
        .collect();

    for dep in &direct {
        assert!(
            *dep == "windows",
            "unexpected direct dependency `{dep}`; the library should depend \
             only on `windows`.\n{tree}"
        );
    }
}
