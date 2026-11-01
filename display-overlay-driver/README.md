# Natively display-overlay driver

A Windows **display driver** (KMD) that presents the Natively stealth overlay to a
**GPU video-overlay / scanout plane below or beside the DWM desktop** — so the overlay
is visible on the physical scanout but **has no window** for `EnumWindows` to find and
its pixels are **not in the DWM desktop surface** a proctor's `DXGI Desktop Duplication`
reads. This is **Tier C** of the stealth-overlay existence defense (the only tier where
`EnumWindows` finds nothing). See `../../CLAUDE.md` for the cross-platform contract and
the plan at `.claude/plans/ethereal-herding-snowflake.md`.

## Phases

| Phase | What | Status |
| --- | --- | --- |
| 0 | Feasibility PoC (user-mode flip-model swap chain) | **Done — No-Go for a plain flip chain** (see `../display-overlay-poc/`) |
| A | KMD build/sign/load/IPC pipeline probe | **Done — proven** (this dir, `spike-a/`) |
| B | Overlay-plane compositing mechanism (driver go/no-go) | **In progress — M1 (user-mode `FLIP_SEQUENTIAL`) is No-Go on this Intel Arc** (windowless not creatable, `0x887A0001`); proceeding to **M2 (WDDM display KMD)**. See `phase-b/DESIGN.md`. |
| C | Production KMD (harden the spike, real CA signing → no-reboot load) | Planned |
| D | OSR overlay render → texture hand-off to the driver | Planned |
| E | Input without a window (`WH_MOUSE_LL` rect routing + drag) | Planned |
| F | System-IL deployment (disguised helper service) | Planned |
| G | (optional) Electron as SYSTEM + UIAccess | Planned |
| H | macOS GPU overlay (needs a Mac; CoreDisplay/IOSurface or a display extension) | Planned |

The macOS branch (Phase H) is a separate track: this whole directory is Windows-only.

## spike-a/ — Phase A pipeline (PROVEN)

A minimal WDM KMD (`NativelyOverlay.sys`) + user-mode client that round-trips
`IOCTL_NOV_ECHO` (client sends `0x1234`, driver returns `0x1235`). This proves the full
**build → sign → trust → load → user-mode↔kernel IPC** pipeline works on Windows, which
is the hard foundation Phase B builds on.

**Proven result** (log: `spike-a/load-postreboot.log`): the driver reaches
`STATE: 4 RUNNING` and the client prints
`sent 0x00001234, got 0x00001235 -> ROUND-TRIP OK (driver pipeline works)`, exit 0.

### Layout

- `driver.c` — WDM KMD. `DriverEntry` creates `\\Device\\NativelyOverlay` + the
  `\\DosDevices\\NativelyOverlay` symlink; `IRP_MJ_DEVICE_CONTROL` handles
  `IOCTL_NOV_ECHO` (returns input+1 on the `SystemBuffer`).
- `overlay_ioctl.h` — shared `IOCTL_NOV_ECHO` definition (used by driver + client).
- `client/client.c` — opens `\\.\NativelyOverlay`, issues the echo IOCTL, exits 0 on OK.
- `driver.vcxproj` — WDM project, `WindowsKernelModeDriver10.0` toolset,
  `PackageReference Microsoft.Windows.WDK.x64 10.0.26100.6584`.
- `wdk-toolset/Toolset.{props,targets}` — a reconstructed WDK MSBuild shim. **Why it
  exists:** the VS WDK workload cannot install on this box (broken VS instance DB), so
  `build.ps1` deploys this shim into the v170 VCTargetsPath to provide the
  `WindowsDriver.*` MSBuild props/targets the project needs.
- `feed/*.nupkg` — the **pinned offline NuGet feed** the WDK restore uses. **Git-ignored**
  (304MB of binary nupkgs; committed it would break `git push` with an HTTP 408). It is a
  build input, not source — recreate it before the first build (see "Offline NuGet feed"
  below).
- `build.ps1` — import vcvars x64, restore the WDK NuGet (offline from `./feed`), locate
  the extracted `c\` content root, deploy the `wdk-toolset/` shim, build the KMD via
  MSBuild, build the client via `cl`.
- `load.ps1` — sign (WDK test cert + DigiCert RFC3161 timestamp), trust the root in
  `LocalMachine\Root`, check test-signing is on, install+start the driver service, run
  the client. `-Cleanup` stops + deletes the service.
- `finish-postreboot.ps1` — one-shot scheduled-task finisher (re-runs `load.ps1`, tees to
  `load-postreboot.log`, then self-disables the task). Used to complete the load after the
  mandatory test-signing reboot.

### Offline NuGet feed (recreate before the first build)

`spike-a/feed/` is a flat local NuGet feed (3 pinned packages, ~304MB) that `build.ps1`
restores the WDK from **offline** (this box's link to nuget.org is flaky; `NuGet.config`
points restore at it with `<clear />`). The nupkgs are **git-ignored** — 304MB of binary
in git would break `git push` (HTTP 408) — so recreate the feed on a fresh checkout. The
WDK package has two transitive `SDK.Cpp` deps that must also be in the flat feed:

```powershell
cd spike-a
nuget install Microsoft.Windows.WDK.x64     -Version 10.0.26100.6584 -OutputDirectory feed
nuget install Microsoft.Windows.SDK.Cpp.x64 -Version 10.0.26100.1   -OutputDirectory feed
nuget install Microsoft.Windows.SDK.Cpp     -Version 10.0.26100.1   -OutputDirectory feed
```

(If nuget.org is unreachable on the machine too, copy `feed/` from one that has it — the
pinned versions are fixed, so any machine's `feed/` is interchangeable.)

### Build + load

```powershell
# 1. Build the KMD + client (creates bin\x64\Release\NativelyOverlay.sys + client.exe)
powershell -ExecutionPolicy Bypass -File spike-a\build.ps1

# 2. Sign + trust + load + run the client (needs an elevated shell; driver must be
#    test-signed and the box must be in test-signing mode - see below)
powershell -ExecutionPolicy Bypass -File spike-a\load.ps1

# 3. Remove the spike service
powershell -ExecutionPolicy Bypass -File spike-a\load.ps1 -Cleanup
```

### The one gate: test-signing mode is a boot-time BCD flag

A **test-signed** driver loads only when the kernel was booted with test-signing ON.
`testsigning` is read from BCD **once at boot** — there is **no runtime toggle**. So the
first `load.ps1` run on a fresh box prints
`!!! TEST-SIGNING IS OFF. Enabling it requires a REBOOT. !!!`, sets
`bcdedit /set testsigning on` (leaving Secure Boot off), and exits 20. After the reboot,
re-run `load.ps1` and it loads (STATE RUNNING) + round-trips. This is **expected and
correct**, not a failure. (The desktop shows a "Test Mode" watermark while test-signing
is on.)

- **Tear down test-signing later:** `bcdedit /set testsigning off` + reboot.
- **The only no-reboot path** is **production-signing** with a real code-signing cert
  chaining to a standard (non-test) CA root — that is Phase C.

### Why the WDK test cert, not a hand-rolled self-signed one

`load.ps1` signs with the WDK's own per-user test cert (`CN="WDKTestCert <user>",...`).
On this box, `signtool` will only take a **natively-created** cert — hand-rolled
`New-SelfSignedCertificate` (CNG) and `.NET` `CertificateRequest`+`X509Store.Add` self-
signed keys (even CAPI `AT_SIGNATURE`) are rejected with "No certificates were found that
met all the given criteria." That is a CAPI key-association quirk of this machine, not a
key-spec issue (see `spike-a/noreboot-test.ps1` for the failed attempt).

## display-overlay-poc/ (sibling) — Phase 0

Standalone Rust crate (`cargo build --release`). Proves a plain **flip-model swap chain**
cannot place a rect of pixels *below* the DWM desktop: the windowed flip's HWND is
enumerable by `EnumWindows` (the existence tell survives) and, being DWM-composited, its
pixels live in the captured DWM desktop surface. Exclusive-fullscreen flip (DirectFlip)
is not creatable on this Intel Arc iGPU (`0x887a0001`). → the path forward is a hardware
video-overlay plane / display driver (Phases B–C).

A second binary, `src/bin/phase-b-flipseq.rs` (Phase B / M1 probe), tests the
`FLIP_SEQUENTIAL` "video overlay" swap effect — the one user-mode path that *might* route
pixels to a post-DWM GPU overlay plane without a driver. **Result on this box:** the
windowless (fullscreen) variant fails to create with the *same* `0x887A0001` as DirectFlip
(so no user-mode overlay plane exists here), and the windowed variant is a plain
DWM-composited flip chain (its pixels are in the captured surface + it owns a window).
→ **M1 is No-Go for a windowless Tier C overlay on this Intel Arc**; the overlay plane must
come from **M2 (a WDDM display KMD)**. Full findings: `phase-b/DESIGN.md`.

## Environment note

This dev box is a **headless/virtual display** (real Intel Arc iGPU, one output, no
physical monitor EDID): local GDI BitBlt returns wallpaper only and `DXGI Desktop
Duplication` produces no frames. So "visible-on-physical-scanout" and "absent-from-DXGI"
**cannot be physically verified here** — they require re-running on a real (non-remote)
display. `EnumWindows` still sees all real windows, confirming the process is on the
interactive desktop.