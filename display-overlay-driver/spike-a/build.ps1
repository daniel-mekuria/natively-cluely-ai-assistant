# Builds the Phase A spike: the KMD (via MSBuild + the WDK NuGet package) and the
# user-mode client (via cl). Run from an elevated shell. Does NOT sign/load —
# see load.ps1.
#
#   powershell -ExecutionPolicy Bypass -File build.ps1
#
# WDK note: the VS WDK *workload* is not installed on this box (the VS instance DB
# is broken, so `vs_installer modify --add ...Workload.WDK` can't attach). Instead the
# WDK is consumed as the Microsoft.Windows.WDK.x64 NuGet package, restored into the
# NuGet cache (offline, from the local ./feed). It supplies the km headers/libs and
# the WindowsDriver.* build files under WDKContentRoot.
#
# The one thing the NuGet package does NOT ship is the WindowsKernelModeDriver10.0
# MSBuild *toolset shim* (Toolset.props/Toolset.targets) that the VS WDK workload
# normally installs — MSBuild's toolset discovery needs it to resolve the
# PlatformToolset. This script reconstructs that shim (wdk-toolset/) and copies it
# into the v170 VCTargetsPath so the PlatformToolset resolves. The C++ compiler (cl)
# still comes from the VS BuildTools (vcvars); ucrt/um/shared from the global Windows SDK.

param([string]$VsPath = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools")

$ErrorActionPreference = "Stop"
$here = $PSScriptRoot

# --- 0. Import the VS x64 compiler env (puts cl on PATH, sets WindowsSdkDir) ---
$vcvars = Join-Path $VsPath "VC\Auxiliary\Build\vcvarsall.bat"
if (-not (Test-Path $vcvars)) { throw "vcvarsall.bat not found at $vcvars" }
Write-Host "Importing env from $vcvars (x64)..."
$envDump = cmd /c "`"$vcvars`" x64 >NUL 2>&1 && set"
foreach ($line in $envDump) {
    if ($line -match '^([^=]+)=(.*)$') {
        [Environment]::SetEnvironmentVariable($matches[1], $matches[2])
    }
}
Write-Host "cl resolved: $(if (Get-Command cl -ErrorAction SilentlyContinue) { (Get-Command cl).Source } else { 'NOT FOUND' })"
Write-Host "WindowsSdkDir=$env:WindowsSdkDir"

# --- 1. Locate MSBuild --------------------------------------------------------
$msbuild = Get-ChildItem (Join-Path $VsPath "MSBuild\Current\Bin\MSBuild.exe") -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $msbuild) { throw "MSBuild.exe not found under $VsPath" }
Write-Host "MSBuild: $($msbuild.FullName)"

# --- 2. Restore the WDK + SDK NuGet packages ---------------------------------
Write-Host "Restoring WDK NuGet packages (Microsoft.Windows.WDK.x64 + SDK.cpp)..."
& $msbuild.FullName "$here\driver.vcxproj" /t:Restore /p:Configuration=Release /p:Platform=x64 /nologo
if ($LASTEXITCODE -ne 0) { Write-Error "WDK NuGet restore failed (exit $LASTEXITCODE)"; exit $LASTEXITCODE }

# --- 3. Locate the restored WDK package and deploy the toolset shim ----------
# WDKContentRoot must point at the restored package's 'c' folder (where the km/um/shared
# headers/libs and the build\10.0.26100.0\ WindowsDriver.* files live).
$wdkPkgDir = Get-ChildItem "$env:USERPROFILE\.nuget\packages\microsoft.windows.wdk.x64" -Directory -ErrorAction SilentlyContinue |
    Sort-Object Name -Descending | Select-Object -First 1
if (-not $wdkPkgDir) { throw "Restored WDK package not found under $env:USERPROFILE\.nuget\packages\microsoft.windows.wdk.x64" }
$wdkContent = Join-Path $wdkPkgDir.FullName "c"
if (-not (Test-Path (Join-Path $wdkContent "build\10.0.26100.0\WindowsDriver.Default.props"))) {
    throw "WDK package layout unexpected at $wdkContent (missing build\10.0.26100.0)"
}
Write-Host "WDK content: $wdkContent"

# Deploy the reconstructed WindowsKernelModeDriver10.0 toolset shim so the
# PlatformToolset resolves (MSBuild discovers it under the v170 VCTargetsPath).
$toolsetDst = Join-Path $VsPath "MSBuild\Microsoft\VC\v170\Platforms\x64\PlatformToolsets\WindowsKernelModeDriver10.0"
New-Item -ItemType Directory -Force -Path $toolsetDst | Out-Null
Copy-Item (Join-Path $here "wdk-toolset\Toolset.props")   (Join-Path $toolsetDst "Toolset.props")   -Force
Copy-Item (Join-Path $here "wdk-toolset\Toolset.targets") (Join-Path $toolsetDst "Toolset.targets") -Force
Write-Host "Deployed WDK toolset shim to: $toolsetDst"

# --- 4. Build the KMD via MSBuild --------------------------------------------
# /p:WDKContentRoot  — global so it is set before any import (the consumer props set it
#                      too, but global is deterministic and wins). NOTE: must carry a
#                      TRAILING BACKSLASH — every WDK build file references it as
#                      '$(WDKContentRoot)build\...' (no separator), so the package's own
#                      consumer props ('$(NuGetPackageFolder)\c') is off by a slash and
#                      the real WDK workload supplies the slash; we do too.
# /p:WDKBuildFolder  — the WDK 'build\' subfolder (== target platform version). Not set by
#                      the package; the WDK workload normally sets it.
# /p:ResolveNuGetPackages=false — the generic NuGet ResolveNuGetPackageAssets target expects a
#                      .NET TFM (it looks up targets[$(TargetFrameworkMoniker)] and cross-checks
#                      TargetFrameworks), but a classic C++ driver project sets neither, so it throws
#                      ("Sequence contains no elements" / "does not reference 'native' framework").
#                      It is redundant here anyway: the package's only build/native file is a single
#                      .props that sets WDKContentRoot (which we supply globally), UCRTContentRoot and
#                      WDK_NuGet — the latter two are never read by any WDK build file (verified), and
#                      the WDK km headers/libs come from $(WDKContentRoot) via the toolset shim. So the
#                      generic resolve is turned off; all WDK wiring flows through the shim.
$wdkContentRoot = $wdkContent + '\'
& $msbuild.FullName "$here\driver.vcxproj" /p:Configuration=Release /p:Platform=x64 `
    /p:WDKContentRoot="$wdkContentRoot" /p:WDKBuildFolder=10.0.26100.0 `
    /p:ResolveNuGetPackages=false /p:EnableTestSign=false /m /nologo
if ($LASTEXITCODE -ne 0) { Write-Error "KMD build failed (exit $LASTEXITCODE)"; exit $LASTEXITCODE }

$sys = Join-Path $here "bin\x64\Release\NativelyOverlay.sys"
if (-not (Test-Path $sys)) { Write-Error "NativelyOverlay.sys not produced"; exit 11 }
Write-Host "KMD built: $sys"

# --- 5. Build the user-mode client via cl ------------------------------------
Push-Location (Join-Path $here "client")
& cl /nologo /O2 /W3 /Fe:client.exe client.c
$clExit = $LASTEXITCODE
Pop-Location
if ($clExit -ne 0) { Write-Error "client build failed (exit $clExit)"; exit $clExit }

$exe = Join-Path $here "client\client.exe"
if (-not (Test-Path $exe)) { Write-Error "client.exe not produced"; exit 12 }
Write-Host "Client built: $exe"

Write-Host "`nBUILD OK. Next: load.ps1 (test-sign + load + run client)."