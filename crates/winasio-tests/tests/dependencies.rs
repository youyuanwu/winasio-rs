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

use std::path::{Path, PathBuf};
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
    cargo_tree_pkg("winasio", extra)
}

/// As [`cargo_tree`], but for an arbitrary workspace package. Used to assert the
/// runtime-agnostic story for both `winasio` and `winasio-axum`.
fn cargo_tree_pkg(pkg: &str, extra: &[&str]) -> Option<String> {
    let mut args = vec!["tree", "-p", pkg, "--edges", "normal", "--color", "never"];
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

/// M1/M4/D-A: `winasio-axum` refuses the tokio cost. axum with the `tokio`
/// feature would pull `tokio`, `mio`, `hyper` and `hyper-util` into the normal
/// graph (measured); this crate depends on axum with default features off and no
/// `tokio` feature, so none of those may appear. The check is on `winasio-axum`'s
/// *own* normal graph, so `winasio-tests` enabling `axum/tokio` for its M2 recipe
/// test — a dev-dependency of a different crate — does not perturb it.
#[test]
fn winasio_axum_pulls_in_no_async_runtime() {
    // The four crates D-A refuses, plus the general runtimes.
    const TOKIO_COST: &[&str] = &["tokio", "mio", "hyper", "hyper-util"];

    let Some(tree) = cargo_tree_pkg("winasio-axum", &["--prefix", "none", "--no-dedupe"]) else {
        return;
    };
    assert!(
        tree.contains("winasio-axum"),
        "cargo tree produced nothing useful:\n{tree}"
    );

    for forbidden in RUNTIMES.iter().chain(TOKIO_COST) {
        let leaked = tree
            .lines()
            .any(|line| crate_name(line) == Some(*forbidden));
        assert!(
            !leaked,
            "`{forbidden}` reached winasio-axum's normal dependency graph. \
             winasio-axum must depend on axum with default features off and no \
             `tokio` feature (D-A); check any new dependency.\n{tree}"
        );
    }
}

/// Run `cargo tree` with an explicit `--edges` value for an arbitrary package.
/// Used by the feature-edge scan below, which needs `normal,features` rather
/// than the plain `normal` [`cargo_tree_pkg`] hardcodes.
fn cargo_tree_edges(pkg: &str, edges: &str, extra: &[&str]) -> Option<String> {
    let mut args = vec!["tree", "-p", pkg, "--edges", edges, "--color", "never"];
    args.extend_from_slice(extra);

    let output = Command::new(env!("CARGO"))
        .args(&args)
        .env("CARGO_TERM_COLOR", "never")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output();

    match output {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
        _ => {
            eprintln!("skipping: `cargo tree` unavailable");
            None
        }
    }
}

/// The tonic-feature edge a `cargo tree -e features` line refers to, if any:
/// e.g. `tokio feature "rt"` -> `Some(("tokio", "rt"))`.
fn feature_edge(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start_matches(['│', '├', '└', '─', ' ', '\u{a0}']);
    let (crate_name, rest) = trimmed.split_once(" feature ")?;
    let feature = rest.trim().trim_matches('"');
    Some((crate_name.trim(), feature))
}

/// D2 / FR-013: `winasio-tonic` admits *bare* tokio (features `default` + `sync`,
/// M13) but no async **runtime** — not the tokio `rt`/`rt-multi-thread`/`net`/
/// `macros` features, and not the `mio`/`hyper`/`hyper-util`/`h2` crates a
/// hyper-based transport would drag in.
///
/// Two scans on the *normal* graph, because the interesting bans are of two
/// kinds. `rt`/`net`/`macros` are tokio **features**, not crates, so an
/// `--edges normal` crate-name scan cannot see them — they need
/// `--edges normal,features` and a feature-edge parse. `mio`/`hyper`/… are
/// crates, caught by a crate-name scan of the same graph. Using `normal` (not a
/// bare `-e features`, which would also surface dev/build-dependency features)
/// keeps the winasio-tests build-time codegen stack (tonic-prost-build, the
/// generated *server* stub with its runtime) out of the picture.
#[test]
fn winasio_tonic_pulls_in_no_async_runtime_beyond_tokio() {
    // tokio *features* that would mean a runtime, not just the sync primitives.
    const BANNED_TOKIO_FEATURES: &[&str] = &[
        "rt",
        "rt-multi-thread",
        "net",
        "macros",
        "process",
        "signal",
        "time",
    ];
    // crates a hyper transport would pull; none may appear.
    const BANNED_CRATES: &[&str] = &["mio", "hyper", "hyper-util", "h2", "socket2"];

    let Some(tree) = cargo_tree_edges(
        "winasio-tonic",
        "normal,features",
        &["--prefix", "none", "--no-dedupe"],
    ) else {
        return;
    };
    assert!(
        tree.contains("winasio-tonic"),
        "cargo tree produced nothing useful:\n{tree}"
    );

    // Feature-edge scan: bare tokio is allowed, its runtime features are not.
    for line in tree.lines() {
        if let Some((krate, feature)) = feature_edge(line) {
            if krate == "tokio" {
                assert!(
                    !BANNED_TOKIO_FEATURES.contains(&feature),
                    "tokio feature `{feature}` reached winasio-tonic's normal graph. \
                     Only non-runtime tokio (`default`/`sync`) is allowed (D2/M13); a \
                     runtime feature means a reactor leaked in.\n{tree}"
                );
            }
        }
    }

    // Crate scan: the hyper transport crates and general runtimes must be absent.
    for forbidden in RUNTIMES.iter().chain(BANNED_CRATES) {
        // tokio itself is allowed (bare); only the OTHER runtimes are banned.
        if *forbidden == "tokio" {
            continue;
        }
        let leaked = tree
            .lines()
            .filter(|l| feature_edge(l).is_none())
            .any(|line| crate_name(line) == Some(*forbidden));
        assert!(
            !leaked,
            "`{forbidden}` reached winasio-tonic's normal dependency graph. \
             winasio-tonic must not pull a hyper-based transport or a second \
             runtime (D2); check any new dependency.\n{tree}"
        );
    }
}

/// `winasio-util` gained a `futures` normal dependency in Phase 1 but had
/// no runtime-guard of its own; assert its normal graph pulls no async runtime
/// (the same forbidden set as the `winasio` guard). `futures` is not a
/// runtime — it is combinators and executors over `std` (we use only
/// `futures::executor::block_on`) — so it is allowed.
#[test]
fn winasio_util_pulls_in_no_async_runtime() {
    let Some(tree) = cargo_tree_pkg("winasio-util", &["--prefix", "none", "--no-dedupe"]) else {
        return;
    };
    assert!(
        tree.contains("winasio-util"),
        "cargo tree produced nothing useful:\n{tree}"
    );

    for runtime in RUNTIMES {
        let leaked = tree.lines().any(|line| crate_name(line) == Some(*runtime));
        assert!(
            !leaked,
            "`{runtime}` reached winasio-util's normal dependency graph. \
             winasio-util must stay runtime-agnostic; check that any new dependency \
             is a dev-dependency rather than a normal one.\n{tree}"
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

#[test]
fn new_file_pipe_sources_do_not_add_unsafe_send_or_sync_impls() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let mut files = Vec::new();
    collect_rs_files(&root.join("crates\\winasio\\src\\fs"), &mut files);
    collect_rs_files(&root.join("crates\\winasio\\src\\pipe"), &mut files);
    files.extend([
        root.join("crates\\winasio\\src\\io.rs"),
        root.join("crates\\winasio\\src\\iocp\\backend.rs"),
        root.join("crates\\winasio\\src\\iocp\\handle.rs"),
        root.join("crates\\winasio\\src\\iocp\\ops\\stream.rs"),
    ]);

    let mut offenders = Vec::new();
    for file in files {
        let source = std::fs::read_to_string(&file).unwrap();
        for (line_index, line) in source.lines().enumerate() {
            if is_unsafe_send_or_sync_impl(line) {
                offenders.push(format!("{}:{}", file.display(), line_index + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "new file/pipe sources must not add unsafe impl Send/Sync; found:\n{}",
        offenders.join("\n")
    );
}

fn collect_rs_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_rs_files(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

fn is_unsafe_send_or_sync_impl(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("unsafe impl")
        && (line.contains(" Send for ")
            || line.contains(" Send for")
            || line.contains(" Sync for ")
            || line.contains(" Sync for"))
}
