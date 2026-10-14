//! Menggambar ikon aplikasi secara terprogram (tanpa aset luar) dan menulis `assets/icon.ico`
//! (beberapa ukuran, frame PNG) serta `docs/icon.png`.
//! Pemakaian: `cargo run --release --example make_icon`
//!
//! Desain (ruang 64x64): ubin gelap membulat, grafik jaringan hijau di atas, dan tiga bar
//! CPU / RAM / disk di bawah, memakai warna yang sama dengan kartu widget. Pada ukuran kecil
//! (<= 32 px) detailnya disederhanakan agar tetap terbaca.

use std::io::Write;

const SIZES: [usize; 7] = [16, 24, 32, 48, 64, 128, 256];
/// Supersampling per sumbu untuk anti-alias.
const SS: usize = 4;

type Px = [f32; 4]; // r, g, b (0..255), a (0..1); premultiplied

const BG: [f32; 3] = [30.0, 33.0, 40.0];
const BORDER: [f32; 3] = [58.0, 63.0, 78.0];
const TRACK: [f32; 3] = [52.0, 57.0, 72.0];
const GREEN: [f32; 3] = [61.0, 220.0, 151.0];
const BLUE: [f32; 3] = [90.0, 169.0, 255.0];
const PURPLE: [f32; 3] = [199.0, 146.0, 234.0];
const TEAL: [f32; 3] = [64.0, 201.0, 214.0];

fn over(dst: &mut Px, c: [f32; 3], a: f32) {
    for i in 0..3 {
        dst[i] = c[i] * a + dst[i] * (1.0 - a);
    }
    dst[3] = a + dst[3] * (1.0 - a);
}

fn in_rrect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    let r = r.min((x1 - x0) / 2.0).min((y1 - y0) / 2.0);
    let (cx, cy) = (x.clamp(x0 + r, x1 - r), y.clamp(y0 + r, y1 - r));
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

fn seg_dist(px: f32, py: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((px - a.0) * dx + (py - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((px - (a.0 + t * dx)).powi(2) + (py - (a.1 + t * dy)).powi(2)).sqrt()
}

fn in_poly(x: f32, y: f32, pts: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let mut j = pts.len() - 1;
    for i in 0..pts.len() {
        let (a, b) = (pts[i], pts[j]);
        if (a.1 > y) != (b.1 > y) && x < (b.0 - a.0) * (y - a.1) / (b.1 - a.1) + a.0 {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Warna satu titik (x, y dalam ruang 64x64).
fn shade(x: f32, y: f32, small: bool) -> Px {
    let mut p: Px = [0.0; 4];
    // ubin dengan garis tepi tipis
    if in_rrect(x, y, 0.0, 0.0, 64.0, 64.0, 14.0) {
        over(&mut p, BORDER, 1.0);
        if in_rrect(x, y, 1.6, 1.6, 62.4, 62.4, 12.6) {
            over(&mut p, BG, 1.0);
        }
    } else {
        return p;
    }

    // grafik jaringan: area terisi + garis
    let line: &[(f32, f32)] = if small {
        &[(9.0, 28.0), (20.0, 22.0), (29.0, 27.0), (40.0, 12.0), (55.0, 20.0)]
    } else {
        &[(10.0, 30.0), (19.0, 25.0), (27.0, 28.0), (37.0, 14.0), (45.0, 22.0), (54.0, 12.0)]
    };
    let base = if small { 33.0 } else { 34.0 };
    let mut poly: Vec<(f32, f32)> = line.to_vec();
    poly.push((line[line.len() - 1].0, base));
    poly.push((line[0].0, base));
    if in_poly(x, y, &poly) {
        over(&mut p, GREEN, 0.28);
    }
    let w = if small { 5.0 } else { 3.2 };
    let near = line.windows(2).any(|s| seg_dist(x, y, s[0], s[1]) <= w / 2.0);
    if near {
        over(&mut p, GREEN, 1.0);
    }

    // bar metrik: (y, tinggi, panjang, warna)
    let bars: &[(f32, f32, f32, [f32; 3])] = if small {
        &[(38.0, 9.0, 30.0, BLUE), (51.0, 9.0, 40.0, PURPLE)]
    } else {
        &[(40.0, 5.0, 30.0, BLUE), (47.0, 5.0, 40.0, PURPLE), (54.0, 5.0, 18.0, TEAL)]
    };
    let (x0, x1) = (10.0, 54.0);
    for &(y0, h, len, c) in bars {
        let r = h / 2.0;
        if in_rrect(x, y, x0, y0, x1, y0 + h, r) {
            over(&mut p, TRACK, 1.0);
        }
        if in_rrect(x, y, x0, y0, x0 + len, y0 + h, r) {
            over(&mut p, c, 1.0);
        }
    }
    p
}

/// RGBA8 (tidak-premultiplied) untuk ukuran `n` px.
fn render(n: usize) -> Vec<u8> {
    let small = n <= 32;
    let mut out = Vec::with_capacity(n * n * 4);
    for py in 0..n {
        for px in 0..n {
            let mut acc: Px = [0.0; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (px as f32 + (sx as f32 + 0.5) / SS as f32) * 64.0 / n as f32;
                    let v = (py as f32 + (sy as f32 + 0.5) / SS as f32) * 64.0 / n as f32;
                    let s = shade(u, v, small);
                    for i in 0..4 {
                        acc[i] += s[i];
                    }
                }
            }
            let k = (SS * SS) as f32;
            let a = acc[3] / k;
            for ch in &acc[..3] {
                let c = if a > 0.0 { ch / k / a } else { 0.0 };
                out.push(c.round().clamp(0.0, 255.0) as u8);
            }
            out.push((a * 255.0).round().clamp(0.0, 255.0) as u8);
        }
    }
    out
}

fn encode_png(n: usize, rgba: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, n as u32, n as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::High);
        let mut w = enc.write_header().expect("header PNG");
        w.write_image_data(rgba).expect("data PNG");
    }
    buf
}

fn main() {
    let frames: Vec<(usize, Vec<u8>)> = SIZES.iter().map(|&n| (n, encode_png(n, &render(n)))).collect();

    // Wadah ICO: ICONDIR + ICONDIRENTRY per frame + data PNG.
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(frames.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * frames.len();
    for (n, data) in &frames {
        let dim = if *n >= 256 { 0u8 } else { *n as u8 };
        ico.extend_from_slice(&[dim, dim, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes()); // planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bpp
        ico.extend_from_slice(&(data.len() as u32).to_le_bytes());
        ico.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += data.len();
    }
    for (_, data) in &frames {
        ico.extend_from_slice(data);
    }

    std::fs::create_dir_all("assets").unwrap();
    std::fs::File::create("assets/icon.ico").unwrap().write_all(&ico).unwrap();
    std::fs::write("docs/icon.png", &frames.last().unwrap().1).unwrap();
    println!("assets/icon.ico: {} byte, {} frame ({:?})", ico.len(), frames.len(), SIZES);
}
