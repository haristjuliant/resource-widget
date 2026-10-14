//! Cetak metrik tiap detik untuk verifikasi tanpa GUI.
//! Pemakaian: `cargo run --example probe -- [detik] [nama-adapter]`
//! Proses teratas dipindai tiap 3 detik (seperti widget).

use std::time::{Duration, Instant};

use resource_widget::metrics::{AdapterSel, Sampler, list_adapters};
use resource_widget::procs::{ProcMonitor, Top};

fn show(t: &Option<Top>, f: impl Fn(u64) -> String) -> String {
    t.as_ref().map_or("-".into(), |t| format!("{} {}", t.name, f(t.value)))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let secs: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    let sel = args.next().map_or(AdapterSel::Auto, AdapterSel::Named);

    println!("adapter terdeteksi: {:?}", list_adapters());
    println!("pilihan: {sel:?}");
    let mut s = Sampler::new(sel);
    let mut procs = ProcMonitor::new();
    procs.scan(0);
    for i in 1..=secs {
        std::thread::sleep(Duration::from_secs(1));
        let v = s.sample();
        println!(
            "down {:>10} B/s  up {:>9} B/s  disk {:>10} B/s ({:>3.0}% aktif)  cpu {:>5.1}%  ram {:>5.1}%  ({:.2}/{:.2} GiB)",
            v.down,
            v.up,
            v.disk,
            v.disk_busy,
            v.cpu,
            v.mem_percent(),
            v.mem_used as f64 / (1u64 << 30) as f64,
            v.mem_total as f64 / (1u64 << 30) as f64,
        );
        if i % 3 == 0 {
            let t0 = Instant::now();
            let top = procs.scan(v.down + v.up).clone();
            let took = t0.elapsed();
            println!(
                "    top  cpu: {} | ram: {} | disk: {} | net: {}   (scan {:.2} ms)",
                show(&top.cpu, |v| format!("{:.1}%", v as f64 / 10.0)),
                show(&top.mem, |v| format!("{:.0} MB", v as f64 / 1048576.0)),
                show(&top.disk, |v| format!("{:.2} MB/s", v as f64 / 1048576.0)),
                show(&top.net, |v| format!("{:.1} KB/s", v as f64 / 1024.0)),
                took.as_secs_f64() * 1000.0,
            );
        }
    }
}
