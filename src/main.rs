#![windows_subsystem = "windows"]

//! Widget kecil selalu-di-atas: kecepatan jaringan, CPU, RAM, dan pemakaian data harian.
//!
//! Satu thread, `GetMessage` blocking + timer 1 detik. State dibaca lewat `GWLP_USERDATA`
//! sebagai `&App` dengan `RefCell`: borrow tidak pernah ditahan melintasi panggilan Win32
//! yang bisa memicu pesan lagi (`SetWindowPos`, `TrackPopupMenu`, ...).

mod stats_window;

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use resource_widget::metrics::{AdapterSel, Sample, Sampler, list_adapters};
use resource_widget::procs::ProcMonitor;
use resource_widget::render::{History, Renderer, View, base_size};
use resource_widget::settings::{self, SCALE_MAX, SCALE_MIN, Settings};
use resource_widget::usage::{Usage, today_key};
use resource_widget::wide;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const TIMER_ID: usize = 1;
/// Statistik pemakaian ditulis ke disk sekitar tiap sekian milidetik (dan saat keluar).
const FLUSH_EVERY_MS: u32 = 60_000;
/// Proses teratas dipindai sekitar tiap sekian milidetik: pemindaian ≈8 ms, jauh lebih
/// mahal dari sampel biasa, jadi sengaja jarang.
const PROC_SCAN_EVERY_MS: u32 = 5_000;
const OPACITIES: [u8; 5] = [100, 90, 80, 70, 50];
const SIZE_PRESETS: [u32; 5] = [75, 100, 125, 150, 200];
/// Pilihan selang update (ms).
const INTERVALS: [u32; 4] = [500, 1000, 2000, 5000];

// ID perintah menu.
const ID_OPACITY: usize = 100; // + indeks OPACITIES
const ID_SIZE: usize = 105; // + indeks SIZE_PRESETS
const ID_INTERVAL: usize = 300; // + indeks INTERVALS (jauh dari ID_ADAPTER_BASE + jumlah adapter)
const ID_UNITS_BYTES: usize = 110;
const ID_UNITS_BITS: usize = 111;
const ID_ADAPTER_AUTO: usize = 120;
const ID_ADAPTER_BASE: usize = 121; // + indeks adapter
const ID_TOPMOST: usize = 130;
const ID_AUTOSTART: usize = 131;
const ID_STATS: usize = 132;
const ID_PROCS: usize = 133;
const ID_COMPACT: usize = 134;
const ID_EXIT: usize = 199;

/// Semua teks antarmuka di satu tempat agar mudah diganti.
mod label {
    pub const OPACITY: &str = "Transparansi";
    pub const SIZE: &str = "Ukuran";
    pub const INTERVAL: &str = "Kecepatan update";
    pub const UNITS: &str = "Satuan";
    pub const UNITS_BYTES: &str = "Byte (KB/s, MB/s)";
    pub const UNITS_BITS: &str = "Bit (Kbps, Mbps)";
    pub const ADAPTER: &str = "Adaptor jaringan";
    pub const ADAPTER_AUTO: &str = "Otomatis (semua adapter fisik)";
    pub const ADAPTER_INACTIVE: &str = " (tidak aktif)";
    pub const COMPACT: &str = "Mode ringkas (tanpa grafik)";
    pub const PROCS: &str = "Proses teratas per metrik";
    pub const STATS: &str = "Statistik pemakaian...";
    pub const TOPMOST: &str = "Selalu di atas";
    pub const AUTOSTART: &str = "Jalankan saat Windows mulai";
    pub const EXIT: &str = "Keluar";
}

/// Awal drag sudut: posisi kursor dan ukuran jendela saat tombol ditekan.
#[derive(Clone, Copy)]
struct ResizeStart {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

struct State {
    settings: Settings,
    sampler: Sampler,
    sample: Sample,
    hist: History,
    renderer: Renderer,
    usage: Usage,
    procs: ProcMonitor,
    ticks: u32,
    resize: Option<ResizeStart>,
}

struct App {
    state: RefCell<State>,
    /// Jendela statistik yang sedang terbuka (null bila tidak ada).
    stats_hwnd: Cell<HWND>,
}

// ------------------------------------------------------------ posisi (murni)

/// Menggeser (x, y) agar kartu `w` x `h` berada penuh di dalam `work`.
fn clamp_into(work: RECT, x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
    let max_x = (work.right - w).max(work.left);
    let max_y = (work.bottom - h).max(work.top);
    (x.clamp(work.left, max_x), y.clamp(work.top, max_y))
}

/// Teks menu untuk selang update: 500 -> "0,5 detik", 2000 -> "2 detik".
fn interval_label(ms: u32) -> String {
    if ms.is_multiple_of(1000) {
        format!("{} detik", ms / 1000)
    } else {
        format!("{},{} detik", ms / 1000, ms % 1000 / 100)
    }
}

/// Jumlah tick antar-penulisan statistik ke disk agar jaraknya tetap sekitar 60 detik.
fn flush_every_ticks(interval_ms: u32) -> u32 {
    (FLUSH_EVERY_MS / interval_ms.max(1)).max(1)
}

/// Jumlah tick antar-pemindaian proses (minimal 1) agar jaraknya sekitar `PROC_SCAN_EVERY_MS`.
fn scan_every_ticks(interval_ms: u32) -> u32 {
    PROC_SCAN_EVERY_MS.div_ceil(interval_ms.max(1)).max(1)
}

/// Ikon aplikasi dari resource exe (ID 1); `small` = ukuran ikon kecil sistem (judul jendela).
/// `1 as *const u16` adalah `MAKEINTRESOURCE(1)`, bukan pointer sungguhan.
#[allow(clippy::manual_dangling_ptr)]
unsafe fn app_icon(hinstance: HINSTANCE, small: bool) -> HICON {
    unsafe {
        let (cx, cy) = if small {
            (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON))
        } else {
            (GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON))
        };
        LoadImageW(hinstance, 1usize as *const u16, IMAGE_ICON, cx, cy, LR_SHARED) as HICON
    }
}

/// Mode tampilan menurut pengaturan (ringkas mematikan grafik dan proses).
fn view_of(s: &Settings) -> View {
    View::from_flags(s.compact, s.procs)
}

/// Skala (persen, kelipatan 5) agar lebar kartu `base_w` (logis) mendekati `width_px` pada `dpi`.
fn scale_for_width(width_px: i32, dpi: u32, base_w: i32) -> u32 {
    let raw = width_px.max(1) as i64 * 9600 / (base_w.max(1) as i64 * dpi.max(1) as i64);
    (((raw + 2) / 5 * 5) as u32).clamp(SCALE_MIN, SCALE_MAX)
}

/// Posisi kiri baru saat lebar kartu berubah dari `old_w` ke `new_w`: kartu yang berada di
/// separuh kanan area kerja menjaga tepi kanannya, selain itu menjaga tepi kirinya.
fn left_after_resize(left: i32, old_w: i32, new_w: i32, work: RECT) -> i32 {
    let center = left + old_w / 2;
    if center > work.left + (work.right - work.left) / 2 { left + old_w - new_w } else { left }
}

// ------------------------------------------------------------------- Win32

unsafe fn work_area_near(x: i32, y: i32, w: i32, h: i32) -> RECT {
    unsafe {
        let mon = MonitorFromPoint(POINT { x: x + w / 2, y: y + h / 2 }, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(mon, &mut mi) != 0 {
            mi.rcWork
        } else {
            RECT { left: 0, top: 0, right: 1920, bottom: 1080 }
        }
    }
}

/// Posisi awal: tersimpan, atau pojok kanan atas area kerja monitor utama.
unsafe fn initial_pos(s: &Settings, w: i32, dpi: u32) -> (i32, i32) {
    if let Some(p) = s.pos {
        return p;
    }
    unsafe {
        let mut work: RECT = zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work as *mut RECT).cast(), 0);
        let margin = (16 * dpi as i32) / 96;
        (work.right - w - margin, work.top + margin)
    }
}

/// Memindahkan/mengubah ukuran jendela dengan posisi dijepit ke area kerja.
unsafe fn place(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        let (x, y) = clamp_into(work_area_near(x, y, w, h), x, y, w, h);
        SetWindowPos(hwnd, null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

unsafe fn apply_opacity(hwnd: HWND, percent: u8) {
    unsafe {
        SetLayeredWindowAttributes(hwnd, 0, (percent as u32 * 255 / 100) as u8, LWA_ALPHA);
    }
}

unsafe fn apply_topmost(hwnd: HWND, on: bool) {
    unsafe {
        let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
        SetWindowPos(hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

/// Sudut membulat: atribut DWM Windows 11 (halus, anti-alias).
unsafe fn apply_corners(hwnd: HWND) {
    unsafe {
        let pref: i32 = DWMWCP_ROUND;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&pref as *const i32).cast(),
            size_of::<i32>() as u32,
        );
    }
}

/// Mengubah skala kartu: membuat ulang objek GDI dan mengubah ukuran jendela
/// (pojok kiri-atas tetap). Penyimpanan ke disk dilakukan pemanggil.
unsafe fn apply_scale(hwnd: HWND, app: &App, pct: u32) {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        let size = {
            let mut st = app.state.borrow_mut();
            if st.settings.scale == pct {
                None
            } else {
                st.settings.scale = pct;
                let view = view_of(&st.settings);
                st.renderer.rebuild(dpi, pct, view);
                Some(st.renderer.size())
            }
        };
        if let Some((w, h)) = size {
            let mut r: RECT = zeroed();
            GetWindowRect(hwnd, &mut r);
            place(hwnd, r.left, r.top, w, h);
            InvalidateRect(hwnd, null(), 0);
        }
    }
}

/// Menerapkan perubahan mode (ringkas / proses teratas): kartu mengikuti lebar barunya tanpa
/// ruang kosong. Posisi disimpan karena tepi kartu bisa bergeser.
unsafe fn apply_view(hwnd: HWND, app: &App) {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        let (w, h) = {
            let mut st = app.state.borrow_mut();
            let (pct, view) = (st.settings.scale, view_of(&st.settings));
            st.renderer.rebuild(dpi, pct, view);
            st.renderer.size()
        };
        let mut r: RECT = zeroed();
        GetWindowRect(hwnd, &mut r);
        let work = work_area_near(r.left, r.top, r.right - r.left, r.bottom - r.top);
        place(hwnd, left_after_resize(r.left, r.right - r.left, w, work), r.top, w, h);
        GetWindowRect(hwnd, &mut r);
        {
            let mut st = app.state.borrow_mut();
            st.settings.pos = Some((r.left, r.top));
            st.settings.save();
        }
        InvalidateRect(hwnd, null(), 0);
    }
}

unsafe fn end_resize(app: &App) {
    unsafe {
        let was_resizing = {
            let mut st = app.state.borrow_mut();
            let was = st.resize.take().is_some();
            if was {
                st.settings.save();
            }
            was
        };
        if was_resizing {
            ReleaseCapture();
        }
    }
}

// --------------------------------------------------------------------- menu

unsafe fn item(menu: HMENU, id: usize, text: &str, checked: bool) {
    unsafe {
        let flags = MF_STRING | if checked { MF_CHECKED } else { 0 };
        AppendMenuW(menu, flags, id, wide(text).as_ptr());
    }
}

/// Item menu yang bisa dinonaktifkan (abu-abu) dan tetap menampilkan status centangnya.
unsafe fn item_state(menu: HMENU, id: usize, text: &str, checked: bool, disabled: bool) {
    unsafe {
        let flags = MF_STRING | if checked { MF_CHECKED } else { 0 } | if disabled { MF_GRAYED } else { 0 };
        AppendMenuW(menu, flags, id, wide(text).as_ptr());
    }
}

unsafe fn submenu(menu: HMENU, text: &str, sub: HMENU) {
    unsafe {
        AppendMenuW(menu, MF_POPUP, sub as usize, wide(text).as_ptr());
    }
}

unsafe fn show_menu(hwnd: HWND, app: &'static App, x: i32, y: i32) {
    unsafe {
        let cur = app.state.borrow().settings.clone();
        let active = list_adapters();
        // (nama untuk dicocokkan, label di menu)
        let mut adapters: Vec<(String, String)> =
            active.iter().map(|a| (a.clone(), a.replace('&', "&&"))).collect();
        if let AdapterSel::Named(n) = &cur.adapter
            && !active.iter().any(|a| a.eq_ignore_ascii_case(n))
        {
            adapters.push((n.clone(), format!("{}{}", n.replace('&', "&&"), label::ADAPTER_INACTIVE)));
        }

        let menu = CreatePopupMenu();

        let m = CreatePopupMenu();
        for (i, o) in OPACITIES.iter().enumerate() {
            item(m, ID_OPACITY + i, &format!("{o} %"), cur.opacity == *o);
        }
        submenu(menu, label::OPACITY, m);

        let m = CreatePopupMenu();
        for (i, p) in SIZE_PRESETS.iter().enumerate() {
            item(m, ID_SIZE + i, &format!("{p} %"), cur.scale == *p);
        }
        submenu(menu, label::SIZE, m);

        let m = CreatePopupMenu();
        for (i, ms) in INTERVALS.iter().enumerate() {
            item(m, ID_INTERVAL + i, &interval_label(*ms), cur.interval_ms == *ms);
        }
        submenu(menu, label::INTERVAL, m);

        let m = CreatePopupMenu();
        item(m, ID_UNITS_BYTES, label::UNITS_BYTES, !cur.bits);
        item(m, ID_UNITS_BITS, label::UNITS_BITS, cur.bits);
        submenu(menu, label::UNITS, m);

        let m = CreatePopupMenu();
        item(m, ID_ADAPTER_AUTO, label::ADAPTER_AUTO, cur.adapter == AdapterSel::Auto);
        for (i, (name, text)) in adapters.iter().enumerate() {
            let selected = matches!(&cur.adapter, AdapterSel::Named(n) if n.eq_ignore_ascii_case(name));
            item(m, ID_ADAPTER_BASE + i, text, selected);
        }
        submenu(menu, label::ADAPTER, m);

        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        item(menu, ID_COMPACT, label::COMPACT, cur.compact);
        // Mode ringkas tidak punya kolom proses: pilihannya tetap tersimpan tetapi dinonaktifkan.
        item_state(menu, ID_PROCS, label::PROCS, cur.procs, cur.compact);
        item(menu, ID_STATS, label::STATS, false);
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        item(menu, ID_TOPMOST, label::TOPMOST, cur.topmost);
        item(menu, ID_AUTOSTART, label::AUTOSTART, settings::autostart_enabled());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        item(menu, ID_EXIT, label::EXIT, false);

        // Idiom wajib agar menu tertutup saat klik di luar.
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, x, y, 0, hwnd, null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);

        if cmd > 0 {
            let names: Vec<String> = adapters.into_iter().map(|(n, _)| n).collect();
            on_command(hwnd, app, cmd as usize, &names);
        }
    }
}

unsafe fn on_command(hwnd: HWND, app: &'static App, id: usize, adapters: &[String]) {
    unsafe {
        match id {
            ID_EXIT => {
                DestroyWindow(hwnd);
                return;
            }
            ID_AUTOSTART => {
                settings::set_autostart(!settings::autostart_enabled());
                return;
            }
            ID_STATS => {
                stats_window::open(app, hwnd);
                return;
            }
            _ => {}
        }

        let mut new_scale = None;
        let mut new_interval = None;
        let mut view_changed = false;
        let (opacity, topmost) = {
            let mut guard = app.state.borrow_mut();
            let st = &mut *guard;
            if (ID_OPACITY..ID_OPACITY + OPACITIES.len()).contains(&id) {
                st.settings.opacity = OPACITIES[id - ID_OPACITY];
            } else if (ID_INTERVAL..ID_INTERVAL + INTERVALS.len()).contains(&id) {
                st.settings.interval_ms = INTERVALS[id - ID_INTERVAL];
                new_interval = Some(st.settings.interval_ms);
            } else if (ID_SIZE..ID_SIZE + SIZE_PRESETS.len()).contains(&id) {
                new_scale = Some(SIZE_PRESETS[id - ID_SIZE]);
            } else if id == ID_UNITS_BYTES || id == ID_UNITS_BITS {
                st.settings.bits = id == ID_UNITS_BITS;
            } else if id == ID_ADAPTER_AUTO {
                st.settings.adapter = AdapterSel::Auto;
                st.sampler.set_adapter(AdapterSel::Auto);
            } else if id >= ID_ADAPTER_BASE && id - ID_ADAPTER_BASE < adapters.len() {
                let sel = AdapterSel::Named(adapters[id - ID_ADAPTER_BASE].clone());
                st.settings.adapter = sel.clone();
                st.sampler.set_adapter(sel);
            } else if id == ID_TOPMOST {
                st.settings.topmost = !st.settings.topmost;
            } else if id == ID_PROCS || id == ID_COMPACT {
                if id == ID_PROCS {
                    st.settings.procs = !st.settings.procs;
                } else {
                    st.settings.compact = !st.settings.compact;
                }
                view_changed = true;
                if view_of(&st.settings).procs {
                    st.procs.scan(0); // baseline agar pemindaian berikutnya punya pembanding
                }
            }
            st.settings.save();
            (st.settings.opacity, st.settings.topmost)
        };
        if let Some(pct) = new_scale {
            apply_scale(hwnd, app, pct);
            app.state.borrow().settings.save();
        }
        if let Some(ms) = new_interval {
            SetTimer(hwnd, TIMER_ID, ms, None); // SetTimer dengan ID sama mengganti selangnya
        }
        if view_changed {
            apply_view(hwnd, app);
        }
        apply_opacity(hwnd, opacity);
        apply_topmost(hwnd, topmost);
        InvalidateRect(hwnd, null(), 0);
    }
}

// ------------------------------------------------------------------ wndproc

fn point_of(lp: LPARAM) -> (i32, i32) {
    ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lp as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let app = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const App;
        if app.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        // App sengaja di-leak di main(), jadi hidup selama proses.
        let app: &'static App = &*app;

        match msg {
            // Seluruh kartu bisa di-drag, kecuali pegangan ukuran di sudut kanan-bawah
            // (HTCLIENT agar kita menerima pesan mouse-nya).
            WM_NCHITTEST => {
                let (x, y) = point_of(lp);
                let mut r: RECT = zeroed();
                GetWindowRect(hwnd, &mut r);
                if app.state.borrow().renderer.grip_hit(x - r.left, y - r.top) {
                    HTCLIENT as LRESULT
                } else {
                    HTCAPTION as LRESULT
                }
            }
            WM_SETCURSOR if (lp & 0xFFFF) as u32 == HTCLIENT => {
                SetCursor(LoadCursorW(null_mut(), IDC_SIZENWSE));
                1
            }
            WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);
                {
                    let mut guard = app.state.borrow_mut();
                    let st = &mut *guard;
                    let today = st.usage.day(today_key());
                    let procs = view_of(&st.settings).procs.then(|| st.procs.top());
                    st.renderer.paint(hdc, &st.sample, &st.hist, st.settings.bits, today, procs);
                }
                EndPaint(hwnd, &ps);
                0
            }
            WM_TIMER if wp == TIMER_ID => {
                {
                    let mut guard = app.state.borrow_mut();
                    let st = &mut *guard;
                    let s = st.sampler.sample();
                    st.hist.push(&s);
                    st.usage.add(today_key(), s.rx_bytes, s.tx_bytes);
                    st.sample = s;
                    st.ticks = st.ticks.wrapping_add(1);
                    if view_of(&st.settings).procs && st.ticks.is_multiple_of(scan_every_ticks(st.settings.interval_ms)) {
                        st.procs.scan(s.down + s.up);
                    }
                    if st.ticks.is_multiple_of(flush_every_ticks(st.settings.interval_ms)) {
                        st.usage.save_if_dirty();
                    }
                }
                InvalidateRect(hwnd, null(), 0);
                stats_window::refresh(app);
                0
            }
            // DefWindowProc tidak membuat WM_CONTEXTMENU dari klik kanan di area HTCAPTION
            // (seluruh kartu), jadi WM_NCRBUTTONUP ditangani langsung.
            WM_NCRBUTTONUP | WM_CONTEXTMENU => {
                let (mut x, mut y) = point_of(lp);
                if msg == WM_CONTEXTMENU && x == -1 && y == -1 {
                    let mut r: RECT = zeroed();
                    GetWindowRect(hwnd, &mut r);
                    (x, y) = (r.left, r.bottom);
                }
                show_menu(hwnd, app, x, y);
                0
            }
            // Hanya build debug: memungkinkan uji otomatis tanpa menu (ID sama dengan item menu).
            // Build rilis tidak menerima perintah dari proses lain.
            #[cfg(debug_assertions)]
            WM_COMMAND => {
                on_command(hwnd, app, wp & 0xFFFF, &[]);
                0
            }
            WM_NCLBUTTONDBLCLK if wp == HTCAPTION as usize => {
                stats_window::open(app, hwnd);
                0
            }
            // --- ubah ukuran dengan menarik sudut kanan-bawah (proporsional) ---
            WM_LBUTTONDOWN => {
                let mut pt: POINT = zeroed();
                let mut r: RECT = zeroed();
                GetCursorPos(&mut pt);
                GetWindowRect(hwnd, &mut r);
                app.state.borrow_mut().resize =
                    Some(ResizeStart { x: pt.x, y: pt.y, w: r.right - r.left, h: r.bottom - r.top });
                SetCapture(hwnd);
                0
            }
            WM_MOUSEMOVE => {
                let start = app.state.borrow().resize;
                if let Some(s) = start {
                    let mut pt: POINT = zeroed();
                    GetCursorPos(&mut pt);
                    let (base_w, base_h) = base_size(view_of(&app.state.borrow().settings));
                    let by_x = s.w + (pt.x - s.x);
                    let by_y = (s.h + (pt.y - s.y)) * base_w / base_h;
                    apply_scale(hwnd, app, scale_for_width(by_x.max(by_y), GetDpiForWindow(hwnd), base_w));
                }
                0
            }
            WM_LBUTTONUP | WM_CAPTURECHANGED => {
                end_resize(app);
                0
            }
            WM_EXITSIZEMOVE => {
                let mut r: RECT = zeroed();
                GetWindowRect(hwnd, &mut r);
                let mut st = app.state.borrow_mut();
                st.settings.pos = Some((r.left, r.top));
                st.settings.save();
                0
            }
            WM_DPICHANGED => {
                let dpi = (wp & 0xFFFF) as u32;
                let (w, h) = {
                    let mut st = app.state.borrow_mut();
                    let (pct, view) = (st.settings.scale, view_of(&st.settings));
                    st.renderer.rebuild(dpi, pct, view);
                    st.renderer.size()
                };
                let r = &*(lp as *const RECT);
                place(hwnd, r.left, r.top, w, h);
                InvalidateRect(hwnd, null(), 0);
                0
            }
            WM_DISPLAYCHANGE => {
                reclamp(hwnd);
                0
            }
            WM_SETTINGCHANGE if wp == SPI_SETWORKAREA as usize => {
                reclamp(hwnd);
                0
            }
            WM_ENDSESSION => {
                if wp != 0 {
                    app.state.borrow_mut().usage.save_if_dirty();
                }
                0
            }
            WM_DESTROY => {
                app.state.borrow_mut().usage.save_if_dirty();
                KillTimer(hwnd, TIMER_ID);
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Menjaga kartu tetap terlihat setelah resolusi/monitor/taskbar berubah.
unsafe fn reclamp(hwnd: HWND) {
    unsafe {
        let mut r: RECT = zeroed();
        GetWindowRect(hwnd, &mut r);
        place(hwnd, r.left, r.top, r.right - r.left, r.bottom - r.top);
    }
}

fn main() {
    unsafe {
        let name = wide("Local\\ResourceWidget");
        let mutex = CreateMutexW(null(), 0, name.as_ptr());
        if mutex.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
            return; // sudah berjalan
        }

        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let settings = Settings::load();
        let dpi0 = GetDpiForSystem();
        let renderer = Renderer::new(dpi0, settings.scale, view_of(&settings));
        let (w, h) = renderer.size();
        let mut sampler = Sampler::new(settings.adapter.clone());
        let first = sampler.sample(); // baseline; kecepatan baru valid mulai tick berikutnya
        let (x, y) = initial_pos(&settings, w, dpi0);
        let (opacity, topmost, interval_ms) = (settings.opacity, settings.topmost, settings.interval_ms);
        let mut procs = ProcMonitor::new();
        if view_of(&settings).procs {
            procs.scan(0); // baseline; proses teratas baru terisi pada pemindaian berikutnya
        }

        let app: &'static App = Box::leak(Box::new(App {
            state: RefCell::new(State {
                settings,
                sampler,
                sample: first,
                hist: History::new(),
                renderer,
                usage: Usage::load(),
                procs,
                ticks: 0,
                resize: None,
            }),
            stats_hwnd: Cell::new(null_mut()),
        }));

        let hinstance = GetModuleHandleW(null());
        let class = wide("ResourceWidgetWnd");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS, // agar klik ganda pada area drag (HTCAPTION) terdeteksi
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: app_icon(hinstance, false),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: app_icon(hinstance, true),
        };
        if RegisterClassExW(&wc) == 0 {
            return;
        }

        let mut ex = WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE;
        if topmost {
            ex |= WS_EX_TOPMOST;
        }
        let title = wide("Resource Widget");
        let hwnd = CreateWindowExW(
            ex,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            x,
            y,
            w,
            h,
            null_mut(),
            null_mut(),
            hinstance,
            (app as *const App).cast::<c_void>(),
        );
        if hwnd.is_null() {
            return;
        }

        // Monitor tempat jendela mendarat bisa ber-DPI lain dari monitor utama.
        let dpi = GetDpiForWindow(hwnd);
        let (w, h) = {
            let mut st = app.state.borrow_mut();
            if dpi != 0 && dpi != dpi0 {
                let (pct, view) = (st.settings.scale, view_of(&st.settings));
                st.renderer.rebuild(dpi, pct, view);
            }
            st.renderer.size()
        };
        place(hwnd, x, y, w, h);
        apply_opacity(hwnd, opacity);
        apply_corners(hwnd);

        SetTimer(hwnd, TIMER_ID, interval_ms, None);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        UpdateWindow(hwnd);

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: RECT = RECT { left: 0, top: 0, right: 1920, bottom: 1040 };

    #[test]
    fn clamp_keeps_card_inside_work_area() {
        assert_eq!(clamp_into(WORK, 100, 100, 240, 138), (100, 100));
        assert_eq!(clamp_into(WORK, -500, -50, 240, 138), (0, 0)); // monitor kiri dicabut
        assert_eq!(clamp_into(WORK, 5000, 5000, 240, 138), (1680, 902));
    }

    #[test]
    fn clamp_handles_secondary_monitor_with_negative_origin() {
        let left = RECT { left: -1920, top: 0, right: 0, bottom: 1080 };
        assert_eq!(clamp_into(left, -100, 1000, 240, 138), (-240, 942));
    }

    #[test]
    fn process_scan_spacing_follows_interval() {
        assert_eq!(scan_every_ticks(500), 10);
        assert_eq!(scan_every_ticks(1000), 5);
        assert_eq!(scan_every_ticks(2000), 3);
        assert_eq!(scan_every_ticks(5000), 1);
        assert_eq!(scan_every_ticks(10_000), 1); // tak pernah 0
        assert_eq!(scan_every_ticks(0), 5_000);
    }

    #[test]
    fn interval_labels_and_flush_spacing() {
        assert_eq!(interval_label(500), "0,5 detik");
        assert_eq!(interval_label(1000), "1 detik");
        assert_eq!(interval_label(5000), "5 detik");
        assert_eq!(flush_every_ticks(500), 120);
        assert_eq!(flush_every_ticks(1000), 60);
        assert_eq!(flush_every_ticks(5000), 12);
        assert_eq!(flush_every_ticks(0), 60_000); // aman terhadap nilai tak valid
        assert!(flush_every_ticks(120_000) >= 1);
    }

    #[test]
    fn scale_follows_dragged_width() {
        let b = base_size(View::FULL).0;
        assert_eq!(scale_for_width(b, 96, b), 100);
        assert_eq!(scale_for_width(b * 3 / 2, 96, b), 150);
        assert_eq!(scale_for_width(b / 2, 96, b), 50);
        assert_eq!(scale_for_width(b * 2, 192, b), 100); // DPI 200 %, skala tetap 100
        assert_eq!(scale_for_width(3, 96, b), SCALE_MIN);
        assert_eq!(scale_for_width(100_000, 96, b), SCALE_MAX);
        assert_eq!(scale_for_width(-50, 96, b), SCALE_MIN);
        // hasil selalu kelipatan 5
        assert_eq!(scale_for_width(b * 113 / 100, 96, b) % 5, 0);
        // lebar dasar mode ringkas lebih sempit: lebar piksel yang sama berarti skala lebih besar
        let narrow = base_size(View { graphs: false, procs: false }).0;
        assert!(scale_for_width(b, 96, narrow) > scale_for_width(b, 96, b));
    }

    #[test]
    fn card_keeps_the_edge_nearest_the_screen_side_when_width_changes() {
        let work = RECT { left: 0, top: 0, right: 1920, bottom: 1040 };
        // kartu di kanan layar: tepi kanan tetap, kiri bergeser
        assert_eq!(left_after_resize(1660, 248, 184, work), 1724);
        // kartu di kiri layar: tepi kiri tetap
        assert_eq!(left_after_resize(100, 248, 184, work), 100);
        // melebar di kanan layar: kiri mundur agar tepi kanan sama
        assert_eq!(left_after_resize(1724, 184, 248, work), 1660);
    }
}
