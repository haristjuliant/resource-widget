//! Jendela "Statistik pemakaian": ringkasan, tabel bulanan, dan tabel harian.
//! Bisa di-scroll dengan roda mouse atau tombol PgUp/PgDn/Home/End; Esc menutup.

use std::cell::Cell;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use resource_widget::render::{BG, DIM, DOWN, TEXT, TRACK, UP, rgb};
use resource_widget::usage::{Kind, build_lines, today_key};
use resource_widget::wide;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_NEXT, VK_PRIOR, VK_UP};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::App;

const CLASS: &str = "ResourceWidgetStats";
const TITLE: &str = "Statistik pemakaian";
const FOOTER: &str = "Dihitung hanya saat widget berjalan, sesuai adapter yang dipilih.";

// Ukuran dalam satuan logis (96-DPI).
const CLIENT_W: i32 = 520;
const CLIENT_H: i32 = 500;
const PAD: i32 = 16;
const FOOTER_H: i32 = 30;
const ROW_H: i32 = 22;
const HEAD_H: i32 = 24;
const SECTION_H: i32 = 36;
// Tepi kanan tiap kolom: label | unduh | unggah | total.
const COL_LABEL_R: i32 = 262;
const COL_DOWN_R: i32 = 332;
const COL_UP_R: i32 = 414;
const COL_TOTAL_R: i32 = CLIENT_W - PAD;

#[derive(Clone, Copy)]
struct Gdi {
    text: HFONT,
    head: HFONT,
    section: HFONT,
    bg: HBRUSH,
    line: HBRUSH,
    thumb: HBRUSH,
}

struct Ctx {
    app: &'static App,
    dpi: Cell<u32>,
    scroll: Cell<i32>,
    gdi: Cell<Gdi>,
}

fn px(v: i32, dpi: u32) -> i32 {
    (v * dpi as i32 + 48) / 96
}

fn line_height(kind: Kind) -> i32 {
    match kind {
        Kind::Section => SECTION_H,
        Kind::Header => HEAD_H,
        Kind::Row => ROW_H,
    }
}

unsafe fn make_gdi(dpi: u32) -> Gdi {
    unsafe {
        let font = |h: i32, weight: i32| {
            let face = wide("Segoe UI");
            CreateFontW(
                -px(h, dpi),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                DEFAULT_PITCH as u32,
                face.as_ptr(),
            )
        };
        Gdi {
            text: font(13, 400),
            head: font(11, 400),
            section: font(15, 600),
            bg: CreateSolidBrush(rgb(BG.0, BG.1, BG.2)),
            line: CreateSolidBrush(rgb(TRACK.0, TRACK.1, TRACK.2)),
            thumb: CreateSolidBrush(rgb(78, 84, 100)),
        }
    }
}

unsafe fn free_gdi(g: Gdi) {
    unsafe {
        for h in [g.text, g.head, g.section, g.bg, g.line, g.thumb] {
            DeleteObject(h);
        }
    }
}

unsafe fn text(dc: HDC, font: HFONT, color: COLORREF, s: &str, mut r: RECT, flags: u32) {
    let mut buf = [0u16; 72];
    let mut n = 0;
    for u in s.encode_utf16().take(buf.len()) {
        buf[n] = u;
        n += 1;
    }
    unsafe {
        SelectObject(dc, font);
        SetTextColor(dc, color);
        DrawTextW(dc, buf.as_ptr(), n as i32, &mut r, flags | DT_SINGLELINE | DT_NOPREFIX);
    }
}

/// Meminta jendela menggambar ulang (dipanggil tiap tick agar angka hari ini hidup).
pub fn refresh(app: &App) {
    let h = app.stats_hwnd.get();
    if !h.is_null() {
        unsafe {
            InvalidateRect(h, null(), 0);
        }
    }
}

/// Membuka jendela statistik, atau membawa yang sudah ada ke depan.
pub unsafe fn open(app: &'static App, owner: HWND) {
    unsafe {
        let existing = app.stats_hwnd.get();
        if !existing.is_null() {
            ShowWindow(existing, SW_RESTORE);
            SetForegroundWindow(existing);
            return;
        }

        let hinstance = GetModuleHandleW(null());
        let class = wide(CLASS);
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(stats_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: null_mut(),
        };
        RegisterClassExW(&wc); // gagal bila sudah terdaftar; tidak masalah

        let dpi = GetDpiForWindow(owner).max(96);
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut r = RECT { left: 0, top: 0, right: px(CLIENT_W, dpi), bottom: px(CLIENT_H, dpi) };
        AdjustWindowRectExForDpi(&mut r, style, 0, 0, dpi);
        let (w, h) = (r.right - r.left, r.bottom - r.top);

        // Tengah area kerja monitor tempat widget berada.
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST), &mut mi);
        let x = mi.rcWork.left + ((mi.rcWork.right - mi.rcWork.left - w) / 2).max(0);
        let y = mi.rcWork.top + ((mi.rcWork.bottom - mi.rcWork.top - h) / 2).max(0);

        let ctx = Box::into_raw(Box::new(Ctx {
            app,
            dpi: Cell::new(dpi),
            scroll: Cell::new(0),
            gdi: Cell::new(zeroed()),
        }));
        let title = wide(TITLE);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            style,
            x,
            y,
            w,
            h,
            null_mut(),
            null_mut(),
            hinstance,
            ctx.cast::<c_void>(),
        );
        if hwnd.is_null() {
            drop(Box::from_raw(ctx));
            return;
        }
        app.stats_hwnd.set(hwnd);
        let dark: i32 = 1;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            (&dark as *const i32).cast(),
            size_of::<i32>() as u32,
        );
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }
}

unsafe fn paint(hwnd: HWND, ctx: &Ctx) {
    unsafe {
        let mut ps: PAINTSTRUCT = zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc: RECT = zeroed();
        GetClientRect(hwnd, &mut rc);
        let (w, h) = (rc.right, rc.bottom);
        let dpi = ctx.dpi.get();
        let g = ctx.gdi.get();

        let lines = {
            let st = ctx.app.state.borrow();
            build_lines(&st.usage, today_key())
        };

        let dc = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        let old = SelectObject(dc, bmp);
        FillRect(dc, &rc, g.bg);
        SetBkMode(dc, TRANSPARENT as i32);

        let view_top = px(PAD, dpi);
        let view_bottom = h - px(FOOTER_H, dpi);
        let view_h = (view_bottom - view_top).max(1);
        let content_h: i32 = lines.iter().map(|l| px(line_height(l.kind), dpi)).sum();
        let max_scroll = (content_h - view_h).max(0);
        let scroll = ctx.scroll.get().clamp(0, max_scroll);
        ctx.scroll.set(scroll);

        IntersectClipRect(dc, 0, view_top, w, view_bottom);
        let col = |l: i32, r: i32, y: i32, lh: i32| RECT { left: px(l, dpi), top: y, right: px(r, dpi), bottom: y + lh };
        let mut y = view_top - scroll;
        for l in &lines {
            let lh = px(line_height(l.kind), dpi);
            if y + lh > view_top && y < view_bottom {
                match l.kind {
                    Kind::Section => {
                        text(dc, g.section, TEXT, &l.cols[0], col(PAD, COL_TOTAL_R, y, lh), DT_LEFT | DT_BOTTOM);
                    }
                    Kind::Header => {
                        text(dc, g.head, DIM, &l.cols[0], col(PAD, COL_LABEL_R, y, lh), DT_LEFT | DT_VCENTER);
                        text(dc, g.head, DIM, &l.cols[1], col(COL_LABEL_R, COL_DOWN_R, y, lh), DT_RIGHT | DT_VCENTER);
                        text(dc, g.head, DIM, &l.cols[2], col(COL_DOWN_R + 8, COL_UP_R, y, lh), DT_RIGHT | DT_VCENTER);
                        text(dc, g.head, DIM, &l.cols[3], col(COL_UP_R + 8, COL_TOTAL_R, y, lh), DT_RIGHT | DT_VCENTER);
                        let rule = RECT {
                            left: px(PAD, dpi),
                            top: y + lh - px(1, dpi).max(1),
                            right: px(COL_TOTAL_R, dpi),
                            bottom: y + lh,
                        };
                        FillRect(dc, &rule, g.line);
                    }
                    Kind::Row => {
                        let (down, up) = (rgb(DOWN.0, DOWN.1, DOWN.2), rgb(UP.0, UP.1, UP.2));
                        text(dc, g.text, TEXT, &l.cols[0], col(PAD, COL_LABEL_R, y, lh), DT_LEFT | DT_VCENTER);
                        text(dc, g.text, down, &l.cols[1], col(COL_LABEL_R, COL_DOWN_R, y, lh), DT_RIGHT | DT_VCENTER);
                        text(dc, g.text, up, &l.cols[2], col(COL_DOWN_R + 8, COL_UP_R, y, lh), DT_RIGHT | DT_VCENTER);
                        text(dc, g.text, TEXT, &l.cols[3], col(COL_UP_R + 8, COL_TOTAL_R, y, lh), DT_RIGHT | DT_VCENTER);
                    }
                }
            }
            y += lh;
        }
        SelectClipRgn(dc, null_mut());

        // thumb scroll
        if max_scroll > 0 {
            let thumb_h = (view_h * view_h / content_h).max(px(24, dpi));
            let top = view_top + (view_h - thumb_h) * scroll / max_scroll;
            let r = RECT { left: w - px(6, dpi), top, right: w - px(3, dpi), bottom: top + thumb_h };
            FillRect(dc, &r, g.thumb);
        }

        // footer
        let rule = RECT { left: px(PAD, dpi), top: view_bottom, right: w - px(PAD, dpi), bottom: view_bottom + px(1, dpi).max(1) };
        FillRect(dc, &rule, g.line);
        let foot = RECT { left: px(PAD, dpi), top: view_bottom, right: w - px(PAD, dpi), bottom: h };
        text(dc, g.head, DIM, FOOTER, foot, DT_LEFT | DT_VCENTER);

        BitBlt(hdc, 0, 0, w, h, dc, 0, 0, SRCCOPY);
        SelectObject(dc, old);
        DeleteObject(bmp);
        DeleteDC(dc);
        EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn stats_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lp as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ctx;
        if p.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let ctx = &*p;
        let dpi = ctx.dpi.get();

        let scroll_by = |delta: i32| {
            ctx.scroll.set(ctx.scroll.get().saturating_add(delta));
            InvalidateRect(hwnd, null(), 0);
        };

        match msg {
            WM_CREATE => {
                ctx.gdi.set(make_gdi(dpi));
                0
            }
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                paint(hwnd, ctx);
                0
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp >> 16) & 0xFFFF) as i16 as i32;
                scroll_by(-delta * px(ROW_H * 3, dpi) / 120);
                0
            }
            WM_KEYDOWN => {
                match wp as u16 {
                    VK_ESCAPE => {
                        DestroyWindow(hwnd);
                    }
                    VK_UP => scroll_by(-px(ROW_H, dpi)),
                    VK_DOWN => scroll_by(px(ROW_H, dpi)),
                    VK_PRIOR => scroll_by(-px(ROW_H * 10, dpi)),
                    VK_NEXT => scroll_by(px(ROW_H * 10, dpi)),
                    VK_HOME => scroll_by(i32::MIN / 2),
                    VK_END => scroll_by(i32::MAX / 2),
                    _ => {}
                }
                0
            }
            WM_DPICHANGED => {
                let new_dpi = (wp & 0xFFFF) as u32;
                free_gdi(ctx.gdi.get());
                ctx.dpi.set(new_dpi);
                ctx.gdi.set(make_gdi(new_dpi));
                let r = &*(lp as *const RECT);
                SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                InvalidateRect(hwnd, null(), 0);
                0
            }
            WM_DESTROY => {
                free_gdi(ctx.gdi.get());
                ctx.app.stats_hwnd.set(null_mut());
                0
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(p));
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
