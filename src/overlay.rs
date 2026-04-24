use std::sync::atomic::Ordering;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::{HISTORY, OVERLAY_ACTIVE, QR_FOUND, QR_NOT_FOUND, WM_QR_RESULT};

// ── State stored per overlay window ─────────────────────────────────────────
struct OverlayState {
    orig_dc: HDC,
    dark_dc: HDC,
    back_dc: HDC,
    orig_bmp: HBITMAP,
    dark_bmp: HBITMAP,
    back_bmp: HBITMAP,
    width: i32,
    height: i32,
    start: POINT,
    current: POINT,
    dragging: bool,
}

// ── Public entry point ──────────────────────────────────────────────────────
pub fn start_selection() {
    OVERLAY_ACTIVE.store(true, Ordering::SeqCst);
    std::thread::spawn(|| unsafe {
        if let Err(_) = create_overlay() {
            OVERLAY_ACTIVE.store(false, Ordering::SeqCst);
        }
    });
}

unsafe fn create_overlay() -> Result<()> {
    let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);

    let screen = GetDC(None);

    // Capture original screenshot
    let orig_dc = CreateCompatibleDC(screen);
    let orig_bmp = CreateCompatibleBitmap(screen, vw, vh);
    SelectObject(orig_dc, orig_bmp);
    let _ = BitBlt(orig_dc, 0, 0, vw, vh, screen, vx, vy, SRCCOPY);

    // Create darkened copy
    let dark_dc = CreateCompatibleDC(screen);
    let dark_bmp = CreateCompatibleBitmap(screen, vw, vh);
    SelectObject(dark_dc, dark_bmp);
    let _ = BitBlt(dark_dc, 0, 0, vw, vh, orig_dc, 0, 0, SRCCOPY);
    darken_bitmap(dark_dc, dark_bmp, vw, vh);

    // Back-buffer for flicker-free painting
    let back_dc = CreateCompatibleDC(screen);
    let back_bmp = CreateCompatibleBitmap(screen, vw, vh);
    SelectObject(back_dc, back_bmp);

    ReleaseDC(None, screen);

    let state = Box::new(OverlayState {
        orig_dc, dark_dc, back_dc,
        orig_bmp, dark_bmp, back_bmp,
        width: vw, height: vh,
        start: POINT::default(),
        current: POINT::default(),
        dragging: false,
    });

    let hi: HINSTANCE = GetModuleHandleW(None)?.into();
    let cls = w!("QROverlay");
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(overlay_proc),
        hInstance: hi,
        lpszClassName: cls,
        hCursor: LoadCursorW(None, IDC_CROSS)?,
        ..Default::default()
    };
    // Ignore error if already registered
    let _ = RegisterClassExW(&wc);

    let raw = Box::into_raw(state);

    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        cls, w!(""),
        WS_POPUP | WS_VISIBLE,
        vx, vy, vw, vh,
        HWND::default(), HMENU::default(), hi,
        Some(raw as *const std::ffi::c_void),
    )?;

    let _ = SetForegroundWindow(hwnd);
    let _ = SetFocus(hwnd);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    Ok(())
}

// ── Overlay window procedure ────────────────────────────────────────────────
unsafe extern "system" fn overlay_proc(
    hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            LRESULT(0)
        }

        WM_PAINT => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const OverlayState;
            if ptr.is_null() {
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            let st = &*ptr;
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            compose(st);
            let _ = BitBlt(hdc, 0, 0, st.width, st.height, st.back_dc, 0, 0, SRCCOPY);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }

        WM_LBUTTONDOWN => {
            let st = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState);
            st.start = mouse_pt(lp);
            st.current = st.start;
            st.dragging = true;
            SetCapture(hwnd);
            LRESULT(0)
        }

        WM_MOUSEMOVE => {
            let st = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState);
            if st.dragging {
                st.current = mouse_pt(lp);
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }

        WM_LBUTTONUP => {
            let st = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState);
            if st.dragging {
                st.current = mouse_pt(lp);
                st.dragging = false;
                let _ = ReleaseCapture();
                let result = recognize_qr(st);
                close_overlay(hwnd);
                report_result(result);
            }
            LRESULT(0)
        }

        WM_RBUTTONDOWN => {
            cancel(hwnd);
            LRESULT(0)
        }

        WM_KEYDOWN => {
            if wp.0 as u16 == VK_ESCAPE.0 {
                cancel(hwnd);
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState;
            if !ptr.is_null() {
                let st = Box::from_raw(ptr);
                let _ = DeleteObject(st.orig_bmp);
                let _ = DeleteObject(st.dark_bmp);
                let _ = DeleteObject(st.back_bmp);
                let _ = DeleteDC(st.orig_dc);
                let _ = DeleteDC(st.dark_dc);
                let _ = DeleteDC(st.back_dc);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            OVERLAY_ACTIVE.store(false, Ordering::SeqCst);
            PostQuitMessage(0); // exits the overlay's message loop, not the main one
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────
fn mouse_pt(lp: LPARAM) -> POINT {
    POINT {
        x: (lp.0 & 0xFFFF) as i16 as i32,
        y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32,
    }
}

unsafe fn cancel(hwnd: HWND) {
    let st = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState);
    if st.dragging {
        st.dragging = false;
        let _ = ReleaseCapture();
    }
    close_overlay(hwnd);
}

unsafe fn close_overlay(hwnd: HWND) {
    let _ = DestroyWindow(hwnd);
}

fn norm_rect(a: POINT, b: POINT) -> RECT {
    RECT {
        left: a.x.min(b.x),
        top: a.y.min(b.y),
        right: a.x.max(b.x),
        bottom: a.y.max(b.y),
    }
}

// ── Painting ────────────────────────────────────────────────────────────────
unsafe fn compose(st: &OverlayState) {
    let bdc = st.back_dc;
    // Dark background
    let _ = BitBlt(bdc, 0, 0, st.width, st.height, st.dark_dc, 0, 0, SRCCOPY);

    // Help text
    SetBkMode(bdc, TRANSPARENT);
    SetTextColor(bdc, COLORREF(0x00FFFFFF));
    let font = CreateFontW(
        22, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
        DEFAULT_CHARSET.0 as u32, 0, 0, CLEARTYPE_QUALITY.0 as u32, 0, w!("Segoe UI"),
    );
    let old_font = SelectObject(bdc, font);
    let text = "Drag to select area  |  ESC / Right-click to cancel";
    let wide: Vec<u16> = text.encode_utf16().collect();
    let _ = TextOutW(bdc, st.width / 2 - 250, 16, &wide);
    SelectObject(bdc, old_font);
    let _ = DeleteObject(font);

    if st.dragging {
        let r = norm_rect(st.start, st.current);
        let w = r.right - r.left;
        let h = r.bottom - r.top;
        if w > 2 && h > 2 {
            // Show original (bright) pixels in selection
            let _ = BitBlt(bdc, r.left, r.top, w, h, st.orig_dc, r.left, r.top, SRCCOPY);

            // Orange border
            let pen = CreatePen(PS_SOLID, 2, COLORREF(0x0000A5FF));
            let old_pen = SelectObject(bdc, pen);
            let null_br = GetStockObject(NULL_BRUSH);
            let old_br = SelectObject(bdc, null_br);
            let _ = Rectangle(bdc, r.left - 1, r.top - 1, r.right + 1, r.bottom + 1);
            SelectObject(bdc, old_pen);
            SelectObject(bdc, old_br);
            let _ = DeleteObject(pen);

            // Size label
            let label = format!("{}×{}", w, h);
            let lw: Vec<u16> = label.encode_utf16().collect();
            let label_font = CreateFontW(
                16, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
                DEFAULT_CHARSET.0 as u32, 0, 0, CLEARTYPE_QUALITY.0 as u32, 0, w!("Segoe UI"),
            );
            let of = SelectObject(bdc, label_font);
            SetTextColor(bdc, COLORREF(0x0000A5FF));
            let _ = TextOutW(bdc, r.left, r.bottom + 4, &lw);
            SelectObject(bdc, of);
            let _ = DeleteObject(label_font);
        }
    }
}

// ── Darken a bitmap in-place ────────────────────────────────────────────────
unsafe fn darken_bitmap(dc: HDC, bmp: HBITMAP, w: i32, h: i32) {
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0 as u32,
            ..Default::default()
        },
        ..Default::default()
    };

    let cnt = (w * h) as usize;
    let mut px = vec![0u8; cnt * 4];

    GetDIBits(dc, bmp, 0, h as u32, Some(px.as_mut_ptr().cast()), &mut bmi, DIB_RGB_COLORS);

    // Darken to ~30% brightness
    for c in px.chunks_exact_mut(4) {
        c[0] = (c[0] as u16 * 3 / 10) as u8;
        c[1] = (c[1] as u16 * 3 / 10) as u8;
        c[2] = (c[2] as u16 * 3 / 10) as u8;
    }

    SetDIBitsToDevice(
        dc, 0, 0, w as u32, h as u32, 0, 0, 0, h as u32,
        px.as_ptr().cast(), &bmi, DIB_RGB_COLORS,
    );
}

// ── QR recognition ──────────────────────────────────────────────────────────
unsafe fn recognize_qr(st: &OverlayState) -> Option<String> {
    let r = norm_rect(st.start, st.current);
    let w = r.right - r.left;
    let h = r.bottom - r.top;
    if w < 10 || h < 10 {
        return None;
    }

    let screen = GetDC(None);
    let tmp_dc = CreateCompatibleDC(screen);
    let tmp_bmp = CreateCompatibleBitmap(screen, w, h);
    let old = SelectObject(tmp_dc, tmp_bmp);

    let _ = BitBlt(tmp_dc, 0, 0, w, h, st.orig_dc, r.left, r.top, SRCCOPY);

    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0 as u32,
            ..Default::default()
        },
        ..Default::default()
    };

    let cnt = (w * h) as usize;
    let mut px = vec![0u8; cnt * 4];
    GetDIBits(tmp_dc, tmp_bmp, 0, h as u32, Some(px.as_mut_ptr().cast()), &mut bmi, DIB_RGB_COLORS);

    SelectObject(tmp_dc, old);
    let _ = DeleteObject(tmp_bmp);
    let _ = DeleteDC(tmp_dc);
    ReleaseDC(None, screen);

    // Convert BGRA → grayscale
    let gray: Vec<u8> = px.chunks_exact(4)
        .map(|c| (0.114 * c[0] as f64 + 0.587 * c[1] as f64 + 0.299 * c[2] as f64) as u8)
        .collect();

    let img = image::GrayImage::from_raw(w as u32, h as u32, gray)?;
    decode_qr(img)
}

fn decode_qr(img: image::GrayImage) -> Option<String> {
    let mut prepared = rqrr::PreparedImage::prepare(img);
    let grids = prepared.detect_grids();
    for grid in grids {
        if let Ok((_meta, content)) = grid.decode() {
            return Some(content);
        }
    }
    None
}

fn report_result(result: Option<String>) {
    let hwnd = crate::get_main_hwnd();
    if hwnd.0.is_null() { return; }

    unsafe {
        if let Some(content) = result {
            HISTORY.lock().unwrap().push(content.clone());
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(content);
            }
            let _ = PostMessageW(hwnd, WM_QR_RESULT, WPARAM(QR_FOUND), LPARAM(0));
        } else {
            let _ = PostMessageW(hwnd, WM_QR_RESULT, WPARAM(QR_NOT_FOUND), LPARAM(0));
        }
    }
}
