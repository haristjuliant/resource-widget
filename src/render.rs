//! Render GDI murni: tata letak (satuan 96-DPI yang di-skala), grafik, bar, dan teks.

use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{COLORREF, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::*;

use crate::metrics::Sample;
use crate::usage::fmt_bytes;
use crate::wide;

pub const HISTORY_LEN: usize = 60;
/// Batas bawah skala grafik (byte/dtk) agar noise kecil tidak memenuhi grafik.
const GRAPH_FLOOR: u64 = 128 * 1024;
/// Persentase mulai dari mana bar berwarna merah.
const HOT_PERCENT: f32 = 90.0;

// ------------------------------------------------------------ logika murni

/// Ring buffer riwayat (1 sampel per detik): kecepatan jaringan (byte/dtk),
/// serta beban CPU dan RAM (persen 0..=100).
pub struct History {
    down: [u64; HISTORY_LEN],
    up: [u64; HISTORY_LEN],
    cpu: [u8; HISTORY_LEN],
    mem: [u8; HISTORY_LEN],
    head: usize,
}

impl History {
    pub fn new() -> Self {
        History { down: [0; HISTORY_LEN], up: [0; HISTORY_LEN], cpu: [0; HISTORY_LEN], mem: [0; HISTORY_LEN], head: 0 }
    }

    pub fn push(&mut self, down: u64, up: u64, cpu_pct: f32, mem_pct: f32) {
        self.down[self.head] = down;
        self.up[self.head] = up;
        self.cpu[self.head] = cpu_pct.clamp(0.0, 100.0).round() as u8;
        self.mem[self.head] = mem_pct.clamp(0.0, 100.0).round() as u8;
        self.head = (self.head + 1) % HISTORY_LEN;
    }

    /// Dari yang tertua ke terbaru.
    pub fn down(&self) -> impl Iterator<Item = u64> + '_ {
        (0..HISTORY_LEN).map(move |i| self.down[(self.head + i) % HISTORY_LEN])
    }

    pub fn up(&self) -> impl Iterator<Item = u64> + '_ {
        (0..HISTORY_LEN).map(move |i| self.up[(self.head + i) % HISTORY_LEN])
    }

    pub fn cpu(&self) -> impl Iterator<Item = u64> + '_ {
        (0..HISTORY_LEN).map(move |i| self.cpu[(self.head + i) % HISTORY_LEN] as u64)
    }

    pub fn mem(&self) -> impl Iterator<Item = u64> + '_ {
        (0..HISTORY_LEN).map(move |i| self.mem[(self.head + i) % HISTORY_LEN] as u64)
    }

    pub fn peak(&self) -> u64 {
        self.down.iter().chain(&self.up).copied().max().unwrap_or(0)
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

/// Mengubah byte/dtk menjadi (angka, satuan) dengan skala otomatis.
/// Byte memakai basis 1024 (KB/s, MB/s); bit memakai basis 1000 (Kbps, Mbps).
pub fn fmt_rate(bytes_per_sec: u64, bits: bool) -> (String, &'static str) {
    let (mut v, base, units) = if bits {
        (bytes_per_sec as f64 * 8.0, 1000.0, ["bps", "Kbps", "Mbps", "Gbps"])
    } else {
        (bytes_per_sec as f64, 1024.0, ["B/s", "KB/s", "MB/s", "GB/s"])
    };
    let mut i = 0;
    while v >= 999.5 && i < 3 {
        v /= base;
        i += 1;
    }
    let s = if i == 0 {
        format!("{v:.0}")
    } else if v < 9.995 {
        format!("{v:.2}")
    } else if v < 99.95 {
        format!("{v:.1}")
    } else {
        format!("{v:.0}")
    };
    (s, units[i])
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 30) as f64
}

// ------------------------------------------------------------------- warna

pub const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

fn mix(a: (u8, u8, u8), b: (u8, u8, u8), pct_b: u32) -> COLORREF {
    let m = |x: u8, y: u8| ((x as u32 * (100 - pct_b) + y as u32 * pct_b) / 100) as u8;
    rgb(m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

pub const BG: (u8, u8, u8) = (27, 29, 35);
const GRAPH_BG: (u8, u8, u8) = (21, 23, 28);
pub const TRACK: (u8, u8, u8) = (46, 50, 62);
pub const DOWN: (u8, u8, u8) = (61, 220, 151);
pub const UP: (u8, u8, u8) = (255, 180, 84);
const CPU: (u8, u8, u8) = (90, 169, 255);
const RAM: (u8, u8, u8) = (199, 146, 234);
const HOT: (u8, u8, u8) = (255, 107, 107);
pub const TEXT: COLORREF = rgb(230, 232, 238);
pub const DIM: COLORREF = rgb(138, 144, 160);
const BORDER: COLORREF = rgb(52, 56, 68);

fn c(t: (u8, u8, u8)) -> COLORREF {
    rgb(t.0, t.1, t.2)
}

// ---------------------------------------------------------------- tata letak

/// Ukuran logis (96-DPI, skala 100 %) kartu.
pub const BASE_W: i32 = 240;
pub const BASE_H: i32 = 143;
const CORNER: i32 = 10;
/// Sisi (logis) area pegangan perubahan ukuran di sudut kanan-bawah.
const GRIP: i32 = 16;

struct Layout {
    w: i32,
    h: i32,
    down_line: RECT,
    up_line: RECT,
    graph: RECT,
    cpu_row: RECT,
    cpu_graph: RECT,
    ram_row: RECT,
    ram_graph: RECT,
    detail: RECT,
    usage_sep: RECT,
    usage_row: RECT,
    s: Scale,
}

#[derive(Clone, Copy)]
struct Scale(u32);

impl Scale {
    fn px(self, v: i32) -> i32 {
        (v * self.0 as i32 + 48) / 96
    }

    fn rect(self, l: i32, t: i32, r: i32, b: i32) -> RECT {
        RECT { left: self.px(l), top: self.px(t), right: self.px(r), bottom: self.px(b) }
    }
}

/// DPI efektif = DPI layar x skala pengguna (persen).
fn effective_dpi(dpi: u32, pct: u32) -> u32 {
    (dpi * pct / 100).max(48)
}

/// Ukuran piksel kartu pada DPI dan skala tertentu (tanpa membuat objek GDI).
pub fn size_for(dpi: u32, pct: u32) -> (i32, i32) {
    let s = Scale(effective_dpi(dpi, pct));
    (s.px(BASE_W), s.px(BASE_H))
}

impl Layout {
    fn new(eff_dpi: u32) -> Self {
        let s = Scale(eff_dpi);
        Layout {
            w: s.px(BASE_W),
            h: s.px(BASE_H),
            down_line: s.rect(10, 10, 122, 32),
            up_line: s.rect(10, 32, 122, 54),
            graph: s.rect(128, 10, 230, 54),
            cpu_row: s.rect(10, 62, 230, 82),
            cpu_graph: s.rect(154, 63, 230, 81),
            ram_row: s.rect(10, 84, 230, 104),
            ram_graph: s.rect(154, 85, 230, 103),
            detail: s.rect(10, 104, 230, 116),
            usage_sep: RECT {
                left: s.px(10),
                top: s.px(119),
                right: s.px(230),
                bottom: s.px(119) + s.px(1).max(1),
            },
            usage_row: s.rect(10, 123, 230, 139),
            s,
        }
    }
}

/// Sub-rect dalam satu baris, koordinat logis horizontal `l..r` relatif ke kiri kartu.
fn span(row: RECT, s: Scale, l: i32, r: i32) -> RECT {
    RECT { left: s.px(l), top: row.top, right: s.px(r), bottom: row.bottom }
}

// ------------------------------------------------------------------ renderer

pub struct Renderer {
    layout: Layout,
    dc: HDC,
    bmp: HBITMAP,
    old_bmp: HGDIOBJ,
    font_big: HFONT,
    font_text: HFONT,
    font_small: HFONT,
    br_bg: HBRUSH,
    br_track: HBRUSH,
    br_graph: HBRUSH,
    br_down: HBRUSH,
    br_down_fill: HBRUSH,
    br_up: HBRUSH,
    br_cpu: HBRUSH,
    br_ram: HBRUSH,
    br_hot: HBRUSH,
    br_cpu_fill: HBRUSH,
    br_ram_fill: HBRUSH,
    pen_down: HPEN,
    pen_up: HPEN,
    pen_cpu: HPEN,
    pen_ram: HPEN,
    pen_border: HPEN,
    pen_grip: HPEN,
    pts: Vec<POINT>,
}

impl Renderer {
    pub fn new(dpi: u32, pct: u32) -> Self {
        let mut r = Renderer {
            layout: Layout::new(effective_dpi(dpi, pct)),
            dc: null_mut(),
            bmp: null_mut(),
            old_bmp: null_mut(),
            font_big: null_mut(),
            font_text: null_mut(),
            font_small: null_mut(),
            br_bg: null_mut(),
            br_track: null_mut(),
            br_graph: null_mut(),
            br_down: null_mut(),
            br_down_fill: null_mut(),
            br_up: null_mut(),
            br_cpu: null_mut(),
            br_ram: null_mut(),
            br_hot: null_mut(),
            br_cpu_fill: null_mut(),
            br_ram_fill: null_mut(),
            pen_down: null_mut(),
            pen_up: null_mut(),
            pen_cpu: null_mut(),
            pen_ram: null_mut(),
            pen_border: null_mut(),
            pen_grip: null_mut(),
            pts: Vec::with_capacity(HISTORY_LEN + 2),
        };
        r.rebuild(dpi, pct);
        r
    }

    /// Ukuran kartu dalam piksel pada DPI/skala saat ini.
    pub fn size(&self) -> (i32, i32) {
        (self.layout.w, self.layout.h)
    }

    pub fn corner_radius(&self) -> i32 {
        self.layout.s.px(CORNER)
    }

    /// `true` bila (x, y), relatif ke pojok kiri-atas kartu, ada di area pegangan ukuran.
    pub fn grip_hit(&self, x: i32, y: i32) -> bool {
        let g = self.layout.s.px(GRIP);
        x >= self.layout.w - g && y >= self.layout.h - g && x < self.layout.w && y < self.layout.h
    }

    /// Membuat ulang semua objek GDI untuk DPI dan skala baru.
    pub fn rebuild(&mut self, dpi: u32, pct: u32) {
        self.destroy();
        self.layout = Layout::new(effective_dpi(dpi, pct));
        let s = self.layout.s;
        let font = |h: i32, weight: i32| unsafe {
            let face = wide("Segoe UI");
            CreateFontW(
                -s.px(h),
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
        self.font_big = font(16, 600);
        self.font_text = font(13, 400);
        self.font_small = font(11, 400);
        unsafe {
            self.br_bg = CreateSolidBrush(c(BG));
            self.br_track = CreateSolidBrush(c(TRACK));
            self.br_graph = CreateSolidBrush(c(GRAPH_BG));
            self.br_down = CreateSolidBrush(c(DOWN));
            self.br_down_fill = CreateSolidBrush(mix(GRAPH_BG, DOWN, 28));
            self.br_up = CreateSolidBrush(c(UP));
            self.br_cpu = CreateSolidBrush(c(CPU));
            self.br_ram = CreateSolidBrush(c(RAM));
            self.br_hot = CreateSolidBrush(c(HOT));
            self.br_cpu_fill = CreateSolidBrush(mix(GRAPH_BG, CPU, 28));
            self.br_ram_fill = CreateSolidBrush(mix(GRAPH_BG, RAM, 28));
            let width = s.px(1).max(1);
            self.pen_down = CreatePen(PS_SOLID, width, c(DOWN));
            self.pen_up = CreatePen(PS_SOLID, width, c(UP));
            self.pen_cpu = CreatePen(PS_SOLID, width, c(CPU));
            self.pen_ram = CreatePen(PS_SOLID, width, c(RAM));
            self.pen_border = CreatePen(PS_SOLID, width, BORDER);
            self.pen_grip = CreatePen(PS_SOLID, width, mix((138, 144, 160), BG, 35));

            let screen = GetDC(null_mut());
            self.dc = CreateCompatibleDC(screen);
            self.bmp = CreateCompatibleBitmap(screen, self.layout.w, self.layout.h);
            ReleaseDC(null_mut(), screen);
            self.old_bmp = SelectObject(self.dc, self.bmp);
            SetBkMode(self.dc, TRANSPARENT as i32);
        }
    }

    fn destroy(&mut self) {
        unsafe {
            if !self.dc.is_null() {
                SelectObject(self.dc, self.old_bmp);
                DeleteDC(self.dc);
                self.dc = null_mut();
            }
            for h in [
                &mut self.bmp,
                &mut self.font_big,
                &mut self.font_text,
                &mut self.font_small,
                &mut self.br_bg,
                &mut self.br_track,
                &mut self.br_graph,
                &mut self.br_down,
                &mut self.br_down_fill,
                &mut self.br_up,
                &mut self.br_cpu,
                &mut self.br_ram,
                &mut self.br_hot,
                &mut self.br_cpu_fill,
                &mut self.br_ram_fill,
                &mut self.pen_down,
                &mut self.pen_up,
                &mut self.pen_cpu,
                &mut self.pen_ram,
                &mut self.pen_border,
                &mut self.pen_grip,
            ] {
                if !h.is_null() {
                    DeleteObject(*h);
                    *h = null_mut();
                }
            }
        }
    }

    /// Menggambar kartu ke back-buffer lalu menyalinnya ke `target`.
    /// `today` = (byte masuk, byte keluar) hari ini.
    ///
    /// # Safety
    /// `target` harus DC yang valid (mis. dari `BeginPaint`) selama pemanggilan.
    pub unsafe fn paint(&mut self, target: HDC, s: &Sample, hist: &History, bits: bool, today: (u64, u64)) {
        let (w, h) = (self.layout.w, self.layout.h);
        let sc = self.layout.s;
        let full = RECT { left: 0, top: 0, right: w, bottom: h };
        unsafe {
            SelectObject(self.dc, GetStockObject(NULL_PEN));
            SelectObject(self.dc, GetStockObject(NULL_BRUSH));
            FillRect(self.dc, &full, self.br_bg);
            // garis tepi tipis mengikuti sudut membulat
            SelectObject(self.dc, self.pen_border);
            let rad = self.corner_radius() * 2;
            RoundRect(self.dc, 0, 0, w, h, rad, rad);
            SelectObject(self.dc, GetStockObject(NULL_PEN));
        }

        // jaringan: panah, angka (rata kanan), satuan (rata kiri)
        let (dl, ul) = (self.layout.down_line, self.layout.up_line);
        self.arrow(dl, sc.px(15), sc.px(5), sc.px(4), true, self.br_down);
        self.arrow(ul, sc.px(15), sc.px(5), sc.px(4), false, self.br_up);
        let (dv, du) = fmt_rate(s.down, bits);
        let (uv, uu) = fmt_rate(s.up, bits);
        self.text(self.font_big, TEXT, &dv, span(dl, sc, 26, 84), DT_RIGHT);
        self.text(self.font_text, DIM, du, span(dl, sc, 88, 122), DT_LEFT);
        self.text(self.font_big, TEXT, &uv, span(ul, sc, 26, 84), DT_RIGHT);
        self.text(self.font_text, DIM, uu, span(ul, sc, 88, 122), DT_LEFT);
        self.net_graph(hist);

        // CPU & RAM: bar + grafik riwayat 60 detik (skala tetap 0..100 %)
        let mem_pct = s.mem_percent();
        let (cpu_g, ram_g) = (self.layout.cpu_graph, self.layout.ram_graph);
        self.meter("CPU", s.cpu, self.br_cpu, self.layout.cpu_row);
        self.load_graph(cpu_g, hist.cpu(), self.br_cpu_fill, self.pen_cpu);
        self.meter("RAM", mem_pct, self.br_ram, self.layout.ram_row);
        self.load_graph(ram_g, hist.mem(), self.br_ram_fill, self.pen_ram);
        let detail = format!("{:.1} / {:.1} GB", gib(s.mem_used), gib(s.mem_total));
        self.text(self.font_small, DIM, &detail, self.layout.detail, DT_RIGHT);

        // pemakaian hari ini
        let sep = self.layout.usage_sep;
        unsafe {
            FillRect(self.dc, &sep, self.br_track);
        }
        let row = self.layout.usage_row;
        self.text(self.font_text, DIM, "Hari ini", span(row, sc, 10, 64), DT_LEFT);
        self.arrow(row, sc.px(73), sc.px(3), sc.px(3), true, self.br_down);
        self.text(self.font_text, TEXT, &fmt_bytes(today.0), span(row, sc, 80, 148), DT_LEFT);
        self.arrow(row, sc.px(156), sc.px(3), sc.px(3), false, self.br_up);
        self.text(self.font_text, TEXT, &fmt_bytes(today.1), span(row, sc, 163, 230), DT_LEFT);

        self.grip();

        // SAFETY: `target` valid menurut kontrak fungsi; `self.dc` milik kita.
        unsafe {
            BitBlt(target, 0, 0, w, h, self.dc, 0, 0, SRCCOPY);
        }
    }

    fn text(&self, font: HFONT, color: COLORREF, s: &str, mut r: RECT, align: u32) {
        let mut buf = [0u16; 40];
        let mut n = 0;
        for u in s.encode_utf16().take(buf.len()) {
            buf[n] = u;
            n += 1;
        }
        unsafe {
            SelectObject(self.dc, font);
            SetTextColor(self.dc, color);
            DrawTextW(self.dc, buf.as_ptr(), n as i32, &mut r, align | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
        }
    }

    /// Segitiga kecil sebagai ikon panah turun/naik (tanpa bergantung pada glyph font).
    fn arrow(&self, line: RECT, cx: i32, hw: i32, hh: i32, down: bool, brush: HBRUSH) {
        let cy = (line.top + line.bottom) / 2;
        let pts = if down {
            [POINT { x: cx - hw, y: cy - hh }, POINT { x: cx + hw, y: cy - hh }, POINT { x: cx, y: cy + hh }]
        } else {
            [POINT { x: cx - hw, y: cy + hh }, POINT { x: cx + hw, y: cy + hh }, POINT { x: cx, y: cy - hh }]
        };
        unsafe {
            SelectObject(self.dc, brush);
            Polygon(self.dc, pts.as_ptr(), 3);
            SelectObject(self.dc, GetStockObject(NULL_BRUSH));
        }
    }

    /// Tiga garis diagonal kecil di sudut kanan-bawah sebagai petunjuk bisa ditarik.
    fn grip(&self) {
        let s = self.layout.s;
        let (right, bottom) = (self.layout.w - s.px(6), self.layout.h - s.px(6));
        unsafe {
            SelectObject(self.dc, self.pen_grip);
            for i in 1..=3 {
                let d = s.px(3) * i;
                MoveToEx(self.dc, right - d, bottom, null_mut());
                LineTo(self.dc, right, bottom - d);
            }
            SelectObject(self.dc, GetStockObject(NULL_PEN));
        }
    }

    /// Menggambar satu deret riwayat di dalam `g` (nilai 0..=`max`, terbaru di kanan):
    /// garis `pen`, dan area di bawahnya bila `fill` ada. Latar digambar pemanggil.
    fn plot(&mut self, g: RECT, vals: impl Iterator<Item = u64>, max: u64, fill: Option<HBRUSH>, pen: HPEN) {
        let (gw, gh) = (g.right - g.left, g.bottom - g.top);
        let max = max.max(1);
        let n = HISTORY_LEN as i32;
        let y_of = |v: u64| g.bottom - 1 - ((v.min(max) as u128 * (gh - 2) as u128) / max as u128) as i32;
        let x_of = |i: i32| g.left + i * (gw - 1) / (n - 1);

        self.pts.clear();
        self.pts.extend(vals.enumerate().map(|(i, v)| POINT { x: x_of(i as i32), y: y_of(v) }));
        let line_count = self.pts.len() as i32;
        unsafe {
            if let Some(brush) = fill {
                self.pts.push(POINT { x: g.right - 1, y: g.bottom - 1 });
                self.pts.push(POINT { x: g.left, y: g.bottom - 1 });
                SelectObject(self.dc, brush);
                Polygon(self.dc, self.pts.as_ptr(), self.pts.len() as i32);
                SelectObject(self.dc, GetStockObject(NULL_BRUSH));
            }
            SelectObject(self.dc, pen);
            Polyline(self.dc, self.pts.as_ptr(), line_count);
            SelectObject(self.dc, GetStockObject(NULL_PEN));
        }
    }

    /// Grafik jaringan: unduh = area terisi + garis, unggah = garis; skala otomatis dengan batas bawah.
    fn net_graph(&mut self, hist: &History) {
        let g = self.layout.graph;
        let peak = hist.peak().max(GRAPH_FLOOR);
        unsafe {
            FillRect(self.dc, &g, self.br_graph);
        }
        self.plot(g, hist.down(), peak, Some(self.br_down_fill), self.pen_down);
        self.plot(g, hist.up(), peak, None, self.pen_up);
    }

    /// Grafik beban CPU/RAM (persen), skala tetap 0..100.
    fn load_graph(&mut self, g: RECT, vals: impl Iterator<Item = u64>, fill: HBRUSH, pen: HPEN) {
        unsafe {
            FillRect(self.dc, &g, self.br_graph);
        }
        self.plot(g, vals, 100, Some(fill), pen);
    }

    /// Satu baris: label, bar, angka persen. Grafik riwayatnya digambar di samping (`*_graph`).
    fn meter(&self, label: &str, pct: f32, brush: HBRUSH, row: RECT) {
        let s = self.layout.s;
        self.text(self.font_text, DIM, label, span(row, s, 10, 38), DT_LEFT);
        let mid = (row.top + row.bottom) / 2;
        let half = s.px(4);
        let bar = RECT { left: s.px(40), top: mid - half, right: s.px(108), bottom: mid + half };
        let pct = pct.clamp(0.0, 100.0);
        let fill_w = ((bar.right - bar.left) as f32 * pct / 100.0) as i32;
        let rad = half * 2;
        unsafe {
            SelectObject(self.dc, self.br_track);
            RoundRect(self.dc, bar.left, bar.top, bar.right + 1, bar.bottom + 1, rad, rad);
            if fill_w > 0 {
                SelectObject(self.dc, if pct >= HOT_PERCENT { self.br_hot } else { brush });
                RoundRect(self.dc, bar.left, bar.top, bar.left + fill_w.max(rad) + 1, bar.bottom + 1, rad, rad);
            }
            SelectObject(self.dc, GetStockObject(NULL_BRUSH));
        }
        self.text(self.font_text, TEXT, &format!("{pct:.0} %"), span(row, s, 110, 148), DT_RIGHT);
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_formatting_bytes() {
        assert_eq!(fmt_rate(0, false), ("0".into(), "B/s"));
        assert_eq!(fmt_rate(512, false), ("512".into(), "B/s"));
        assert_eq!(fmt_rate(1024, false), ("1.00".into(), "KB/s"));
        assert_eq!(fmt_rate(13_002_342, false), ("12.4".into(), "MB/s"));
        assert_eq!(fmt_rate(350 * 1024, false), ("350".into(), "KB/s"));
        // 1000 B/s sudah pindah satuan agar angka tidak lebih dari 3 digit
        assert_eq!(fmt_rate(1000, false), ("0.98".into(), "KB/s"));
        assert_eq!(fmt_rate(5 << 30, false), ("5.00".into(), "GB/s"));
    }

    #[test]
    fn rate_formatting_bits() {
        assert_eq!(fmt_rate(125_000, true), ("1.00".into(), "Mbps"));
        assert_eq!(fmt_rate(100, true), ("800".into(), "bps"));
        assert_eq!(fmt_rate(12_500_000, true), ("100".into(), "Mbps"));
    }

    #[test]
    fn rounding_never_shows_four_digits() {
        // 999.6 KB/s dibulatkan ke 1000 -> harus naik ke MB/s
        let (v, u) = fmt_rate((999.6 * 1024.0) as u64, false);
        assert_eq!((v.as_str(), u), ("0.98", "MB/s"));
        assert_eq!(fmt_rate((99.97 * 1024.0) as u64, false), ("100".into(), "KB/s"));
    }

    #[test]
    fn history_is_ordered_oldest_first_and_wraps() {
        let mut h = History::new();
        for i in 1..=(HISTORY_LEN as u64 + 5) {
            h.push(i, i * 2, i as f32, 150.0);
        }
        let d: Vec<u64> = h.down().collect();
        assert_eq!(d.len(), HISTORY_LEN);
        assert_eq!(d[0], 6);
        assert_eq!(*d.last().unwrap(), HISTORY_LEN as u64 + 5);
        assert_eq!(h.peak(), (HISTORY_LEN as u64 + 5) * 2);
    }

    #[test]
    fn size_scales_with_dpi_and_user_percent() {
        assert_eq!(size_for(96, 100), (BASE_W, BASE_H));
        assert_eq!(size_for(192, 100), (BASE_W * 2, BASE_H * 2));
        assert_eq!(size_for(96, 200), (BASE_W * 2, BASE_H * 2));
        assert_eq!(size_for(144, 50), (BASE_W * 3 / 4, (BASE_H * 3 + 2) / 4));
        let (w, h) = (Layout::new(96).w, Layout::new(96).h);
        assert_eq!((w, h), (BASE_W, BASE_H));
    }
}
