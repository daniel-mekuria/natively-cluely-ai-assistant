# One-shot post-reboot finisher for the Phase A pipeline probe.
#
# After the test-signing reboot, a scheduled task runs this elevated at logon.
# It re-runs load.ps1 (idempotent: re-signs, reinstalls the service, starts it,
# runs the client) and tees ALL of the output to load-postreboot.log, so the
# pipeline result is checkable even if the Claude Code session did not resume.
# ASCII-only (PowerShell 5.1 mis-reads a no-BOM em-dash).
$ErrorActionPreference = "Continue"
$here = $PSScriptRoot
$log  = Join-Path $here "load-postreboot.log"
$load = Join-Path $here "load.ps1"

"=== post-reboot Phase A load started: $(Get-Date) ===" | Out-File $log -Encoding ascii
"testsigning BCD: $(bcdedit /enum '{current}' 2>&1 | Select-String 'testsigning')" | Out-File $log -Append -Encoding ascii

# Child powershell with piped output: Write-Host in the child goes to the pipe,
# so Tee-Object captures everything (a scheduled task's own -RedirectStandardOutput
# would miss Write-Host, which bypasses the process stdout stream).
& powershell -NoProfile -ExecutionPolicy Bypass -File $load 2>&1 | Tee-Object -FilePath $log -Append

"=== post-reboot Phase A load finished: $(Get-Date) ===" | Out-File $log -Append -Encoding ascii
# One-shot: disable the task so it does not fire again on a later logon.
schtasks /Change /TN "NativelyOverlaySpikeFinish" /DISABLE 2>&1 | Out-Null