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

    Elevation: when run interactively without administrator rights, the script
    relaunches itself elevated and Windows shows a UAC prompt for you to approve.
    In a non-interactive context, or with `-NoElevate` (which CI passes), it does
    NOT prompt and instead fails loudly with a non-zero exit code so a hard CI
    gate surfaces the problem rather than hanging.

.PARAMETER Uninstall
    Remove the binding, the certificate, and its CNG key container. Leaving
    global machine state behind is unacceptable, so teardown is first-class.

.PARAMETER NoElevate
    Do not attempt to self-elevate via UAC; if not already elevated, fail loudly
    with exit code 1. Set automatically on the elevated relaunch to avoid a UAC
    loop, and passed by CI so a headless runner never blocks on a prompt.

.EXAMPLE
    # Interactively, from a NON-elevated PowerShell: approve the UAC prompt.
    pwsh -File scripts/setup-https-test.ps1

.EXAMPLE
    # From an already-elevated PowerShell (no prompt), once per machine:
    pwsh -File scripts/setup-https-test.ps1

.EXAMPLE
    # Clean up completely:
    pwsh -File scripts/setup-https-test.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    [switch]$Uninstall,

    # Internal / CI guard. Set automatically when this script relaunches itself
    # elevated (to prevent a UAC loop), and passed explicitly by CI to force the
    # deterministic loud-fail path instead of attempting an interactive UAC
    # prompt on a headless runner (which would fail or hang).
    [switch]$NoElevate
)

$ErrorActionPreference = 'Stop'

# --- Single source of truth: port / AppId / subject / friendly name ----------
. "$PSScriptRoot\https-test-config.ps1"

# --- Elevation: self-elevate via UAC when interactive, else fail loudly ------
function Test-IsAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

if (-not (Test-IsAdministrator)) {
    # Never attempt an interactive UAC prompt when we were told not to
    # (`-NoElevate`, e.g. from CI) or when there is no interactive desktop to
    # show the prompt on. In those cases fail loudly and deterministically so a
    # hard CI gate surfaces the problem rather than hanging.
    if ($NoElevate -or -not [Environment]::UserInteractive) {
        # Write to stderr WITHOUT throwing (Write-Error under -Stop would abort
        # before the explicit non-zero exit below), so the exit code is
        # deterministic for a caller that scripts on top of this.
        [Console]::Error.WriteLine(@"
ERROR: This script configures machine-wide state (an HTTP.sys SSL binding and a
LocalMachine\My certificate) and REQUIRES administrator rights.

Re-run it from an elevated PowerShell:
    Start an *Administrator* PowerShell, then:
    pwsh -File scripts/setup-https-test.ps1
"@)
        exit 1
    }

    # Interactive: relaunch this exact script elevated. Windows shows a UAC
    # prompt the user approves (or declines). Relaunch with the same host
    # (pwsh.exe or powershell.exe), `-NoElevate` to break any loop, and forward
    # `-Uninstall` if set.
    $hostExe = (Get-Process -Id $PID).Path
    if ([string]::IsNullOrWhiteSpace($hostExe)) { $hostExe = 'pwsh' }
    $childArgs = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $PSCommandPath, '-NoElevate')
    if ($Uninstall) { $childArgs += '-Uninstall' }

    Write-Host "This step needs administrator rights. A Windows UAC prompt will appear -- approve it to continue."
    try {
        $proc = Start-Process -FilePath $hostExe -ArgumentList $childArgs -Verb RunAs -Wait -PassThru
    } catch {
        [Console]::Error.WriteLine("ERROR: elevation was declined or could not start: $($_.Exception.Message)")
        exit 1
    }

    $code = $proc.ExitCode
    if ($code -eq 0 -and -not $Uninstall) {
        # The elevated child ran in its own window that has now closed; show the
        # result here using a read that is permitted UNELEVATED (C7).
        Write-Host ""
        Write-Host "Elevated provisioning completed. Current SSL binding (read unelevated):"
        netsh http show sslcert ipport=("0.0.0.0:$HttpsTestPort") 2>&1 | Write-Host
    } elseif ($code -ne 0) {
        [Console]::Error.WriteLine("The elevated setup process exited with code $code.")
    }
    exit $code
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
