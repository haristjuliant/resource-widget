//! Statistik pemakaian data per hari dan per bulan.
//!
//! Disimpan sebagai CSV `YYYYMMDD,rx,tx` di `%APPDATA%\ResourceWidget\usage.csv`.
//! Hanya trafik yang terjadi selama widget berjalan yang tercatat.

use std::path::PathBuf;

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

use crate::settings::data_dir;

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "Mei", "Jun", "Jul", "Agu", "Sep", "Okt", "Nov", "Des"];
const MAX_MONTH_LINES: usize = 24;
const MAX_DAY_LINES: usize = 90;

/// Total satu hari; `key` = YYYYMMDD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Day {
    pub key: u32,
    pub rx: u64,
    pub tx: u64,
}

#[derive(Default)]
pub struct Usage {
    /// Terurut naik menurut `key`, tanpa duplikat.
    days: Vec<Day>,
    dirty: bool,
}

/// Kunci tanggal lokal hari ini (YYYYMMDD).
pub fn today_key() -> u32 {
    // SAFETY: GetLocalTime hanya mengisi struct yang kita berikan.
    let t: SYSTEMTIME = unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    };
    t.wYear as u32 * 10_000 + t.wMonth as u32 * 100 + t.wDay as u32
}

impl Usage {
    /// Menambah byte ke hari `key` (membuat barisnya bila belum ada).
    pub fn add(&mut self, key: u32, rx: u64, tx: u64) {
        if rx == 0 && tx == 0 {
            return;
        }
        match self.days.binary_search_by_key(&key, |d| d.key) {
            Ok(i) => {
                self.days[i].rx += rx;
                self.days[i].tx += tx;
            }
            Err(i) => self.days.insert(i, Day { key, rx, tx }),
        }
        self.dirty = true;
    }

    pub fn day(&self, key: u32) -> (u64, u64) {
        self.days
            .binary_search_by_key(&key, |d| d.key)
            .map_or((0, 0), |i| (self.days[i].rx, self.days[i].tx))
    }

    /// Total satu bulan; `ym` = YYYYMM.
    pub fn month(&self, ym: u32) -> (u64, u64) {
        self.days
            .iter()
            .filter(|d| d.key / 100 == ym)
            .fold((0, 0), |(rx, tx), d| (rx + d.rx, tx + d.tx))
    }

    pub fn total(&self) -> (u64, u64) {
        self.days.iter().fold((0, 0), |(rx, tx), d| (rx + d.rx, tx + d.tx))
    }

    pub fn first_key(&self) -> Option<u32> {
        self.days.first().map(|d| d.key)
    }

    /// Total per bulan, bulan terbaru dulu.
    pub fn months_desc(&self) -> Vec<(u32, u64, u64)> {
        let mut out: Vec<(u32, u64, u64)> = Vec::new();
        for d in &self.days {
            let ym = d.key / 100;
            match out.last_mut() {
                Some(m) if m.0 == ym => {
                    m.1 += d.rx;
                    m.2 += d.tx;
                }
                _ => out.push((ym, d.rx, d.tx)),
            }
        }
        out.reverse();
        out
    }

    pub fn parse(text: &str) -> Self {
        let mut u = Usage::default();
        for line in text.lines() {
            let mut it = line.split(',').map(str::trim);
            let (Some(k), Some(rx), Some(tx)) = (it.next(), it.next(), it.next()) else { continue };
            let (Ok(k), Ok(rx), Ok(tx)) = (k.parse::<u32>(), rx.parse::<u64>(), tx.parse::<u64>()) else {
                continue;
            };
            if is_valid_key(k) {
                u.add(k, rx, tx); // menggabungkan duplikat dan mengurutkan
            }
        }
        u.dirty = false;
        u
    }

    pub fn serialize(&self) -> String {
        let mut out = String::with_capacity(self.days.len() * 32);
        for d in &self.days {
            out += &format!("{},{},{}\n", d.key, d.rx, d.tx);
        }
        out
    }

    fn path() -> Option<PathBuf> {
        Some(data_dir()?.join("usage.csv"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| Self::parse(&t))
            .unwrap_or_default()
    }

    /// Menulis ke disk bila ada perubahan (tulis file sementara lalu rename agar atomik).
    pub fn save_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(p) = Self::path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = p.with_extension("csv.tmp");
        if std::fs::write(&tmp, self.serialize()).is_ok() && std::fs::rename(&tmp, &p).is_ok() {
            self.dirty = false;
        }
    }
}

fn is_valid_key(k: u32) -> bool {
    let (y, m, d) = (k / 10_000, k / 100 % 100, k % 100);
    (2000..=9999).contains(&y) && (1..=12).contains(&m) && (1..=31).contains(&d)
}

// ------------------------------------------------------------------ format

/// Byte → teks dengan skala otomatis (basis 1024), maksimal 3 angka penting.
pub fn fmt_bytes(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 999.5 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} B")
    } else if v < 9.995 {
        format!("{v:.2} {}", units[i])
    } else if v < 99.95 {
        format!("{v:.1} {}", units[i])
    } else {
        format!("{v:.0} {}", units[i])
    }
}

pub fn month_label(ym: u32) -> String {
    let m = (ym % 100).clamp(1, 12) as usize;
    format!("{} {}", MONTHS[m - 1], ym / 100)
}

pub fn date_label(key: u32) -> String {
    let m = (key / 100 % 100).clamp(1, 12) as usize;
    format!("{:02} {} {}", key % 100, MONTHS[m - 1], key / 10_000)
}

// ------------------------------------------------- baris tabel (jendela statistik)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Judul bagian.
    Section,
    /// Judul kolom.
    Header,
    Row,
}

/// Satu baris tabel: kolom 0 = label, 1 = unduh, 2 = unggah, 3 = total.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub kind: Kind,
    pub cols: [String; 4],
}

fn section(title: &str) -> Line {
    Line { kind: Kind::Section, cols: [title.into(), String::new(), String::new(), String::new()] }
}

fn header(first: &str) -> Line {
    Line {
        kind: Kind::Header,
        cols: [first.into(), "Unduh".into(), "Unggah".into(), "Total".into()],
    }
}

fn row(label: String, rx: u64, tx: u64) -> Line {
    Line {
        kind: Kind::Row,
        cols: [label, fmt_bytes(rx), fmt_bytes(tx), fmt_bytes(rx.saturating_add(tx))],
    }
}

/// Ringkasan + tabel bulanan + tabel harian, terbaru di atas.
pub fn build_lines(u: &Usage, today: u32) -> Vec<Line> {
    let mut v = vec![section("Ringkasan"), header("")];
    let (rx, tx) = u.day(today);
    v.push(row("Hari ini".into(), rx, tx));
    let (rx, tx) = u.month(today / 100);
    v.push(row("Bulan ini".into(), rx, tx));
    let (rx, tx) = u.total();
    let since = u.first_key().map_or(String::new(), |k| format!(" (sejak {})", date_label(k)));
    v.push(row(format!("Total tercatat{since}"), rx, tx));

    v.push(section("Bulanan"));
    v.push(header("Bulan"));
    v.extend(u.months_desc().into_iter().take(MAX_MONTH_LINES).map(|(ym, rx, tx)| row(month_label(ym), rx, tx)));

    v.push(section("Harian"));
    v.push(header("Tanggal"));
    v.extend(u.days.iter().rev().take(MAX_DAY_LINES).map(|d| row(date_label(d.key), d.rx, d.tx)));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Usage {
        let mut u = Usage::default();
        u.add(20261013, 100, 10);
        u.add(20261014, 200, 20);
        u.add(20260930, 1000, 100);
        u.add(20261014, 50, 5); // hari yang sama: digabung
        u
    }

    #[test]
    fn add_merges_same_day_and_keeps_order() {
        let u = sample();
        assert_eq!(u.day(20261014), (250, 25));
        assert_eq!(u.day(20250101), (0, 0));
        let keys: Vec<u32> = u.days.iter().map(|d| d.key).collect();
        assert_eq!(keys, vec![20260930, 20261013, 20261014]);
    }

    #[test]
    fn zero_bytes_do_not_create_rows_or_dirty() {
        let mut u = Usage::default();
        u.add(20261014, 0, 0);
        assert!(u.days.is_empty() && !u.dirty);
    }

    #[test]
    fn month_and_total_aggregation() {
        let u = sample();
        assert_eq!(u.month(202610), (350, 35));
        assert_eq!(u.month(202609), (1000, 100));
        assert_eq!(u.total(), (1350, 135));
        assert_eq!(u.months_desc(), vec![(202610, 350, 35), (202609, 1000, 100)]);
    }

    #[test]
    fn csv_roundtrip_and_tolerant_parse() {
        let u = sample();
        let back = Usage::parse(&u.serialize());
        assert_eq!(back.days, u.days);
        assert!(!back.dirty);

        let messy = "bad\n20261014,5,6\n20261014,1,1\n,,\n99999999,1,1\n20260230x,1,1\n20261015,7";
        let p = Usage::parse(messy);
        assert_eq!(p.days, vec![Day { key: 20261014, rx: 6, tx: 7 }]);
    }

    #[test]
    fn byte_formatting() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1024), "1.00 KB");
        assert_eq!(fmt_bytes(340 * 1024 * 1024), "340 MB");
        assert_eq!(fmt_bytes(1_288_490_189), "1.20 GB");
        assert_eq!(fmt_bytes(1000), "0.98 KB");
        // pembulatan tidak boleh menghasilkan 4 digit
        assert_eq!(fmt_bytes((999.6 * 1024.0) as u64), "0.98 MB");
    }

    #[test]
    fn labels() {
        assert_eq!(date_label(20261014), "14 Okt 2026");
        assert_eq!(month_label(202605), "Mei 2026");
    }

    #[test]
    fn lines_have_summary_months_and_days() {
        let u = sample();
        let l = build_lines(&u, 20261014);
        assert_eq!(l[2].cols[0], "Hari ini");
        assert_eq!(l[2].cols[1], "250 B");
        assert_eq!(l[3].cols[0], "Bulan ini");
        assert!(l[4].cols[0].starts_with("Total tercatat (sejak 30 Sep 2026)"));
        let months: Vec<&str> = l.iter().filter(|x| x.kind == Kind::Row).map(|x| x.cols[0].as_str()).collect();
        assert!(months.contains(&"Okt 2026") && months.contains(&"Sep 2026") && months.contains(&"13 Okt 2026"));
        // hari terbaru paling atas pada tabel harian
        let first_day = l.iter().rposition(|x| x.kind == Kind::Header).unwrap() + 1;
        assert_eq!(l[first_day].cols[0], "14 Okt 2026");
    }

    #[test]
    fn today_key_is_plausible() {
        assert!(is_valid_key(today_key()));
    }
}
