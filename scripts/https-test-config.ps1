# ------------------------------------------------------------
# Copyright 2023 Youyuan Wu
# Licensed under the MIT License (MIT). See License.txt in the repo root for
# license information.
# ------------------------------------------------------------

# Single source of truth for the winasio-rs HTTPS integration-test binding.
#
# CONSUMED BY:
#   * scripts/setup-https-test.ps1                      -- dot-sources this file
#   * crates/winasio-tests/tests/common/tls_config.rs   -- parses this file at
#     compile time via `include_str!`
#
# Keep each assignment below on its own line in the exact
#   $Name = <value>
# form the Rust parser expects (see `tls_config.rs`). A format change is caught
# loudly: the Rust parser panics rather than silently using a stale default, and
# a drifted port makes the tests skip (no binding found) instead of passing
# against the wrong endpoint.

# The single TCP port the certificate is bound to and the client connects to.
# Chosen clear of every other integration-test port (grep the tests for `124`).
$HttpsTestPort = 12495

# The HTTP.sys AppId that owns the binding. A stable GUID for this crate so the
# uninstall path and any leftover sweep can recognise our own binding (R2).
$HttpsTestAppId = '{c9e7f4a2-3b6d-4e18-9a5c-7d2f0b1e6a34}'

# The self-signed certificate subject. The client requests host `localhost`, so
# the certificate carries `CN=localhost` plus a `DNS:localhost` SAN (added by
# `New-SelfSignedCertificate -DnsName localhost`) to satisfy name validation.
$HttpsTestSubject = 'CN=localhost'

# A friendly name stamped on the certificate so the setup/uninstall scripts can
# find *exactly* our certificate in `LocalMachine\My` without touching any other
# `CN=localhost` certificate a developer may already have.
$HttpsTestFriendlyName = 'winasio-rs-https-test'
