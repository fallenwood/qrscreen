#![windows_subsystem = "windows"]

mod overlay;

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// ── Constants ───────────────────────────────────────────────────────────────
const WM_TRAYICON: u32 = WM_APP + 1;
const WM_QR_RESULT: u32 = WM_APP + 2;
const IDM_CLEAR: u16 = 1001;
const IDM_QUIT: u16 = 1002;
const IDM_HISTORY_BASE: u16 = 2000;
const MAX_HISTORY: usize = 20;

// ── Global state ────────────────────────────────────────────────────────────
pub static OVERLAY_ACTIVE: AtomicBool = AtomicBool::new(false);
// Store HWND as raw isize for thread-safety
pub static MAIN_HWND_RAW: AtomicIsize = AtomicIsize::new(0);
pub static HISTORY: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn get_main_hwnd() -> HWND {
    HWND(MAIN_HWND_RAW.load(Ordering::SeqCst) as *mut std::ffi::c_void)
}

// 0 = nothing, 1 = found, 2 = not found  (sent via WPARAM of WM_QR_RESULT)
const QR_FOUND: usize = 1;
const QR_NOT_FOUND: usize = 2;

// ── Entry point ─────────────────────────────────────────────────────────────
fn main() -> Result<()> {
    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = w!("QRScreenMain");

        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("QR Screen Reader"),
            WS_OVERLAPPEDWINDOW,
            0, 0, 0, 0,
            HWND::default(), HMENU::default(), hinstance, None,
        )?;

        MAIN_HWND_RAW.store(hwnd.0 as isize, Ordering::SeqCst);
        create_tray(hwnd, hinstance)?;

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        remove_tray(hwnd);
        Ok(())
    }
}

// ── Window procedure ────────────────────────────────────────────────────────
unsafe extern "system" fn wndproc(
    hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAYICON => {
            match lp.0 as u32 {
                WM_LBUTTONUP => {
                    if !OVERLAY_ACTIVE.load(Ordering::SeqCst) {
                        overlay::start_selection();
                    }
                }
                WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as u16;
            handle_menu_command(hwnd, id);
            LRESULT(0)
        }
        WM_QR_RESULT => {
            match wp.0 {
                QR_FOUND => balloon(hwnd, "QR Code Found", "Content copied to clipboard!"),
                QR_NOT_FOUND => balloon(hwnd, "No QR Code", "No QR code detected in selection."),
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => { PostQuitMessage(0); LRESULT(0) }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn handle_menu_command(hwnd: HWND, id: u16) {
    if id >= IDM_HISTORY_BASE && id < IDM_HISTORY_BASE + MAX_HISTORY as u16 {
        let idx = (id - IDM_HISTORY_BASE) as usize;
        let hist = HISTORY.lock().unwrap();
        if let Some(entry) = hist.iter().rev().nth(idx) {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(entry.clone());
            }
            drop(hist);
            balloon(hwnd, "Copied", "QR content copied to clipboard!");
        }
        return;
    }
    match id {
        IDM_CLEAR => { HISTORY.lock().unwrap().clear(); }
        IDM_QUIT => unsafe {
            remove_tray(hwnd);
            PostQuitMessage(0);
        },
        _ => {}
    }
}

// ── Tray icon ───────────────────────────────────────────────────────────────
fn create_tray(hwnd: HWND, hinstance: HINSTANCE) -> Result<()> {
    unsafe {
        let mut nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAYICON,
            hIcon: make_icon(hinstance),
            ..Default::default()
        };
        let tip = "QR Screen Reader";
        let w: Vec<u16> = tip.encode_utf16().collect();
        nid.szTip[..w.len()].copy_from_slice(&w);
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        Ok(())
    }
}

fn remove_tray(hwnd: HWND) {
    unsafe {
        let nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            ..Default::default()
        };
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

fn balloon(hwnd: HWND, title: &str, msg: &str) {
    unsafe {
        let mut nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_INFO,
            dwInfoFlags: NIIF_INFO,
            ..Default::default()
        };
        let tw: Vec<u16> = title.encode_utf16().collect();
        nid.szInfoTitle[..tw.len().min(63)].copy_from_slice(&tw[..tw.len().min(63)]);
        let mw: Vec<u16> = msg.encode_utf16().collect();
        nid.szInfo[..mw.len().min(255)].copy_from_slice(&mw[..mw.len().min(255)]);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

// ── Context menu ────────────────────────────────────────────────────────────
fn show_menu(hwnd: HWND) {
    unsafe {
        let menu = CreatePopupMenu().unwrap();
        let hist = HISTORY.lock().unwrap();
        if hist.is_empty() {
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, w!("(no history)"));
        } else {
            for (i, entry) in hist.iter().rev().take(MAX_HISTORY).enumerate() {
                let display: String = if entry.len() > 50 {
                    format!("{}...", &entry[..50])
                } else {
                    entry.clone()
                };
                let wide: Vec<u16> = display.encode_utf16().chain(std::iter::once(0)).collect();
                let _ = AppendMenuW(
                    menu, MF_STRING,
                    (IDM_HISTORY_BASE + i as u16) as usize,
                    PCWSTR(wide.as_ptr()),
                );
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, IDM_CLEAR as usize, w!("Clear History"));
        }
        drop(hist);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, IDM_QUIT as usize, w!("Quit"));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_LEFTALIGN | TPM_BOTTOMALIGN, pt.x, pt.y, 0, hwnd, None);
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
}

// ── Icon (simple QR-like pattern) ───────────────────────────────────────────
fn make_icon(_hi: HINSTANCE) -> HICON {
    unsafe {
        let s: i32 = 32;
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, s, s);
        let old = SelectObject(dc, bmp);

        let white = CreateSolidBrush(COLORREF(0x00FFFFFF));
        let black = CreateSolidBrush(COLORREF(0x00000000));
        let full = RECT { left: 0, top: 0, right: s, bottom: s };
        FillRect(dc, &full, white);

        // Three QR finder patterns
        for &(ox, oy) in &[(2, 2), (20, 2), (2, 20)] {
            let r1 = RECT { left: ox, top: oy, right: ox + 10, bottom: oy + 10 };
            FillRect(dc, &r1, black);
            let r2 = RECT { left: ox + 2, top: oy + 2, right: ox + 8, bottom: oy + 8 };
            FillRect(dc, &r2, white);
            let r3 = RECT { left: ox + 3, top: oy + 3, right: ox + 7, bottom: oy + 7 };
            FillRect(dc, &r3, black);
        }
        // Some data dots
        for &(dx, dy) in &[(14, 14), (20, 20), (26, 14), (14, 26)] {
            FillRect(dc, &RECT { left: dx, top: dy, right: dx + 4, bottom: dy + 4 }, black);
        }

        let _ = DeleteObject(white);
        let _ = DeleteObject(black);
        SelectObject(dc, old);

        let mask_dc = CreateCompatibleDC(screen);
        let mask = CreateCompatibleBitmap(screen, s, s);
        let old_m = SelectObject(mask_dc, mask);
        let _ = BitBlt(mask_dc, 0, 0, s, s, None, 0, 0, BLACKNESS);
        SelectObject(mask_dc, old_m);
        let _ = DeleteDC(mask_dc);
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);

        let ii = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: bmp,
        };
        CreateIconIndirect(&ii).unwrap_or_default()
    }
}
