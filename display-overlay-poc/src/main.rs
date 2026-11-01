//! Phase 0 feasibility PoC: can we put pixels on the **physical scanout** (visible to the
//! user) while keeping them **absent from the DWM desktop** that a proctor's `DXGI Desktop
//! Duplication` reads?
//!
//! Mechanism under test: a **fullscreen flip-model** swap chain (DirectFlip) that takes over
//! the primary output. We fill it with a "desktop" stand-in (blue) + a magenta rect (the
//! "overlay"), present it, then capture via (a) `DXGI Desktop Duplication` (the proctor's
//! modern capture) and (b) a GDI BitBlt (legacy), and report whether the magenta is present.
//!
//! GO signal: physical screen shows the frame (you'll see the blue + magenta rect), AND the
//! magenta is ABSENT from the DXGI-dup frame.
//!
//! Build: `cargo build --release`   Run: `display-overlay-poc.exe <output-dir>`

use std::ffi::c_void;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

use windows::core::{ComInterface, Interface};
use windows::Win32::Foundation::{BOOL, FALSE, TRUE, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDeviceAndSwapChain, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext,
    ID3D11RenderTargetView, ID3D11Texture2D, D3D11_BUFFER_DESC, D3D11_TEXTURE2D_DESC,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE,
    D3D11_MAP_READ, D3D11_MAP_WRITE_DISCARD, D3D11_MAPPED_SUBRESOURCE,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, CreateDXGIFactory2, DXGI_OUTDUPL_FRAME_INFO, DXGI_SWAP_CHAIN_DESC,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter, IDXGIOutput,
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, IDXGIFactory1, IDXGIFactory2,
    IDXGISwapChain,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_DESC, DXGI_MODE_SCANLINE_ORDER, DXGI_MODE_SCALING,
    DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, DEVMODEW, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBRUSH,
    ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, SW_SHOWNOACTIVATE, WS_EX_TOPMOST, WS_POPUP, CreateWindowExW,
    DefWindowProcW, EnumWindows, HCURSOR, HICON, HMENU, RegisterClassW, SetProcessDPIAware,
    ShowWindow, WNDCLASSW,
};

const RECT_W: u32 = 400;
const RECT_H: u32 = 220;
const MAGENTA: [u8; 4] = [255, 0, 255, 255]; // BGRA
const BLUE: [u8; 4] = [255, 180, 0, 255]; // BGRA (a stand-in for the desktop)

/// Query an interface `T` off an existing COM object `src` (windows 0.52 has no
/// `query_interface` convenience, so drive `ComInterface::query` directly).
fn qib<S, T>(src: &S) -> Option<T>
where
    S: ComInterface,
    T: ComInterface + Interface,
{
    let mut raw: *mut c_void = std::ptr::null_mut();
    let hr = unsafe { src.query(&<T as ComInterface>::IID, &mut raw) };
    if hr.is_err() {
        return None;
    }
    Some(unsafe { T::from_raw(raw) })
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Proctor-side existence probe: does `hwnd` surface to a plain `EnumWindows` call?
static ENUM_TARGET: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static ENUM_FOUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

unsafe extern "system" fn enum_proc(h: HWND, _l: LPARAM) -> BOOL {
    if h.0 == ENUM_TARGET.load(std::sync::atomic::Ordering::SeqCst) {
        ENUM_FOUND.store(true, std::sync::atomic::Ordering::SeqCst);
        return FALSE; // stop enumerating
    }
    TRUE
}

fn is_enumerable_by_enumwindows(hwnd: HWND) -> bool {
    use std::sync::atomic::Ordering;
    ENUM_TARGET.store(hwnd.0, Ordering::SeqCst);
    ENUM_FOUND.store(false, Ordering::SeqCst);
    unsafe { let _ = EnumWindows(Some(enum_proc), LPARAM(0)); }
    ENUM_FOUND.load(Ordering::SeqCst)
}

/// Find the exact refresh rate of the display mode matching `cx x cy`, so an
/// exclusive-fullscreen swap chain is created with a guaranteed-valid mode.
fn enum_desktop_refresh(factory: &IDXGIFactory2, cx: u32, cy: u32, fallback_hz: u32) -> DXGI_RATIONAL {
    let mut refresh = DXGI_RATIONAL { Numerator: fallback_hz, Denominator: 1 };
    unsafe {
        if let Ok(adapter) = factory.EnumAdapters(0) {
            if let Ok(output) = adapter.EnumOutputs(0) {
                let mut num: u32 = 0;
                if output
                    .GetDisplayModeList(DXGI_FORMAT_B8G8R8A8_UNORM, 0, &mut num, None)
                    .is_ok()
                    && num > 0
                {
                    let mut modes: Vec<DXGI_MODE_DESC> = vec![DXGI_MODE_DESC::default(); num as usize];
                    let mut num2: u32 = 0;
                    if output
                        .GetDisplayModeList(
                            DXGI_FORMAT_B8G8R8A8_UNORM,
                            0,
                            &mut num2,
                            Some(modes.as_mut_ptr()),
                        )
                        .is_ok()
                    {
                        modes.truncate(num2 as usize);
                        if let Some(m) = modes.iter().find(|m| m.Width == cx && m.Height == cy) {
                            refresh = m.RefreshRate;
                        }
                    }
                }
            }
        }
    }
    refresh
}

fn main() {
    let out_dir = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let out = Path::new(&out_dir);
    let _ = fs::create_dir_all(out);
    println!("output dir: {}", out.display());

    // Make GDI capture operate in physical (unscaled) coordinates, matching the desktop mode.
    unsafe { let _ = SetProcessDPIAware(); }

    // Query the real current desktop mode (this session is DPI-scaled: virtual 1536x960 over a
    // 1920x1200 physical output).
    let mut dm = DEVMODEW::default();
    dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    let dm_ok = unsafe { EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut dm) };
    let (cx, cy, hz) = if dm_ok.0 != 0 {
        let (w, h) = (dm.dmPelsWidth, dm.dmPelsHeight);
        let f = dm.dmDisplayFrequency.max(1);
        println!("current desktop mode: {w}x{h} @ {f}Hz (EnumDisplaySettings)");
        (w, h, f)
    } else {
        println!("[info] EnumDisplaySettings failed; falling back to 1920x1200@60");
        (1920, 1200, 60)
    };
    println!("primary screen (physical): {cx}x{cy}");
    let rx = (cx.saturating_sub(RECT_W)) as i32;
    let ry: i32 = 40;
    let rw = RECT_W as i32;
    let rh = RECT_H as i32;
    println!("overlay rect (magenta): x={} y={} w={} h={}", rx, ry, rw, rh);

    // 1a. D3D11 device alone (no swap chain) — confirms the device and isolates swap-chain errors.
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut feature_level: D3D_FEATURE_LEVEL = D3D_FEATURE_LEVEL_11_0;
    let levels = [D3D_FEATURE_LEVEL_11_0];
    let hr_dev = unsafe {
        D3D11CreateDeviceAndSwapChain(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_FLAG(0),
            Some(&levels[..]),
            D3D11_SDK_VERSION,
            None,
            None,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )
    };
    match hr_dev {
        Ok(_) => println!("[OK] D3D11 device created (feature level {feature_level:?})"),
        Err(e) => {
            println!("[FAIL] D3D11 device (no swap chain): {e}");
            return;
        }
    }
    let device = device.unwrap();
    let context = context.unwrap();

    let factory: IDXGIFactory2 = match unsafe { CreateDXGIFactory2(0) } {
        Ok(f) => f,
        Err(e) => {
            println!("[FAIL] CreateDXGIFactory2: {e}");
            return;
        }
    };

    // 1b. Try an EXCLUSIVE-FULLSCREEN flip-model swap chain (DirectFlip candidate).
    let refresh = enum_desktop_refresh(&factory, cx, cy, hz);
    println!(
        "guaranteed-valid exclusive mode: {}x{} @ {}x/{}",
        cx, cy, refresh.Numerator, refresh.Denominator
    );
    let fullscreen_desc = DXGI_SWAP_CHAIN_DESC {
        BufferDesc: DXGI_MODE_DESC {
            Width: cx,
            Height: cy,
            RefreshRate: refresh,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            ScanlineOrdering: DXGI_MODE_SCANLINE_ORDER(0),
            Scaling: DXGI_MODE_SCALING(0),
        },
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        OutputWindow: HWND::default(),
        Windowed: FALSE,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        Flags: 0,
    };
    let mut swap: Option<IDXGISwapChain> = None;
    let mut is_fullscreen = false;
    let mut windowed_enum_visible = false;
    let hr_swap = unsafe { factory.CreateSwapChain(&device, &fullscreen_desc, &mut swap) };
    if hr_swap.is_ok() {
        is_fullscreen = true;
        println!("[OK] exclusive-fullscreen FLIP swap chain created (DirectFlip candidate)");
    } else {
        println!(
            "[info] exclusive-fullscreen FLIP swap chain failed (HRESULT {:#010x}); trying windowed.",
            hr_swap.0 as u32
        );
    }

    if !is_fullscreen {
        // Windowed flip-model on a real, visible, topmost borderless window at the overlay rect.
        let hinstance = unsafe { GetModuleHandleW(None) }
            .unwrap_or_else(|_| windows::Win32::Foundation::HMODULE::default());
        let class_name = windows::core::w!("POCOverlayWindow");
        let wndclass = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance.into(),
            hIcon: HICON::default(),
            hCursor: HCURSOR::default(),
            hbrBackground: HBRUSH::default(),
            lpszMenuName: windows::core::PCWSTR::null(),
            lpszClassName: class_name,
        };
        if unsafe { RegisterClassW(&wndclass) } == 0 {
            println!(
                "[FAIL] RegisterClassW: {}",
                unsafe { GetLastError().err().map(|e| e.code().0).unwrap_or(0) }
            );
            return;
        }
        let hwnd: HWND = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST,
                class_name,
                windows::core::w!("POC Overlay"),
                WS_POPUP,
                rx,
                ry,
                rw,
                rh,
                HWND::default(),
                HMENU::default(),
                hinstance,
                None,
            )
        };
        if hwnd.0 == 0 {
            println!(
                "[FAIL] CreateWindowExW: {}",
                unsafe { GetLastError().err().map(|e| e.code().0).unwrap_or(0) }
            );
            return;
        }
        unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE); }
        let windowed_desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Width: RECT_W,
                Height: RECT_H,
                RefreshRate: refresh,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                ScanlineOrdering: DXGI_MODE_SCANLINE_ORDER(0),
                Scaling: DXGI_MODE_SCALING(0),
            },
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            OutputWindow: hwnd,
            Windowed: TRUE,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            Flags: 0,
        };
        let mut swap2: Option<IDXGISwapChain> = None;
        let hr_swap2 = unsafe { factory.CreateSwapChain(&device, &windowed_desc, &mut swap2) };
        if hr_swap2.is_ok() {
            println!("[OK] windowed FLIP swap chain created on HWND {:?}", hwnd);
        } else {
            println!("[FAIL] windowed FLIP swap chain (HRESULT {:#010x})", hr_swap2.0 as u32);
            return;
        }
        swap = swap2;
        windowed_enum_visible = is_enumerable_by_enumwindows(hwnd);
        println!(
            "[{}] windowed-flip HWND {:?} enumerable by EnumWindows (proctor existence probe): {}",
            if windowed_enum_visible { "tell" } else { "note" }, hwnd, windowed_enum_visible
        );
    }

    let swap = swap.unwrap();
    let (sw, sh) = if is_fullscreen { (cx, cy) } else { (RECT_W, RECT_H) };
    let (frx, fry, frw, frh) = if is_fullscreen {
        (rx, ry, rw, rh)
    } else {
        (0, 0, rw, rh)
    };

    // 2. CPU-writable source texture matching the back buffer (blue, magenta in the overlay rect).
    let dyn_desc = D3D11_TEXTURE2D_DESC {
        Width: sw,
        Height: sh,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
    };
    let mut src_tex: Option<ID3D11Texture2D> = None;
    match unsafe { device.CreateTexture2D(&dyn_desc, None, Some(&mut src_tex)) } {
        Ok(_) => println!("[OK] dynamic CPU-writable source texture ({sw}x{sh})"),
        Err(e) => {
            println!("[FAIL] CreateTexture2D(src): {e}");
            return;
        }
    }
    let src_tex = src_tex.unwrap();
    fill_texture(&context, &src_tex, sw, sh, frx, fry, frw, frh);
    println!("[OK] filled source texture (blue + magenta rect)");

    // 3. Back buffer + render target view.
    let back_tex: ID3D11Texture2D = match unsafe { swap.GetBuffer(0) } {
        Ok(t) => t,
        Err(e) => {
            println!("[FAIL] GetBuffer(0): {e}");
            return;
        }
    };
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    match unsafe { device.CreateRenderTargetView(&back_tex, None, Some(&mut rtv)) } {
        Ok(_) => println!("[OK] render target view"),
        Err(e) => {
            println!("[FAIL] CreateRenderTargetView: {e}");
            return;
        }
    }
    let rtv = rtv.unwrap();

    // 4. Present the overlay to the physical scanout.
    for _ in 0..8 {
        unsafe {
            context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            context.ClearRenderTargetView(&rtv, &[0.1, 0.7, 1.0, 1.0]);
            context.CopyResource(&back_tex, &src_tex);
            let _ = swap.Present(1, 0);
        }
        thread::sleep(Duration::from_millis(120));
    }
    println!(
        "[OK] Presented. On the physical screen you should see the magenta rect ({}).",
        if is_fullscreen { "fullscreen, top-right" } else { "a small window at the overlay rect" }
    );
    thread::sleep(Duration::from_millis(300));

    // 5. Capture: DXGI Desktop Duplication (proctor's modern view) + GDI BitBlt (legacy).
    let dup_pixels = capture_dxgi_dup(&device, &context);
    let gdi_pixels = capture_gdi(cx, cy);

    // 6. Scan for magenta in the on-screen overlay rect of each captured frame.
    let dup_magenta = scan_magenta(dup_pixels.as_deref(), cx, cy, rx, ry, rw, rh);
    let gdi_magenta = scan_magenta(gdi_pixels.as_deref(), cx, cy, rx, ry, rw, rh);

    // 7. Save the captured frames for visual inspection.
    if let Some(p) = dup_pixels.as_ref() {
        let _ = write_bmp(&out.join("dxgi_dup.bmp"), p, cx, cy);
    }
    if let Some(p) = gdi_pixels.as_ref() {
        let _ = write_bmp(&out.join("gdi_blt.bmp"), p, cx, cy);
    }

    println!(
        "\n================= RESULTS ({}) =================",
        if is_fullscreen { "EXCLUSIVE-FULLSCREEN flip (DirectFlip)" } else { "WINDOWED flip" }
    );
    println!(
        "DXGI Desktop Duplication: magenta pixels in overlay rect = {} / {}",
        dup_magenta.unwrap_or(-1),
        (rw * rh)
    );
    println!(
        "GDI BitBlt screenshot:     magenta pixels in overlay rect = {} / {}",
        gdi_magenta.unwrap_or(-1),
        (rw * rh)
    );
    if is_fullscreen {
        println!("Windowed-flip HWND enumerable by EnumWindows: n/a (exclusive flip owns no window)");
    } else {
        println!(
            "Windowed-flip HWND enumerable by EnumWindows: {} ({} existence tell)",
            windowed_enum_visible,
            if windowed_enum_visible { "presence confirmed" } else { "absent" }
        );
    }
    println!("-------------------------------------------");
    if is_fullscreen {
        // DirectFlip owns no window (existence tell already gone) — the question is capture absence.
        match (dup_magenta, gdi_magenta) {
            (Some(dm), Some(gm)) if dm < 500 && gm < 500 => {
                println!("VERDICT: GO — content ABSENT from both capture methods and there is no window to enumerate. Confirm you saw it on the physical screen.");
            }
            (Some(dm), Some(gm)) => {
                println!("VERDICT: NO-GO — exclusive-flip content is PRESENT in capture (dup={}, gdi={}); it occupies the scanned output.", dm, gm);
            }
            _ => {
                println!("VERDICT: INCONCLUSIVE — exclusive flip created but no capture frame (this session is a headless/virtual display). Re-run on a physical display.");
            }
        }
    } else {
        println!(
            "VERDICT: NO-GO for 'rect below DWM' — a windowed flip-model swap chain is DWM-composited, so its pixels live in the DWM desktop surface a proctor captures, and its HWND {} — the existence tell survives.",
            if windowed_enum_visible { "is enumerable by EnumWindows (proven this run)" } else { "exists" }
        );
        println!("A plain flip-model swap chain cannot place a rect of pixels *below* the DWM desktop. That needs a hardware video-overlay plane (Phase 0 option b) or a display driver (option c).");
        if dup_magenta.is_none() || gdi_magenta.is_none() {
            println!("Environment note: a capture method produced no frame here — this session is a headless/virtual display (GDI capture returned wallpaper only; DXGI Desktop Duplication produced no frames; one display output with no physical monitor EDID). The windowed-flip in-capture result is therefore architectural, not physically measured, in this run.");
        }
    }
    println!("BMPs saved to: {}", out.display());

    // 8. Tear down (restore the desktop).
    drop(rtv);
    drop(back_tex);
    drop(src_tex);
    unsafe { swap.SetFullscreenState(None, None).ok(); }
    drop(swap);
    drop(context);
    drop(device);
    println!("\n[done] desktop restored");
}

fn fill_texture(context: &ID3D11DeviceContext, tex: &ID3D11Texture2D, cx: u32, cy: u32, rx: i32, ry: i32, rw: i32, rh: i32) {
    unsafe {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE { pData: std::ptr::null_mut(), RowPitch: 0, DepthPitch: 0 };
        if context.Map(tex, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped)).is_err() {
            println!("[FAIL] Map(src)");
            return;
        }
        let ptr = mapped.pData as *mut u8;
        let pitch = mapped.RowPitch as usize;
        for y in 0..cy as usize {
            let row = ptr.add(y * pitch);
            for x in 0..cx as usize {
                let px = x as i32;
                let py = y as i32;
                let in_rect = px >= rx && px < rx + rw && py >= ry && py < ry + rh;
                let c = if in_rect { &MAGENTA } else { &BLUE };
                row.add(x * 4).copy_from_nonoverlapping(c.as_ptr(), 4);
            }
        }
        context.Unmap(tex, 0);
    }
}

fn capture_dxgi_dup(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Option<Vec<u8>> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapter: IDXGIAdapter = factory.EnumAdapters(0).ok()?;
        let output: IDXGIOutput = adapter.EnumOutputs(0).ok()?;
        let output1: IDXGIOutput1 = qib::<IDXGIOutput, IDXGIOutput1>(&output)?;
        let dup: IDXGIOutputDuplication = output1.DuplicateOutput(device).ok()?;
        println!("[info] DuplicateOutput created; acquiring a DWM frame...");
        let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = Default::default();
        let mut desktop_res: Option<IDXGIResource> = None;
        let mut got = false;
        for _ in 0..15 {
            match dup.AcquireNextFrame(400, &mut frame_info, &mut desktop_res) {
                Ok(_) if desktop_res.is_some() => {
                    got = true;
                    break;
                }
                _ => thread::sleep(Duration::from_millis(150)),
            }
        }
        if !got {
            println!("[info] no DXGI-dup frame within retries");
            return None;
        }
        let desktop_res = desktop_res?;
        let buf: ID3D11Buffer = qib::<IDXGIResource, ID3D11Buffer>(&desktop_res)?;
        let mut d = D3D11_BUFFER_DESC { ByteWidth: 0, Usage: D3D11_USAGE_DEFAULT, BindFlags: 0, CPUAccessFlags: 0, MiscFlags: 0, StructureByteStride: 0 };
        buf.GetDesc(&mut d);
        let mut staging: Option<ID3D11Buffer> = None;
        let sdesc = D3D11_BUFFER_DESC {
            ByteWidth: d.ByteWidth,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        device.CreateBuffer(&sdesc, None, Some(&mut staging)).ok()?;
        let staging = staging?;
        context.CopyResource(&staging, &buf);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE { pData: std::ptr::null_mut(), RowPitch: 0, DepthPitch: 0 };
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ok()?;
        let bytes = (mapped.RowPitch as usize).max(d.ByteWidth as usize);
        let mut v = vec![0u8; bytes];
        std::ptr::copy_nonoverlapping(mapped.pData as *const u8, v.as_mut_ptr(), bytes);
        context.Unmap(&staging, 0);
        dup.ReleaseFrame().ok();
        Some(v)
    }
}

fn capture_gdi(cx: u32, cy: u32) -> Option<Vec<u8>> {
    unsafe {
        let hdc_screen: HDC = GetDC(HWND::default());
        let hdc_mem: HDC = CreateCompatibleDC(hdc_screen);
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: cx as i32,
                biHeight: -(cy as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0 as u32,
                biSizeImage: (cx * cy * 4) as u32,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let hbitmap: HBITMAP = match CreateDIBSection(hdc_mem, &bi, DIB_RGB_COLORS, &mut bits, HANDLE(0), 0) {
            Ok(b) => b,
            Err(e) => {
                println!("[info] GDI: CreateDIBSection failed: {e}");
                DeleteDC(hdc_mem);
                ReleaseDC(HWND::default(), hdc_screen);
                return None;
            }
        };
        let old: HGDIOBJ = SelectObject(hdc_mem, HGDIOBJ(hbitmap.0));
        let ok = BitBlt(hdc_mem, 0, 0, cx as i32, cy as i32, hdc_screen, 0, 0, SRCCOPY).is_ok();
        let mut out = vec![0u8; (cx * cy * 4) as usize];
        if bits.is_null() || !ok {
            println!("[info] GDI: BitBlt failed or no bits");
            SelectObject(hdc_mem, old);
            DeleteObject(HGDIOBJ(hbitmap.0));
            DeleteDC(hdc_mem);
            ReleaseDC(HWND::default(), hdc_screen);
            return None;
        }
        std::ptr::copy_nonoverlapping(bits as *const u8, out.as_mut_ptr(), out.len());
        SelectObject(hdc_mem, old);
        DeleteObject(HGDIOBJ(hbitmap.0));
        DeleteDC(hdc_mem);
        ReleaseDC(HWND::default(), hdc_screen);
        Some(out)
    }
}

fn scan_magenta(pixels: Option<&[u8]>, cx: u32, cy: u32, rx: i32, ry: i32, rw: i32, rh: i32) -> Option<i64> {
    let pixels = pixels?;
    let mut count = 0i64;
    let x0 = rx.max(0);
    let y0 = ry.max(0);
    let x1 = (rx + rw).min(cx as i32);
    let y1 = (ry + rh).min(cy as i32);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y as u32) * cx + (x as u32)) as usize * 4;
            if i + 3 < pixels.len() {
                let b = pixels[i];
                let g = pixels[i + 1];
                let r = pixels[i + 2];
                if r > 200 && b > 200 && g < 80 {
                    count += 1;
                }
            }
        }
    }
    Some(count)
}

fn write_bmp(path: &Path, pixels: &[u8], cx: u32, cy: u32) -> std::io::Result<()> {
    let row = (cx * 4) as u32;
    let img = (row * cy) as u32;
    let mut f: Vec<u8> = Vec::with_capacity(54 + img as usize);
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&((54 + img) as u32).to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&54u32.to_le_bytes());
    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&(cx as i32).to_le_bytes());
    f.extend_from_slice(&(-(cy as i32)).to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&32u16.to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&img.to_le_bytes());
    f.extend_from_slice(&(cx as i32).to_le_bytes());
    f.extend_from_slice(&(-(cy as i32)).to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(pixels);
    fs::write(path, f)
}