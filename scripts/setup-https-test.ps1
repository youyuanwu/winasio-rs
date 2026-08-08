<#
------------------------------------------------------------
Copyright 2023 Youyuan Wu
Licensed under the MIT License (MIT). See License.txt in the repo root for
license information.
------------------------------------------------------------

.SYNOPSIS
    Provision (or tear down) the machine-wide HTTP.sys SSL binding the
    winasio-rs HTTPS integration tests consume.

.DESCRIPTION
    Binding a certificate to an ip:port in the HTTP.sys SSL table is a
    machine-global, administrator-only operation (measured: an unelevated
    `netsh http add sslcert` fails ERROR_ACCESS_DENIED even with a bogus
    thumbprint). Rather than have every test process attempt this at run time,
    this one-time script -- run elevated by a developer, or by CI on an elevated
    runner -- generates a self-signed `localhost` certificate into
    `LocalMachine\My` and binds it to the fixed test port. The tests then run
    UNELEVATED, detect the pre-existing binding, and skip cleanly when it is
    absent.

    Idempotent: re-running deliberately replaces the binding (delete-then-add)
    and reuses an existing test certificate rather than piling up duplicates.

    The port, AppId, subject and friendly name are read from the single source
    of truth, `https-test-config.ps1`, which the Rust tests parse too, so the
    two sides cannot drift.

.PARAMETER Uninstall
    Remove the binding, the certificate, and its CNG key container. Leaving
    global machine state behind is unacceptable, so teardown is first-class.

.EXAMPLE
    # From an elevated PowerShell, once per machine:
    pwsh -File scripts/setup-https-test.ps1

.EXAMPLE
    # Clean up completely:
    pwsh -File scripts/setup-https-test.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'

# --- Single source of truth: port / AppId / subject / friendly name ----------
. "$PSScriptRoot\https-test-config.ps1"

# --- Elevation gate: fail loudly and clearly when not administrator ----------
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    # Write to stderr WITHOUT throwing (Write-Error under -Stop would abort
    # before the explicit non-zero exit below), so the exit code is deterministic
    # for a caller that scripts on top of this.
    [Console]::Error.WriteLine(@"
ERROR: This script configures machine-wide state (an HTTP.sys SSL binding and a
LocalMachine\My certificate) and REQUIRES administrator rights.

Re-run it from an elevated PowerShell:
    Start an *Administrator* PowerShell, then:
    pwsh -File scripts/setup-https-test.ps1
"@)
    exit 1
}

$ipports = @("0.0.0.0:$HttpsTestPort", "[::]:$HttpsTestPort")

function Get-TestCertificates {
    # Match on BOTH subject and our friendly name so we never touch an unrelated
    # CN=localhost certificate a developer may already have installed.
    Get-ChildItem Cert:\LocalMachine\My |
        Where-Object { $_.Subject -eq $HttpsTestSubject -and $_.FriendlyName -eq $HttpsTestFriendlyName }
}

function Remove-TestCertificates {
    foreach ($cert in Get-TestCertificates) {
        # Delete the CNG key container first (the cert's private key), then the
        # certificate itself, so no key container is orphaned.
        try {
            $key = [System.Security.Cryptography.X509Certificates.RSACertificateExtensions]::GetRSAPrivateKey($cert)
            if ($null -ne $key -and $null -ne $key.Key) { $key.Key.Delete() }
        } catch {
            Write-Warning "Could not delete key container for $($cert.Thumbprint): $($_.Exception.Message)"
        }
        Remove-Item -Path ("Cert:\LocalMachine\My\" + $cert.Thumbprint) -Force -ErrorAction SilentlyContinue
    }
}

function Remove-Bindings {
    foreach ($ip in $ipports) {
        # Ignore failure: deleting an absent binding is not an error for us.
        netsh http delete sslcert ipport=$ip 2>&1 | Out-Null
    }
}

# --- Uninstall path ----------------------------------------------------------
if ($Uninstall) {
    Remove-Bindings
    Remove-TestCertificates
    Write-Host "winasio-rs HTTPS test setup REMOVED (port $HttpsTestPort): bindings, certificate and key container cleaned up."
    exit 0
}

# --- Install path ------------------------------------------------------------

# 1. Reuse an existing test certificate if present (idempotent); else create one
#    with a DNS:localhost SAN and a 2048-bit RSA key valid for five years.
$cert = Get-TestCertificates | Select-Object -First 1
if ($null -eq $cert) {
    $cert = New-SelfSignedCertificate `
        -Subject $HttpsTestSubject `
        -DnsName 'localhost' `
        -FriendlyName $HttpsTestFriendlyName `
        -CertStoreLocation 'Cert:\LocalMachine\My' `
        -KeyAlgorithm RSA `
        -KeyLength 2048 `
        -KeyExportPolicy Exportable `
        -NotAfter (Get-Date).AddYears(5)
}
$thumbprint = $cert.Thumbprint

# 2. Deliberately replace any existing binding on our ip:ports, then bind.
foreach ($ip in $ipports) {
    netsh http delete sslcert ipport=$ip 2>&1 | Out-Null
    $out = netsh http add sslcert ipport=$ip certhash=$thumbprint appid=$HttpsTestAppId certstorename=MY 2>&1
    if ($LASTEXITCODE -ne 0) {
        # Non-throwing stderr write so the cleanup below still runs and the exit
        # code is deterministic.
        [Console]::Error.WriteLine("ERROR: netsh http add sslcert failed for $ip (exit $LASTEXITCODE): $($out -join ' ')")
        # Best-effort cleanup so a partial run leaves nothing behind.
        Remove-Bindings
        Remove-TestCertificates
        exit 1
    }
}

Write-Host "winasio-rs HTTPS test setup COMPLETE:"
Write-Host "  Port       : $HttpsTestPort"
Write-Host "  Thumbprint : $thumbprint"
Write-Host "  AppId      : $HttpsTestAppId"
Write-Host "  Subject    : $HttpsTestSubject (DNS:localhost SAN)"
Write-Host ""
Write-Host "The winasio-rs HTTPS tests will now RUN (unelevated). Tear down with:"
Write-Host "  pwsh -File scripts/setup-https-test.ps1 -Uninstall"
