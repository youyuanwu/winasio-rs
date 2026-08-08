// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Single source of truth for the HTTPS integration-test binding, shared with
//! the setup script.
//!
//! The port the certificate is bound to lives in exactly one file,
//! [`scripts/https-test-config.ps1`](../../../../scripts/https-test-config.ps1),
//! which both `scripts/setup-https-test.ps1` (which binds the certificate) and
//! this module consume — so the two cannot drift into binding one port and
//! connecting to another. The file is embedded here with [`include_str!`], so a
//! moved or renamed config file is a **compile** error, and a malformed one is a
//! **panic** with a clear message rather than a silent fallback to a stale port.

/// The verbatim contents of the single-source config, embedded at compile time.
///
/// Path is relative to this file: `common` → `tests` → `winasio-tests` →
/// `crates` → repo root → `scripts`.
const CONFIG_PS1: &str = include_str!("../../../../scripts/https-test-config.ps1");

/// The TCP port the HTTPS test certificate is bound to, parsed from the single
/// source of truth ([`CONFIG_PS1`]).
///
/// Panics if the `$HttpsTestPort = <n>` assignment is missing or unparseable —
/// that means the config file's format drifted from what the setup script and
/// this parser agree on, and failing loudly is far better than silently testing
/// the wrong port.
pub fn https_test_port() -> u16 {
    parse_u16_assignment(CONFIG_PS1, "HttpsTestPort").unwrap_or_else(|| {
        panic!(
            "could not parse `$HttpsTestPort = <n>` from scripts/https-test-config.ps1; \
             the single-source config format has drifted from tls_config.rs"
        )
    })
}

/// Extract the integer value of a PowerShell `$<name> = <u16>` assignment from
/// `source`, ignoring surrounding whitespace and trailing comments.
///
/// Kept deliberately small and format-specific: the config file documents the
/// exact `$Name = value` shape this expects. Exposed (rather than private) so
/// the consuming test binary can exercise its edge cases directly.
pub fn parse_u16_assignment(source: &str, name: &str) -> Option<u16> {
    let needle = format!("${name}");
    for line in source.lines() {
        let line = line.trim();
        // Skip comments and unrelated lines fast.
        if line.starts_with('#') || !line.starts_with(&needle) {
            continue;
        }
        // Expect `$Name = value`. Split on the first '=' after the name.
        let (lhs, rhs) = line.split_once('=')?;
        if lhs.trim() != needle {
            // e.g. `$HttpsTestPortSomethingElse` — not our assignment.
            continue;
        }
        // Take the leading digit run of the value (tolerate a trailing comment).
        let digits: String = rhs
            .trim()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(port) = digits.parse::<u16>() {
            return Some(port);
        }
    }
    None
}
