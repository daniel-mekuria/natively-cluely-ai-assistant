# Phase B — overlay-plane compositing mechanism (driver go/no-go)

Goal: determine and build the mechanism that puts the overlay's pixels on the
**physical scanout at a rect** while keeping them **absent from the DWM desktop
surface** a proctor's `DXGI Desktop Duplication` (or Graphics Capture) reads, and
**with no window** for `EnumWindows`. This is the crux of Tier C; if no mechanism
can satisfy all three (rect + below-DWM + windowless) on the target GPU, Phase B is
**No-Go** and we fall back to Tier A/B (window + capture-exclusion + System IL).

## The property we need, stated precisely

A pixel is *hidden from a same-session admin proctor* iff:
1. It is **not in the surface the proctor captures.** The proctor captures the
   **DWM-composited desktop** (`IDXGIOutputDuplication::AcquireNextFrame` returns
   DWM's output; Graphics Capture is the same DWM surface). So the overlay must be
   composed **by the GPU, after DWM**, on a separate scanout/overlay layer — not by
   DWM. (If DWM composites it, it is in the captured desktop.)
2. There is **no top-level window** for it (`EnumWindows`/`FindWindow` finds nothing).
3. It is **on the physical connector** (visible to the user) at a rect, transparent
   where undrawn.

The Phase 0 PoC already established: a windowed flip-model swap chain is
DWM-composited (fails 1) and has an enumerable HWND (fails 2); exclusive-fullscreen
flip (DirectFlip) owns no window and bypasses DWM but is **whole-output** (fails the
rect requirement) and **not creatable on this Intel Arc** (`0x887a0001`).

## Candidate mechanisms (lightest first)

### M1 — user-mode hardware video overlay via `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL`
The flip-model "video overlay" swap-chain effect. On GPUs with a hardware video
overlay, presenting an (often NV12/YUV 4:2:0) texture this way can route it to a
**dedicated GPU overlay plane composited after DWM** — the exact "video not in
screenshot" property. No custom KMD for the present; the existing Intel KMD does the
compositing.

- **Pros:** lightest (pure user-mode D3D11/DXGI, no driver build), reuses the proven
  Phase 0 harness (swap chain + DXGI-dup companion) — just change the swap effect and
  the format and drop the window.
- **Cons / open questions:**
  - DWM *decides* whether to honor the overlay route; it is not a user-controllable
    "present to plane N." On some configs DWM composites it back into the desktop
    (defeating property 1).
  - A `FLIP_SEQUENTIAL` swap chain is normally attached to a window or a fullscreen
    state. The **windowless** variant (present to the overlay plane with no HWND) is
    the hard part — this is the actual existence tell.
  - Format/rect: does the overlay plane honor an arbitrary RGBA rect, or only
    NV12-ish fullscreen? The overlay is our transcript panel (RGBA, arbitrary rect).
- **Probe (B2):** extend the PoC — create a `FLIP_SEQUENTIAL` swap chain (windowed to a
  0×0 / non-interactive host first to confirm the effect + DXGI-absence, then attempt
  the windowless present), present a magenta rect, capture via DXGI dup + GDI, report
  presence/absence + whether a window is required.

### M2 — custom WDDM display KMD owning an overlay plane
A display miniport (WDDM 2.x) that, in addition to the primary desktop flip queue,
exposes a second presentable target mapped to a **GPU overlay/scanout plane**. The
app hands a texture + rect to the KMD (over the proven Phase A IOCTL/IPC channel); the
KDM's present path maps it to the overlay plane, composited after DWM.

- **Pros:** full control; windowless by construction; arbitrary rect/format.
- **Cons:** heaviest. Requires the **display DDK** (`dxgkmdi.h`, `d3dkmtdi.h`,
  `dxapi.h`, WDDM present DDIs `DxgkDdiPresent*` + the flip-queue/present path), which
  is **NOT installed** on this box (see Toolchain below). The Phase A spike is a
  non-PnP WDM character device; a display KMD is a different, larger driver model
  (PnP + DDK + DWM interplay).
- **Probe (B3):** obtain the display DDK, extend the KMD to a WDDM display miniport
  with an overlay present path, load it (test-signing), present a test texture, verify
  the present path works at the API level.

### M3 — separate child display adapter / second scanout
Expose a second display output (virtual, on the same physical connector) and present
the overlay to it. Heavier than M2 and changes the monitor topology (a second output
appears in `EnumDisplayMonitors` — its own tell). Deprioritized.

**Decision criterion for B2:** if M1 yields windowless + DXGI-absent + rect, we proceed on
M1 (no custom KMD needed for the present; Phase A/C shrink to the stealth/IPC layer). If
M1 fails any of the three, we go to M2 (and must clear the Toolchain gap first). **B2
result (below): M1 fails the windowless requirement → go to M2** (or accept Tier B).

## M1 probe result (B2) — run on this box, 2026-11-01

`display-overlay-poc/src/bin/phase-b-flipseq.rs` (build `cargo build --release`, run
`phase-b-flipseq.exe <out>`) tested both variants of the `FLIP_SEQUENTIAL` video-overlay
effect on the physical output (1920×1200@60), overlay rect 400×220 at (1520,40):

| Variant | CreateSwapChain | Present | DXGI-dup magenta | GDI magenta |
| --- | --- | --- | --- | --- |
| A. fullscreen (windowless) | **FAIL `0x887A0001`** (`DXGI_ERROR_INVALID_CALL`) | — | — | — |
| B. windowed | OK | OK | -1 (no frame, headless) | **88000** (= the full 400×220 rect) |

**Findings:**

1. **The windowless path (A) is No-Go on this Intel Arc.** A windowless `FLIP_SEQUENTIAL`
   swap chain (`OutputWindow = NULL`) returns `DXGI_ERROR_INVALID_CALL` (`0x887A0001`) —
   the *exact same* error Phase 0's exclusive-fullscreen DirectFlip got. This Intel iGPU
   will not create **any** windowless flip-model swap chain. M1's whole value was the
   windowless user-mode overlay plane → **M1 is No-Go for Tier C** on this GPU.
2. **The windowed path (B) is creatable but defeats the goal.** It presents correctly, and
   its magenta is fully present in the GDI/DWM surface (88000 = the whole rect) — i.e. it
   is DWM-composited. So it **fails property 1** (pixels must be *absent* from the captured
   surface) and, owning a top-level HWND, **fails property 2** (no window). It is, at best,
   a Tier B (hidden / 0×0 host window) candidate — a windowed flip chain has no claim on a
   post-DWM overlay plane.
3. The "video-not-in-screenshot" overlay-plane property is a **fullscreen** behavior. Since
   the fullscreen variant cannot even be created here, we cannot route a rect to an overlay
   plane in user mode on this GPU.

**Decision (was "to make at the end of B2"):** M1 is **No-Go** for a windowless Tier C
overlay on this Intel Arc. The overlay plane must come from **M2 — a custom WDDM display
KMD** with a present path to a GPU overlay/scanout plane (which also requires clearing the
Toolchain gap below). Alternatively, accept **Tier B** (windowed, hidden host) as the
practical ceiling on an Intel iGPU if M2 proves intractable.

**Physical caveat:** A/B's "absent-from-DXGI-dup" / "visible-on-physical-scanout" are not
physically confirmable on this headless box (dup produces no frames; GDI is wallpaper +
this window). Variant A's *creation failure* and variant B's *windowed, DWM-composited*
nature are, however, driver-level facts independent of the headless capture quirk.

## Toolchain gap (blocking M2, not M1)

The display-driver DDK headers are **not present** anywhere on this machine:
- WDK-as-NuGet (`microsoft.windows.wdk.x64` 10.0.26100.6584) `Include/…/km/` has
  `ntddk.h`, `dispmprt.h`, … but **no** `dxgkmdi.h` / `d3dkmtdi.h` / `dxapi.h`.
- Windows Kits `Include/10.0.26100.0/shared/` has `d3dkmthk.h` (and `d3dkmddi.h`) but
  not the display miniport set. No `ddk` folder.
So a M2 build needs the display DDK added (the "Windows Display Driver" DDK component)
— which on this box means getting the DDK install that the broken VS WDK workload
would normally provide. M1 needs no such thing (pure user-mode D3D11/DXGI, already
buildable via the PoC's `windows` crate).

## Verification (headless-display caveat)

This box is a **headless/virtual display** (Intel Arc, one output, no EDID): GDI BitBlt
returns wallpaper only and `DXGI Desktop Duplication` produces no frames. So:
- **M1/M2 "absent from DXGI dup"** and **"visible on physical scanout"** **cannot be
  physically confirmed here** — they need a real (non-remote) display (re-run the probe
  there). This is the same limitation noted in Phase 0.
- What **can** be verified here: the swap chain / present path **succeeds** at the API
  level (HRESULTs, present returns success), the windowless variant **creates** (or
  not), and the code paths build + load. Mark physical results "Requires physical
  Windows display verification."

## Cross-platform note

Phase B is **Windows-only**. The macOS counterpart is **Phase H** (CoreDisplay/IOSurface
or a display extension; a Mac is required). Nothing here changes the macOS track.

## Open questions from B1 — answered by the B2 probe

1. Does a windowless `FLIP_SEQUENTIAL` present to a GPU overlay plane even exist as a
   user-mode path on Intel, or is the overlay always window/fullscreen-scoped?
   → **Answered: it does not exist.** A windowless `FLIP_SEQUENTIAL` swap chain (like
   DirectFlip) is not creatable on this Intel Arc (`0x887A0001`).
2. If windowed, can the host window be made **0×0 / non-enumerable** (Tier B) and is the
   resulting overlay still on the separate plane (absent from DXGI dup)?
   → **Answered: no claim on the plane.** The windowed variant is DWM-composited (its pixels
   are in the GDI surface). It is a Tier B host-window candidate, not a separate plane.
3. Does the overlay plane honor an arbitrary **RGBA rect**, or only NV12/fullscreen?
   (Our overlay is an RGBA transcript panel.)
   → **Unreachable in user mode** (the fullscreen variant won't create), so the overlay plane
   is only reachable via M2 (a display KMD), where we control the rect/format.
4. What does DWM actually do with a `FLIP_SEQUENTIAL` present — overlay plane or desktop
   composite?
   → **Desktop composite** (windowed B's magenta is fully present in the GDI surface). The
   overlay plane is not engaged in the user-mode path on this GPU.