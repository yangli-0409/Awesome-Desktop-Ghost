// Teleprompter: a live copy of a window (or a part of it you frame by dragging)
// in a small panel at the top of the primary screen, right under a laptop's
// camera, so your eyes stay near the lens while you read notes.
//
// The copy is a DWM thumbnail: the compositor draws it, so it costs almost
// nothing and keeps showing while the source window is covered by others. The
// panel, its × button and the region picker are Ghost's own windows, so they
// are excluded from capture directly (no injection), like the HUD.
//
// State lives in Cells, not a RefCell: Win32 calls made here send messages back
// into these window procedures synchronously, and a held borrow would panic.

use std::cell::Cell;

use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmRegisterThumbnail, DwmSetWindowAttribute, DwmUnregisterThumbnail,
    DwmUpdateThumbnailProperties, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWINDOWATTRIBUTE,
    DWM_THUMBNAIL_PROPERTIES,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateEllipticRgn, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, FillRect, FrameRect, GetMonitorInfoW,
    GetStockObject, GetTextExtentPoint32W, InvalidateRect, LineTo, MonitorFromPoint, MoveToEx,
    SelectObject, SetBkMode, SetTextColor, SetWindowRgn, TextOutW, DEFAULT_GUI_FONT, HBRUSH,
    HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTOPRIMARY, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::win::window_rect;

const PANEL_CLASS: PCWSTR = w!("GhostPrompter");
const CLOSE_CLASS: PCWSTR = w!("GhostPrompterClose");
const PICKER_CLASS: PCWSTR = w!("GhostRegionPicker");
const TIMER_FOLLOW: usize = 1;
const EXCLUDE: WINDOW_DISPLAY_AFFINITY = WINDOW_DISPLAY_AFFINITY(0x11); // WDA_EXCLUDEFROMCAPTURE
const REG_KEY: &str = r"Software\Ghost";

// DWM_TNP_* flags.
const TNP_RECTDESTINATION: u32 = 0x1;
const TNP_RECTSOURCE: u32 = 0x2;
const TNP_OPACITY: u32 = 0x4;
const TNP_VISIBLE: u32 = 0x8;
const TNP_SOURCECLIENTAREAONLY: u32 = 0x10;

// Pixels the picker paints in this color become see-through holes (LWA_COLORKEY).
const KEY: COLORREF = COLORREF(0x00FF00FF);

#[derive(Default)]
struct State {
    panel: Cell<isize>,
    close: Cell<isize>,
    thumb: Cell<isize>,
    source: Cell<isize>,
    crop: Cell<RECT>, // in the source window's coordinates (from its GetWindowRect origin)
    aspect: Cell<f64>, // crop height / width
    picker: Cell<isize>,
    pick_target: Cell<isize>,
    anchor: Cell<Option<POINT>>,
    cursor: Cell<POINT>,
}

thread_local! {
    static S: State = State::default();
}

fn st<R>(f: impl FnOnce(&State) -> R) -> R {
    S.with(f)
}

fn hwnd(h: isize) -> HWND {
    HWND(h as *mut _)
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

fn lp_point(lp: LPARAM) -> POINT {
    POINT { x: (lp.0 & 0xFFFF) as i16 as i32, y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32 }
}

fn alpha(percent: u8) -> u8 {
    ((percent as u32 * 255) / 100) as u8
}

fn dpi_of(h: isize) -> i32 {
    let d = unsafe { GetDpiForWindow(hwnd(h)) } as i32;
    if d > 0 { d } else { 96 }
}

// ---- settings (HKCU\Software\Ghost) ----------------------------------------

fn load(name: &str, default: u32) -> u32 {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(REG_KEY)
        .and_then(|k| k.get_value::<u32, _>(name))
        .unwrap_or(default)
}

fn save(name: &str, v: u32) {
    if let Ok((k, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(REG_KEY) {
        let _ = k.set_value(name, &v);
    }
}

pub fn opacity() -> u8 {
    load("PrompterOpacity", 90).clamp(50, 100) as u8
}

pub fn set_opacity(p: u8) {
    save("PrompterOpacity", p as u32);
    let panel = st(|s| s.panel.get());
    if panel != 0 {
        unsafe {
            let _ = SetLayeredWindowAttributes(hwnd(panel), COLORREF(0), alpha(p), LWA_ALPHA);
        }
    }
}

pub fn is_active() -> bool {
    st(|s| s.panel.get() != 0)
}

// ---- classes ----------------------------------------------------------------

pub fn register_classes(hinst: HINSTANCE) {
    unsafe {
        let arrow = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
        let panel = WNDCLASSW {
            lpfnWndProc: Some(panel_proc),
            hInstance: hinst,
            lpszClassName: PANEL_CLASS,
            hCursor: arrow,
            hbrBackground: CreateSolidBrush(rgb(18, 18, 20)),
            ..Default::default()
        };
        RegisterClassW(&panel);
        let close = WNDCLASSW {
            lpfnWndProc: Some(close_proc),
            hInstance: hinst,
            lpszClassName: CLOSE_CLASS,
            hCursor: arrow,
            ..Default::default()
        };
        RegisterClassW(&close);
        let picker = WNDCLASSW {
            lpfnWndProc: Some(picker_proc),
            hInstance: hinst,
            lpszClassName: PICKER_CLASS,
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&picker);
    }
}

// ---- geometry -----------------------------------------------------------------

/// The visible bounds of a window (GetWindowRect includes the invisible
/// resize borders on Windows 10/11).
fn frame_bounds(h: isize) -> RECT {
    let mut r = RECT::default();
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd(h),
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut _ as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .is_ok();
    if ok { r } else { window_rect(h) }
}

/// Work area and DPI of the primary monitor: on a laptop that is the built-in
/// screen, the one with the camera above it.
fn primary() -> (RECT, i32) {
    unsafe {
        let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        (mi.rcWork, dx as i32)
    }
}

fn under_camera(w: i32) -> (i32, i32) {
    let (work, dpi) = primary();
    (work.left + (work.right - work.left - w) / 2, work.top + 6 * dpi / 96)
}

// ---- region picker -----------------------------------------------------------

/// Bring `target` to the front and cover it with a dimmed overlay: drag to frame
/// a region, Enter for the whole window, Esc (or right-click) to cancel.
pub fn pick(target: isize) {
    cancel_pick();
    unsafe {
        let t = hwnd(target);
        if IsIconic(t).as_bool() {
            let _ = ShowWindow(t, SW_SHOWNOACTIVATE);
        }
        // Raise without activating: activating another app would cost Ghost the
        // right to give the picker keyboard focus.
        let _ = SetWindowPos(t, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let r = frame_bounds(target);
        let Ok(h) = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PICKER_CLASS,
            w!("Ghost"),
            WS_POPUP,
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            None,
            None,
            None,
            None,
        ) else {
            return;
        };
        st(|s| {
            s.picker.set(h.0 as isize);
            s.pick_target.set(target);
            s.anchor.set(None);
        });
        let _ = SetLayeredWindowAttributes(h, KEY, 140, LWA_ALPHA | LWA_COLORKEY);
        let _ = SetWindowDisplayAffinity(h, EXCLUDE);
        let _ = ShowWindow(h, SW_SHOW);
        let _ = SetForegroundWindow(h);
        let _ = SetFocus(h);
    }
}

fn cancel_pick() {
    let p = st(|s| s.picker.replace(0));
    if p != 0 {
        unsafe {
            let _ = DestroyWindow(hwnd(p));
        }
    }
}

/// `sel` is in picker client coordinates; None means the whole window.
fn finish_pick(sel: Option<RECT>) {
    let (picker, target) = st(|s| (s.picker.replace(0), s.pick_target.replace(0)));
    if picker == 0 {
        return;
    }
    let pr = window_rect(picker);
    unsafe {
        let _ = DestroyWindow(hwnd(picker));
    }
    let screen = match sel {
        Some(r) => RECT { left: pr.left + r.left, top: pr.top + r.top, right: pr.left + r.right, bottom: pr.top + r.bottom },
        None => pr,
    };
    start(target, screen);
}

fn selection() -> Option<RECT> {
    let (a, c) = st(|s| (s.anchor.get(), s.cursor.get()));
    let a = a?;
    Some(RECT { left: a.x.min(c.x), top: a.y.min(c.y), right: a.x.max(c.x), bottom: a.y.max(c.y) })
}

extern "system" fn picker_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_LBUTTONDOWN => {
                SetCapture(h);
                let p = lp_point(lp);
                st(|s| {
                    s.anchor.set(Some(p));
                    s.cursor.set(p);
                });
                let _ = InvalidateRect(h, None, false);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                if st(|s| s.anchor.get().is_some()) {
                    let mut c = RECT::default();
                    let _ = GetClientRect(h, &mut c);
                    let p = lp_point(lp);
                    st(|s| s.cursor.set(POINT { x: p.x.clamp(0, c.right), y: p.y.clamp(0, c.bottom) }));
                    let _ = InvalidateRect(h, None, false);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let _ = ReleaseCapture();
                let sel = selection();
                st(|s| s.anchor.set(None));
                match sel {
                    Some(r) if r.right - r.left >= 40 && r.bottom - r.top >= 40 => finish_pick(Some(r)),
                    _ => {
                        let _ = InvalidateRect(h, None, false); // a click: keep waiting
                    }
                }
                LRESULT(0)
            }
            WM_RBUTTONUP | WM_CLOSE => {
                cancel_pick();
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let k = wp.0 as u16;
                if k == VK_ESCAPE.0 {
                    cancel_pick();
                } else if k == VK_RETURN.0 {
                    finish_pick(None);
                }
                LRESULT(0)
            }
            WM_ACTIVATE => {
                // Clicking another window cancels, like macOS.
                if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    let _ = PostMessageW(h, WM_CLOSE, WPARAM(0), LPARAM(0));
                }
                DefWindowProcW(h, msg, wp, lp)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                paint_picker(h);
                LRESULT(0)
            }
            _ => DefWindowProcW(h, msg, wp, lp),
        }
    }
}

fn paint_picker(h: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(h, &mut ps);
        let mut c = RECT::default();
        let _ = GetClientRect(h, &mut c);

        // Draw off-screen, then copy once: no flicker while dragging.
        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, c.right, c.bottom);
        let old = SelectObject(mem, HGDIOBJ(bmp.0));

        fill(mem, &c, rgb(0, 0, 0));
        if let Some(r) = selection() {
            fill(mem, &r, KEY); // the framed region shows through undimmed
            frame(mem, &r, rgb(255, 255, 255));
            frame(mem, &RECT { left: r.left + 1, top: r.top + 1, right: r.right - 1, bottom: r.bottom - 1 }, rgb(255, 255, 255));
        }

        let text: Vec<u16> = "拖动框出要放进提词器的区域 · 回车用整个窗口 · Esc 取消".encode_utf16().collect();
        let font = GetStockObject(DEFAULT_GUI_FONT);
        let oldf = SelectObject(mem, font);
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(mem, &text, &mut sz);
        let x = (c.right - sz.cx) / 2;
        let y = 16;
        fill(mem, &RECT { left: x - 12, top: y - 6, right: x + sz.cx + 12, bottom: y + sz.cy + 6 }, rgb(30, 30, 34));
        SetBkMode(mem, TRANSPARENT);
        SetTextColor(mem, rgb(255, 255, 255));
        let _ = TextOutW(mem, x, y, &text);
        SelectObject(mem, oldf);

        let _ = BitBlt(hdc, 0, 0, c.right, c.bottom, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        let _ = EndPaint(h, &ps);
    }
}

fn fill(hdc: windows::Win32::Graphics::Gdi::HDC, r: &RECT, color: COLORREF) {
    unsafe {
        let b = CreateSolidBrush(color);
        FillRect(hdc, r, b);
        let _ = DeleteObject(HGDIOBJ(b.0));
    }
}

fn frame(hdc: windows::Win32::Graphics::Gdi::HDC, r: &RECT, color: COLORREF) {
    unsafe {
        let b: HBRUSH = CreateSolidBrush(color);
        FrameRect(hdc, r, b);
        let _ = DeleteObject(HGDIOBJ(b.0));
    }
}

// ---- panel -------------------------------------------------------------------

/// Show `sel` (screen coordinates, inside `source`) in the panel under the camera.
pub fn start(source: isize, sel: RECT) {
    stop();
    let wr = window_rect(source);
    let crop = RECT { left: sel.left - wr.left, top: sel.top - wr.top, right: sel.right - wr.left, bottom: sel.bottom - wr.top };
    let (cw, ch) = (crop.right - crop.left, crop.bottom - crop.top);
    if cw < 10 || ch < 10 {
        return;
    }
    let aspect = ch as f64 / cw as f64;

    let (work, dpi) = primary();
    let mut w = load("PrompterWidth", 0) as i32;
    if w == 0 {
        w = 480 * dpi / 96;
    }
    w = w.clamp(160, ((work.right - work.left) * 9 / 10).max(160));
    let mut h = (w as f64 * aspect).round() as i32;
    let max_h = (work.bottom - work.top) * 6 / 10;
    if h > max_h {
        h = max_h;
        w = (h as f64 / aspect).round() as i32;
    }
    let (x, y) = under_camera(w);

    st(|s| {
        s.source.set(source);
        s.crop.set(crop);
        s.aspect.set(aspect);
    });

    unsafe {
        let Ok(panel) = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PANEL_CLASS,
            w!("Ghost 提词器"),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            None,
            None,
        ) else {
            return;
        };
        let _ = SetLayeredWindowAttributes(panel, COLORREF(0), alpha(opacity()), LWA_ALPHA);
        let _ = SetWindowDisplayAffinity(panel, EXCLUDE);
        // Rounded corners on Windows 11 (DWMWA_WINDOW_CORNER_PREFERENCE = 33,
        // DWMWCP_ROUND = 2); ignored on Windows 10.
        let round: u32 = 2;
        let _ = DwmSetWindowAttribute(panel, DWMWINDOWATTRIBUTE(33), &round as *const _ as *const _, 4);

        let thumb = match DwmRegisterThumbnail(panel, hwnd(source)) {
            Ok(t) => t,
            Err(e) => {
                let _ = DestroyWindow(panel);
                crate::error_box(&format!("提词器无法显示这个窗口：{e}"));
                return;
            }
        };

        let close = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            CLOSE_CLASS,
            w!("Ghost"),
            WS_POPUP,
            0,
            0,
            24,
            24,
            panel, // owned: always above the panel
            None,
            None,
            None,
        )
        .map(|c| c.0 as isize)
        .unwrap_or(0);

        st(|s| {
            s.panel.set(panel.0 as isize);
            s.thumb.set(thumb);
            s.close.set(close);
        });
        if close != 0 {
            let _ = SetLayeredWindowAttributes(hwnd(close), COLORREF(0), 235, LWA_ALPHA);
            let _ = SetWindowDisplayAffinity(hwnd(close), EXCLUDE);
        }
        update_thumb();
        place_close();
        let _ = ShowWindow(panel, SW_SHOWNOACTIVATE);
        SetTimer(panel, TIMER_FOLLOW, 500, None);
    }
}

pub fn stop() {
    let (panel, close, thumb) = st(|s| {
        s.source.set(0);
        (s.panel.replace(0), s.close.replace(0), s.thumb.replace(0))
    });
    unsafe {
        if thumb != 0 {
            let _ = DwmUnregisterThumbnail(thumb);
        }
        if close != 0 {
            let _ = DestroyWindow(hwnd(close));
        }
        if panel != 0 {
            let _ = KillTimer(hwnd(panel), TIMER_FOLLOW);
            let _ = DestroyWindow(hwnd(panel));
        }
    }
}

/// Move the panel back to the top centre of the primary screen.
pub fn snap() {
    let panel = st(|s| s.panel.get());
    if panel == 0 {
        return;
    }
    let r = window_rect(panel);
    let (x, y) = under_camera(r.right - r.left);
    unsafe {
        let _ = SetWindowPos(hwnd(panel), None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

pub fn exit() {
    cancel_pick();
    stop();
}

fn update_thumb() {
    let (panel, thumb, crop) = st(|s| (s.panel.get(), s.thumb.get(), s.crop.get()));
    if panel == 0 || thumb == 0 {
        return;
    }
    unsafe {
        let mut c = RECT::default();
        let _ = GetClientRect(hwnd(panel), &mut c);
        let props = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: TNP_RECTDESTINATION | TNP_RECTSOURCE | TNP_OPACITY | TNP_VISIBLE | TNP_SOURCECLIENTAREAONLY,
            rcDestination: c,
            rcSource: crop,
            opacity: 255,
            fVisible: BOOL(1),
            fSourceClientAreaOnly: BOOL(0),
        };
        let _ = DwmUpdateThumbnailProperties(thumb, &props);
    }
}

fn place_close() {
    let (panel, close) = st(|s| (s.panel.get(), s.close.get()));
    if panel == 0 || close == 0 {
        return;
    }
    let d = dpi_of(panel);
    let sz = 24 * d / 96;
    let pad = 8 * d / 96;
    let r = window_rect(panel);
    unsafe {
        let _ = SetWindowPos(hwnd(close), None, r.right - sz - pad, r.top + pad, sz, sz, SWP_NOZORDER | SWP_NOACTIVATE);
        let rgn = CreateEllipticRgn(0, 0, sz + 1, sz + 1);
        let _ = SetWindowRgn(hwnd(close), rgn, true);
    }
}

/// Called from the 60 ms poll: show the × while the pointer is over the panel.
pub fn poll(pt: POINT) {
    let (panel, close) = st(|s| (s.panel.get(), s.close.get()));
    if panel == 0 || close == 0 {
        return;
    }
    let inside = |h: isize| {
        let r = window_rect(h);
        pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
    };
    let over = inside(panel) || inside(close);
    unsafe {
        let shown = IsWindowVisible(hwnd(close)).as_bool();
        if over != shown {
            let _ = ShowWindow(hwnd(close), if over { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
    }
}

fn keep_aspect(edge: u32, r: &mut RECT) {
    let a = st(|s| s.aspect.get());
    if a <= 0.0 {
        return;
    }
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if edge == WMSZ_TOP || edge == WMSZ_BOTTOM {
        r.right = r.left + (h as f64 / a).round() as i32;
    } else {
        let nh = (w as f64 * a).round() as i32;
        if edge == WMSZ_TOPLEFT || edge == WMSZ_TOPRIGHT {
            r.top = r.bottom - nh;
        } else {
            r.bottom = r.top + nh;
        }
    }
}

fn hit_test(h: HWND, lp: LPARAM) -> u32 {
    let p = lp_point(lp);
    let r = window_rect(h.0 as isize);
    let e = 8 * dpi_of(h.0 as isize) / 96;
    let (l, rt, t, b) = (p.x < r.left + e, p.x >= r.right - e, p.y < r.top + e, p.y >= r.bottom - e);
    match (l, rt, t, b) {
        (true, _, true, _) => HTTOPLEFT,
        (_, true, true, _) => HTTOPRIGHT,
        (true, _, _, true) => HTBOTTOMLEFT,
        (_, true, _, true) => HTBOTTOMRIGHT,
        (true, _, _, _) => HTLEFT,
        (_, true, _, _) => HTRIGHT,
        (_, _, true, _) => HTTOP,
        (_, _, _, true) => HTBOTTOM,
        _ => HTCAPTION, // drag anywhere to move
    }
}

extern "system" fn panel_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCHITTEST => LRESULT(hit_test(h, lp) as isize),
            WM_NCLBUTTONDBLCLK => LRESULT(0), // no maximize on double-click
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_SIZING => {
                keep_aspect(wp.0 as u32, &mut *(lp.0 as *mut RECT));
                LRESULT(1)
            }
            WM_GETMINMAXINFO => {
                let mmi = &mut *(lp.0 as *mut MINMAXINFO);
                mmi.ptMinTrackSize = POINT { x: 120, y: 40 };
                LRESULT(0)
            }
            WM_SIZE => {
                update_thumb();
                place_close();
                LRESULT(0)
            }
            WM_MOVE => {
                place_close();
                LRESULT(0)
            }
            WM_EXITSIZEMOVE => {
                let r = window_rect(h.0 as isize);
                save("PrompterWidth", (r.right - r.left).max(0) as u32);
                LRESULT(0)
            }
            WM_TIMER if wp.0 == TIMER_FOLLOW => {
                // The source window closed: end, like macOS.
                let src = st(|s| s.source.get());
                if src == 0 || !IsWindow(hwnd(src)).as_bool() {
                    stop();
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(h, msg, wp, lp),
        }
    }
}

// ---- × button ------------------------------------------------------------------

extern "system" fn close_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_LBUTTONUP => {
                stop();
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(h, &mut ps);
                let mut c = RECT::default();
                let _ = GetClientRect(h, &mut c);
                fill(hdc, &c, rgb(40, 40, 46));
                let d = dpi_of(h.0 as isize);
                let pen = CreatePen(PS_SOLID, (2 * d / 96).max(1), rgb(240, 240, 245));
                let old = SelectObject(hdc, HGDIOBJ(pen.0));
                let m = c.right * 3 / 10;
                let _ = MoveToEx(hdc, m, m, None);
                let _ = LineTo(hdc, c.right - m, c.bottom - m);
                let _ = MoveToEx(hdc, c.right - m, m, None);
                let _ = LineTo(hdc, m, c.bottom - m);
                SelectObject(hdc, old);
                let _ = DeleteObject(HGDIOBJ(pen.0));
                let _ = EndPaint(h, &ps);
                LRESULT(0)
            }
            _ => DefWindowProcW(h, msg, wp, lp),
        }
    }
}
