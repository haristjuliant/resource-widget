//! Cetak metrik tiap detik untuk verifikasi tanpa GUI.
//! Pemakaian: `cargo run --example probe -- [detik] [nama-adapter]`

use std::time::Duration;

use resource_widget::metrics::{AdapterSel, Sampler, list_adapters};

fn main() {
    let mut args = std::env::args().skip(1);
    let secs: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    let sel = args.next().map_or(AdapterSel::Auto, AdapterSel::Named);

    println!("adapter terdeteksi: {:?}", list_adapters());
    println!("pilihan: {sel:?}");
    let mut s = Sampler::new(sel);
    for _ in 0..secs {
        std::thread::sleep(Duration::from_secs(1));
        let v = s.sample();
        println!(
            "down {:>12} B/s  up {:>12} B/s  cpu {:>5.1}%  ram {:>5.1}%  ({:.2}/{:.2} GiB)",
            v.down,
            v.up,
            v.cpu,
            v.mem_percent(),
            v.mem_used as f64 / (1u64 << 30) as f64,
            v.mem_total as f64 / (1u64 << 30) as f64,
        );
    }
}
