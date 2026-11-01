# Phase A load: test-sign the spike driver, enable test-signing if needed, load
# it as a kernel driver service, run the client, and (with -Cleanup) remove it.
# Run from an elevated shell.
#
#   powershell -ExecutionPolicy Bypass -File load.ps1            # sign + load + run
#   powershell -ExecutionPolicy Bypass -File load.ps1 -Cleanup    # stop + remove service

param(
    [switch]$Cleanup,
    [string]$ServiceName = "NativelyOverlaySpike",
    [string]$VsPath = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools"
)
$ErrorActionPreference = "Stop"
$here = $PSScriptRoot
$sys  = Join-Path $here "bin\x64\Release\NativelyOverlay.sys"

function Find-KitTool([string]$name) {
    Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin" -Recurse -Filter $name -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\x64\\' } | Select-Object -First 1 -ExpandProperty FullName
}

if ($Cleanup) {
    Write-Host "Stopping + removing $ServiceName ..."
    & sc.exe stop $ServiceName 2>&1 | Out-Host
    & sc.exe delete $ServiceName 2>&1 | Out-Host
    Write-Host "Cleanup done. (Test-signing mode stays on until `bcdedit /set testsigning off` + reboot.)"
    exit 0
}

if (-not (Test-Path $sys)) { Write-Error "$sys not found - run build.ps1 first."; exit 1 }

# --- 1. Test certificate (the WDK's own test-signing cert) -----------------
# The WDK build (build.ps1) auto-creates a per-user test cert "WDKTestCert <user>"
# in CurrentUser\My with a private key and the code-signing EKU. It is the same cert
# the WDK TestSign step signs with, so it is the one signtool reliably accepts here
# (a hand-rolled New-SelfSignedCertificate key does not always meet signtool's
# key-spec criteria). Prefer it; if it is missing, point at build.ps1.
$cert = Get-ChildItem Cert:\CurrentUser\My |
    Where-Object { $_.Subject -match "WDKTestCert" -and $_.HasPrivateKey } |
    Select-Object -First 1
if (-not $cert) {
    Write-Error "WDK test cert not found in CurrentUser\My. Run build.ps1 first (it creates the WDK test cert), then re-run load.ps1."
    exit 30
}

# Trust it as a machine-level root. The kernel validates a test-signed driver's
# signature against the machine's Trusted Root store at load time, so the root must
# be in LocalMachine\Root (a passing user-mode signtool verify is not the load gate).
$trusted = Get-ChildItem Cert:\LocalMachine\Root -ErrorAction SilentlyContinue |
    Where-Object { $_.Thumbprint -eq $cert.Thumbprint }
if (-not $trusted) {
    $store = New-Object System.Security.Cryptography.X509Certificates.X509Store("Root", "LocalMachine")
    $store.Open([System.Security.Cryptography.X509Certificates.OpenFlags]::ReadWrite)
    $store.Add($cert)
    $store.Close()
    Write-Host "Trusted $($cert.Subject) as a machine root (LocalMachine\Root)."
}

Write-Host "Signing with WDK test cert $($cert.Thumbprint) ($($cert.Subject))"

# --- 2. Sign the driver ----------------------------------------------------
# /tr + /td: RFC3161 timestamp (keeps the signature valid past the cert's expiry).
# If the timestamp server is unreachable, fall back to signing with no timestamp
# (fine while the test cert is still valid). NOTE: /tr and /t are mutually
# exclusive; /tr requires /td; both require /fd.
$signtool = Find-KitTool "signtool.exe"
if (-not $signtool) { throw "signtool.exe not found" }
& $signtool sign /sha1 $cert.Thumbprint /fd SHA256 /td SHA256 /tr http://timestamp.digicert.com $sys 2>&1 | Out-Host
if ($LASTEXITCODE -ne 0) {
    Write-Host "(timestamp server unreachable - retrying without a timestamp)"
    & $signtool sign /sha1 $cert.Thumbprint /fd SHA256 $sys 2>&1 | Out-Host
    if ($LASTEXITCODE -ne 0) { Write-Error "signtool sign failed (exit $LASTEXITCODE)"; exit $LASTEXITCODE }
}

# --- 3. Test-signing enabled? ---------------------------------------------
# bcdedit reports the value as "Yes"/"No" (the line is absent entirely when it is
# the default/off), so match "testsigning  Yes" - not "on".
$ts = (& bcdedit /enum "{current}" 2>$null) -match "testsigning\s+Yes"
if (-not $ts) {
    Write-Host ""
    Write-Host "!!! TEST-SIGNING IS OFF. Enabling it requires a REBOOT. !!!"
    Write-Host "    bcdedit /set testsigning on   (then reboot, then re-run load.ps1)"
    & bcdedit /set testsigning on
    exit 20
}

# --- 4. Install + start the kernel driver service --------------------------
# Idempotent: stop + delete any stale registration from a prior run so `sc create`
# never fails with "service already exists" (a not-found stop/delete on a fresh run
# is expected and swallowed).
& sc.exe stop $ServiceName 2>&1 | Out-Host
& sc.exe delete $ServiceName 2>&1 | Out-Host
Write-Host "Installing $ServiceName -> $sys ..."
& sc.exe create $ServiceName type= kernel binPath= "$sys" start= demand 2>&1 | Out-Host
if ($LASTEXITCODE -ne 0) { Write-Error "sc create failed (exit $LASTEXITCODE)"; exit $LASTEXITCODE }
& sc.exe start $ServiceName 2>&1 | Out-Host
Start-Sleep -Milliseconds 300

# --- 5. Run the client -----------------------------------------------------
$exe = Join-Path $here "client\client.exe"
& $exe
$rc = $LASTEXITCODE
Write-Host "client exit code: $rc"
exit $rc