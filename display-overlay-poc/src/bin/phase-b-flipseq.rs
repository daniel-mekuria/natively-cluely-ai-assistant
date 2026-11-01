//! Phase B / M1 probe — does a `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL` (the flip-model
//! "video overlay" effect) put pixels on a GPU overlay plane that is **absent from the
//! DWM desktop surface** a proctor's `DXGI Desktop Duplication` reads, and can it be
//! done **windowless**?
//!
//! Phase 0 already proved a `FLIP_DISCARD` swap chain is No-Go (windowed = enumerable
//! + DWM-composited; exclusive-fullscreen = not creatable on this Intel Arc). This probe
//! isolates the one untested variable: the **video-overlay swap effect**
//! (`FLIP_SEQUENTIAL`), which on GPUs with a hardware video overlay is the mechanism that
//! composites video *after* DWM — the "video not in screenshot" property.
//!
//! Variants tried, reported for each:
//!   A. **Fullscreen** `FLIP_SEQUENTIAL` (OutputWindow = NULL) — the native video-overlay
//!      path, windowless by construction.
//!   B. **Windowed** `FLIP_SEQUENTIAL` (topmost borderless at the overlay rect) — to see
//!      whether a window is still required and whether the windowed video overlay is in
//!      the DXGI-dup frame.
//!
//! For each variant that creates, we present a magenta rect, capture via
//! (1) `DXGI Desktop Duplication` (the proctor's modern view) and (2) GDI BitBlt, and
//! report the magenta pixel count in the overlay rect.
//!
//! **GO (M1):** a variant whose rect is visible on the physical scanout AND whose magenta
//! is ABSENT from the DXGI-dup frame (and windowless). **PARTIAL:** absent from dup but
//! needs a window (Tier B at best). **NO-GO:** magenta present in the dup frame (DWM
//! composited it), or nothing created.
//!
//! Physical confirmation (visible-on-scanout + absent-from-dup) requires a real display;
//! on a headless box the capture frames may be empty (reported as -1).
//!
//! Build: `cargo build --release`
//! Run:   `phase-b-flipseq.exe <output-dir>`

use std::ffi::c_void;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

use windows::core::{ComInterface, Interface};
use windows::Win32::Foundation::{FALSE, TRUE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDeviceAndSwapChain, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC,
    D3D11_TEXTURE2D_DESC, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_MAP_READ,
    D3D11_MAP_WRITE_DISCARD,
    D3D11_MAPPED_SUBRESOURCE, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC, D3D11_SDK_VERSION,
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, CreateDXGIFactory2, DXGI_OUTDUPL_FRAME_INFO, DXGI_SWAP_CHAIN_DESC,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter,
    IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, IDXGIFactory1,
    IDXGIFactory2, IDXGISwapChain,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_DESC, DXGI_MODE_SCANLINE_ORDER, DXGI_MODE_SCALING,
    DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, DEVMODEW, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBRUSH,
    ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, SW_SHOWNOACTIVATE, WS_EX_TOPMOST, WS_POPUP, CreateWindowExW,
    DefWindowProcW, HCURSOR, HICON, HMENU, RegisterClassW, SetProcessDPIAware, ShowWindow,
    WNDCLASSW,
};

const RECT_W: u32 = 400;
const RECT_H: u32 = 220;
const MAGENTA: [u8; 4] = [255, 0, 255, 255]; // BGRA
const BLUE: [u8; 4] = [255, 180, 0, 255]; // BGRA (desktop stand-in)

fn qib<S, T>(src: &S) -> Option<T>
where
    S: ComInterface,
    T: ComInterface + Interface,
{
    let mut raw: *mut c_void = std::ptr::null_mut();
    unsafe {
        if src.query(&<T as ComInterface>::IID, &mut raw).is_ok() {
            return Some(T::from_raw(raw));
        }
    }
    None
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn create_probe_window(rx: i32, ry: i32) -> HWND {
    unsafe {
        let hinstance = GetModuleHandleW(None)
            .unwrap_or_else(|_| windows::Win32::Foundation::HMODULE::default());
        let class_name = windows::core::w!("M1ProbeWindow");
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
        if RegisterClassW(&wndclass) == 0 {
            return HWND::default();
        }
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST,
            class_name,
            windows::core::w!("M1 Probe"),
            WS_POPUP,
            rx,
            ry,
            RECT_W as i32,
            RECT_H as i32,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        );
        if hwnd.0 != 0 {
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        hwnd
    }
}

/// Query the refresh rate of the desktop mode matching cx x cy.
fn enum_desktop_refresh(factory: &IDXGIFactory2, cx: u32, cy: u32, fallback: u32) -> (u32, u32) {
    let mut num: u32 = 0;
    unsafe {
        if let Ok(adapter) = factory.EnumAdapters(0) {
            if let Ok(output) = adapter.EnumOutputs(0) {
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
                            return (m.RefreshRate.Numerator, m.RefreshRate.Denominator.max(1));
                        }
                    }
                }
            }
        }
    }
    (fallback, 1)
}

fn make_flipseq_swapchain(
    factory: &IDXGIFactory2,
    device: &ID3D11Device,
    cx: u32,
    cy: u32,
    rn: u32,
    rd: u32,
    windowed: bool,
    hwnd: HWND,
) -> Option<IDXGISwapChain> {
    let (bw, bh) = if windowed { (RECT_W, RECT_H) } else { (cx, cy) };
    let desc = DXGI_SWAP_CHAIN_DESC {
        BufferDesc: DXGI_MODE_DESC {
            Width: bw,
            Height: bh,
            RefreshRate: DXGI_RATIONAL { Numerator: rn, Denominator: rd.max(1) },
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            ScanlineOrdering: DXGI_MODE_SCANLINE_ORDER(0),
            Scaling: DXGI_MODE_SCALING(0),
        },
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        OutputWindow: if windowed { hwnd } else { HWND::default() },
        Windowed: if windowed { TRUE } else { FALSE },
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        Flags: 0,
    };
    let mut swap: Option<IDXGISwapChain> = None;
    let hr = unsafe { factory.CreateSwapChain(device, &desc, &mut swap) };
    if hr.0 != 0 {
        println!("[M1] CreateSwapChain failed: 0x{:08X}", hr.0);
        return None;
    }
    swap
}

/// Present MAGENTA over the overlay rect on the back buffer. For the windowed variant the
/// whole buffer IS the overlay rect (magenta everywhere); for fullscreen, a blue fill with
/// a magenta rect at (rx, ry).
unsafe fn present_magenta(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    swap: &IDXGISwapChain,
    fullscreen: bool,
    cx: u32,
    cy: u32,
    rx: i32,
    ry: i32,
) {
    let (sw, sh) = if fullscreen { (cx, cy) } else { (RECT_W, RECT_H) };
    let all_magenta = !fullscreen;
    let (frx, fry, frw, frh) = (rx, ry, RECT_W as i32, RECT_H as i32);

    let desc = D3D11_TEXTURE2D_DESC {
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
    let mut src: Option<ID3D11Texture2D> = None;
    if let Err(e) = device.CreateTexture2D(&desc, None, Some(&mut src)) {
        println!("[M1] CreateTexture2D failed: {e:?}");
        return;
    }
    let src = src.unwrap();
    let mut mapped = D3D11_MAPPED_SUBRESOURCE { pData: std::ptr::null_mut(), RowPitch: 0, DepthPitch: 0 };
    if context.Map(&src, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped)).is_err() {
        println!("[M1] Map(src) failed");
        return;
    }
    let ptr = mapped.pData as *mut u8;
    let pitch = mapped.RowPitch as usize;
    for y in 0..sh as usize {
        let row = ptr.add(y * pitch);
        for x in 0..sw as usize {
            let px = x as i32;
            let py = y as i32;
            let in_rect = all_magenta || (px >= frx && px < frx + frw && py >= fry && py < fry + frh);
            let c = if in_rect { &MAGENTA } else { &BLUE };
            row.add(x * 4).copy_from_nonoverlapping(c.as_ptr(), 4);
        }
    }
    context.Unmap(&src, 0);

    let back: ID3D11Texture2D = match swap.GetBuffer(0) {
        Ok(t) => t,
        Err(e) => {
            println!("[M1] GetBuffer(0) failed: {e}");
            return;
        }
    };
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    if device.CreateRenderTargetView(&back, None, Some(&mut rtv)).is_err() {
        println!("[M1] CreateRenderTargetView failed");
        return;
    }
    let rtv = rtv.unwrap();
    for _ in 0..8 {
        context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
        context.ClearRenderTargetView(&rtv, &[0.1, 0.7, 1.0, 1.0]);
        context.CopyResource(&back, &src);
        let _ = swap.Present(1, 0);
        thread::sleep(Duration::from_millis(120));
    }
}

fn capture_dxgi_dup(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Option<Vec<u8>> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapter: IDXGIAdapter = factory.EnumAdapters(0).ok()?;
        let output: IDXGIOutput = adapter.EnumOutputs(0).ok()?;
        let output1: IDXGIOutput1 = qib::<IDXGIOutput, IDXGIOutput1>(&output)?;
        let dup: IDXGIOutputDuplication = output1.DuplicateOutput(device).ok()?;
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
            return None;
        }
        let desktop_res = desktop_res?;
        let buf: ID3D11Buffer = qib::<IDXGIResource, ID3D11Buffer>(&desktop_res)?;
        let mut d = D3D11_BUFFER_DESC::default();
        buf.GetDesc(&mut d);
        let sdesc = D3D11_BUFFER_DESC {
            ByteWidth: d.ByteWidth,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut staging: Option<ID3D11Buffer> = None;
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
        let hdc_screen = GetDC(HWND::default());
        let hdc_mem = CreateCompatibleDC(hdc_screen);
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: cx as i32,
                biHeight: -(cy as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0 as u32,
                biSizeImage: (cx * cy * 4) as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let hbitmap = match CreateDIBSection(hdc_mem, &bi, DIB_RGB_COLORS, &mut bits, windows::Win32::Foundation::HANDLE(0), 0) {
            Ok(b) => b,
            Err(_) => {
                DeleteDC(hdc_mem);
                ReleaseDC(HWND::default(), hdc_screen);
                return None;
            }
        };
        let old = SelectObject(hdc_mem, windows::Win32::Graphics::Gdi::HGDIOBJ(hbitmap.0));
        let ok = BitBlt(hdc_mem, 0, 0, cx as i32, cy as i32, hdc_screen, 0, 0, SRCCOPY).is_ok();
        let mut out = vec![0u8; (cx * cy * 4) as usize];
        if bits.is_null() || !ok {
            SelectObject(hdc_mem, old);
            DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(hbitmap.0));
            DeleteDC(hdc_mem);
            ReleaseDC(HWND::default(), hdc_screen);
            return None;
        }
        std::ptr::copy_nonoverlapping(bits as *const u8, out.as_mut_ptr(), out.len());
        SelectObject(hdc_mem, old);
        DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(hbitmap.0));
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

/// Run one variant (A fullscreen / B windowed): create the swap chain, present magenta,
/// capture (DXGI dup + GDI), scan, restore. Returns (created, dup_magenta, gdi_magenta),
/// with -1 meaning that capture produced no frame.
fn run_variant(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    factory: &IDXGIFactory2,
    cx: u32,
    cy: u32,
    rn: u32,
    rd: u32,
    rx: i32,
    ry: i32,
    windowed: bool,
) -> (bool, i64, i64) {
    let tag = if windowed { "B windowed" } else { "A fullscreen" };
    let hwnd = if windowed { create_probe_window(rx, ry) } else { HWND::default() };
    if windowed && hwnd.0 == 0 {
        println!("[M1][{tag}] CreateWindowExW failed");
        return (false, -1, -1);
    }
    let swap = match make_flipseq_swapchain(factory, device, cx, cy, rn, rd, windowed, hwnd) {
        Some(s) => {
            println!("[M1][{tag}] FLIP_SEQUENTIAL swap chain CREATED");
            s
        }
        None => {
            println!("[M1][{tag}] FLIP_SEQUENTIAL swap chain FAILED to create");
            return (false, -1, -1);
        }
    };
    unsafe { present_magenta(device, context, &swap, !windowed, cx, cy, rx, ry); }
    let dup = capture_dxgi_dup(device, context);
    let gdi = capture_gdi(cx, cy);
    let dm = scan_magenta(dup.as_deref(), cx, cy, rx, ry, RECT_W as i32, RECT_H as i32).unwrap_or(-1);
    let gm = scan_magenta(gdi.as_deref(), cx, cy, rx, ry, RECT_W as i32, RECT_H as i32).unwrap_or(-1);
    unsafe {
        let _ = swap.SetFullscreenState(None, None);
        drop(swap);
    }
    (true, dm, gm)
}

fn main() {
    let out_dir = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let out = Path::new(&out_dir);
    let _ = fs::create_dir_all(out);
    println!("output dir: {}", out.display());

    unsafe { let _ = SetProcessDPIAware(); }

    let mut dm = DEVMODEW::default();
    dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    let (cx, cy) = if unsafe { EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut dm) }.0 != 0 {
        (dm.dmPelsWidth, dm.dmPelsHeight)
    } else {
        (1920, 1200)
    };
    let rx = (cx.saturating_sub(RECT_W)) as i32;
    let ry: i32 = 40;
    println!("primary (physical): {cx}x{cy}; overlay rect x={rx} y={ry} {RECT_W}x{RECT_H}");

    // Device + context (device only; swap chains created per variant).
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut feature_level: D3D_FEATURE_LEVEL = D3D_FEATURE_LEVEL_11_0;
    let levels = [D3D_FEATURE_LEVEL_11_0];
    let hr = unsafe {
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
    match hr {
        Ok(_) => println!("[OK] D3D11 device created (feature level {feature_level:?})"),
        Err(e) => {
            println!("[FAIL] D3D11 device: {e}");
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

    let (rn, rd) = enum_desktop_refresh(&factory, cx, cy, 60);
    println!("[M1] FLIP_SEQUENTIAL refresh: {rn}x/{rd}");

    let a = run_variant(&device, &context, &factory, cx, cy, rn, rd, rx, ry, false);
    let b = run_variant(&device, &context, &factory, cx, cy, rn, rd, rx, ry, true);

    println!("\n================= M1 RESULTS (FLIP_SEQUENTIAL video overlay) =================");
    println!("(magenta pixels found in the {RECT_W}x{RECT_H} overlay rect; -1 = that capture produced no frame)");
    println!("[A] fullscreen FLIP_SEQUENTIAL (windowless): created={}, DXGI-dup={}, GDI={}", a.0, a.1, a.2);
    println!("[B] windowed FLIP_SEQUENTIAL:              created={}, DXGI-dup={}, GDI={}", b.0, b.1, b.2);
    println!("-------------------------------------------------------");

    let a_absent = a.0 && a.1 >= 0 && a.1 < 500;
    let b_absent = b.0 && b.1 >= 0 && b.1 < 500;
    if a_absent {
        println!("[M1] VERDICT: GO candidate (A) - fullscreen FLIP_SEQUENTIAL is windowless and its magenta is ABSENT from the DXGI-dup frame. Confirm physically on a real display.");
    } else if b_absent {
        println!("[M1] VERDICT: PARTIAL (B) - windowed FLIP_SEQUENTIAL is absent from the dup frame but needs a window (the existence tell survives -> Tier B at best).");
    } else {
        println!("[M1] VERDICT: NO-GO / inconclusive - magenta present in the dup frame (DWM composited it) or no frame produced. Headless note: -1 means no capture frame; re-run on a physical display before concluding.");
    }
    println!("[M1] done");
}