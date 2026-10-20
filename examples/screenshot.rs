//! Membuat gambar kartu dengan data demo sintetis (tanpa data dari mesin ini) untuk README.
//! Pemakaian: `cargo run --release --example screenshot -- <out.bmp> [skala-persen] [full|noprocs|compact]`

use std::ptr::null_mut;

use resource_widget::metrics::Sample;
use resource_widget::procs::{ProcTop, Top};
use resource_widget::render::{History, Renderer, View};

use windows_sys::Win32::Graphics::Gdi::*;

const GIB: u64 = 1 << 30;

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().expect("pemakaian: screenshot <out.bmp> [skala-persen]");
    let pct: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(200);
    let view = match args.next().as_deref() {
        Some("noprocs") => View::from_flags(false, false, true),
        Some("compact") => View::from_flags(true, true, true),
        _ => View::FULL,
    };

    // Riwayat 60 titik yang deterministik (LCG sederhana, tanpa data nyata).
    let mut seed = 12345u32;
    let mut rnd = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / 16_777_216.0
    };
    let total = 16 * GIB;
    let mut hist = History::new();
    let mut last = Sample::default();
    for i in 0..60u32 {
        let t = i as f32;
        let burst = (22..30).contains(&i);
        let busy = if (15..35).contains(&i) { 30.0 + 40.0 * rnd() } else { 3.0 + 10.0 * rnd() };
        let mem_pct = 58.0 + 0.05 * t + rnd();
        last = Sample {
            down: ((0.8 + 0.6 * (t * 0.25).sin().abs() + if burst { 3.0 } else { 0.0 } + 0.4 * rnd()) * 1_048_576.0) as u64,
            up: ((0.05 + 0.04 * rnd() + if burst { 0.2 } else { 0.0 }) * 1_048_576.0) as u64,
            cpu: 14.0 + 10.0 * (t * 0.2).sin() + 8.0 * rnd() + if (40..46).contains(&i) { 30.0 } else { 0.0 },
            mem_used: (total as f64 * mem_pct as f64 / 100.0) as u64,
            mem_total: total,
            disk: (busy as f64 / 100.0 * 150e6) as u64,
            disk_busy: busy,
            ssd_temp: Some(50.0 + 4.0 * (t * 0.1).sin() + rnd()),
            cpu_temp: Some(55.0 + 8.0 * rnd()),
            ..Sample::default()
        };
        hist.push(&last);
    }
    // Nilai terakhir (yang tampil sebagai angka) dibuat bulat dan mudah dibaca.
    last.down = 2_516_582; // 2.4 MB/s
    last.up = 184_320; // 180 KB/s
    last.cpu = 23.0;
    last.mem_used = (total as f64 * 0.61) as u64;
    last.disk = 8_598_323; // 8.2 MB/s
    last.disk_busy = 43.0;
    last.ssd_temp = Some(58.0);
    last.cpu_temp = Some(62.0);
    hist.push(&last);

    let demo = |value: u64| Some(Top { name: "demo".into(), value });
    let procs = ProcTop {
        cpu: demo(120),             // 12 %
        mem: demo(GIB + GIB / 5),   // 1.2 GB
        disk: demo(3 * 1_048_576),  // 3 MB/s
        net: demo(1_153_434),       // 1.1 MB/s
    };
    let today = (GIB + GIB / 3, 32 * 1_048_576 + 524_288);

    let mut r = Renderer::new(96, pct, view);
    let (w, h) = r.size();
    // SAFETY: DC/bitmap milik kita; paint() hanya menggambar ke DC yang valid ini.
    let pixels = unsafe {
        let screen = GetDC(null_mut());
        let dc = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(dc, bmp);
        r.paint(dc, &last, &hist, false, today, Some(&procs));
        SelectObject(dc, old); // GetDIBits butuh bitmap tidak terpilih di DC

        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h; // top-down
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        GetDIBits(dc, bmp, 0, h as u32, buf.as_mut_ptr().cast(), &mut bi, DIB_RGB_COLORS);
        DeleteObject(bmp);
        DeleteDC(dc);
        ReleaseDC(null_mut(), screen);
        buf
    };

    // BMP 32-bit bottom-up: balik urutan baris.
    let row = (w * 4) as usize;
    let mut file = Vec::with_capacity(54 + pixels.len());
    file.extend_from_slice(b"BM");
    file.extend_from_slice(&((54 + pixels.len()) as u32).to_le_bytes());
    file.extend_from_slice(&[0; 4]);
    file.extend_from_slice(&54u32.to_le_bytes());
    file.extend_from_slice(&40u32.to_le_bytes());
    file.extend_from_slice(&w.to_le_bytes());
    file.extend_from_slice(&h.to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&32u16.to_le_bytes());
    file.extend_from_slice(&[0; 24]);
    for y in (0..h as usize).rev() {
        file.extend_from_slice(&pixels[y * row..(y + 1) * row]);
    }
    std::fs::write(&out, file).expect("gagal menulis file keluaran");
    println!("{out}: {w}x{h} (skala {pct} %, {view:?})");
}
