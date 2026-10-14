//! Metrik sistem (jaringan, CPU, RAM) langsung dari API Win32. Tanpa dependensi UI.

use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::GetSystemTimes;

/// Jeda antar-sampel lebih dari ini dianggap sleep/resume: kecepatan dibuang (byte tetap dihitung).
/// Harus lebih besar dari selang update terpanjang (5 dtk).
const MAX_GAP: Duration = Duration::from_secs(15);

const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_OPER_STATUS_UP: i32 = 1;
// `windows-sys` tidak punya accessor bitfield; urutan bit mengikuti MIB_IF_ROW2.
const FLAG_HARDWARE: u8 = 1 << 0;
const FLAG_FILTER: u8 = 1 << 1;

/// Adapter yang dipantau.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterSel {
    /// Jumlah semua adapter fisik yang Up.
    Auto,
    /// Satu adapter, dicocokkan lewat nama (alias) tanpa membedakan huruf besar/kecil.
    Named(String),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    /// Byte/detik masuk (download).
    pub down: u64,
    /// Byte/detik keluar (upload).
    pub up: u64,
    /// Beban CPU 0..=100.
    pub cpu: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    /// Byte masuk/keluar sejak sampel sebelumnya (untuk statistik pemakaian).
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// Throughput disk fisik (baca + tulis), byte/detik.
    pub disk: u64,
    /// Persen waktu aktif disk tersibuk (0..=100), seperti kolom Disk di Task Manager.
    pub disk_busy: f32,
}

impl Sample {
    pub fn mem_percent(&self) -> f32 {
        if self.mem_total == 0 {
            0.0
        } else {
            (self.mem_used as f64 * 100.0 / self.mem_total as f64) as f32
        }
    }
}

// ---------------------------------------------------------------- logika murni

/// Selisih counter; counter yang turun (reset/adapter di-restart) dihitung 0.
pub fn counter_delta(prev: u64, cur: u64) -> u64 {
    cur.saturating_sub(prev)
}

pub fn bytes_per_sec(delta: u64, dt: Duration) -> u64 {
    let us = dt.as_micros();
    if us == 0 {
        return 0;
    }
    (delta as u128 * 1_000_000 / us) as u64
}

/// `idle/kernel/user` dalam satuan 100 ns; kernel sudah mencakup idle.
pub fn cpu_percent(prev: (u64, u64, u64), cur: (u64, u64, u64)) -> f32 {
    let idle = cur.0.saturating_sub(prev.0);
    let total = cur.1.saturating_sub(prev.1) + cur.2.saturating_sub(prev.2);
    if total == 0 {
        return 0.0;
    }
    let busy = total.saturating_sub(idle);
    (busy as f64 * 100.0 / total as f64).clamp(0.0, 100.0) as f32
}

/// Satu pembacaan counter satu adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub luid: u64,
    pub rx: u64,
    pub tx: u64,
}

/// Menyimpan baseline counter per adapter (kunci LUID).
#[derive(Default)]
pub struct NetTracker {
    prev: Vec<Row>,
}

impl NetTracker {
    pub fn reset(&mut self) {
        self.prev.clear();
    }

    /// Mengembalikan total delta (rx, tx) sejak panggilan sebelumnya.
    /// Adapter yang baru muncul belum punya baseline sehingga delta-nya 0.
    pub fn update(&mut self, rows: &[Row]) -> (u64, u64) {
        let (mut rx, mut tx) = (0, 0);
        for r in rows {
            if let Some(p) = self.prev.iter().find(|p| p.luid == r.luid) {
                rx += counter_delta(p.rx, r.rx);
                tx += counter_delta(p.tx, r.tx);
            }
        }
        self.prev.clear();
        self.prev.extend_from_slice(rows);
        (rx, tx)
    }
}

// ------------------------------------------------------------------- Win32

fn wide_slice(buf: &[u16]) -> &[u16] {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    &buf[..n]
}

fn eq_wide_ci(a: &[u16], b: &[u16]) -> bool {
    let lower = |c: u16| if (b'A' as u16..=b'Z' as u16).contains(&c) { c + 32 } else { c };
    a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| lower(x) == lower(y))
}

fn flags(r: &MIB_IF_ROW2) -> u8 {
    r.InterfaceAndOperStatusFlags._bitfield
}

/// Adapter yang layak dihitung: Up, bukan loopback, bukan filter driver.
fn is_candidate(r: &MIB_IF_ROW2) -> bool {
    r.OperStatus == IF_OPER_STATUS_UP
        && r.Type != IF_TYPE_SOFTWARE_LOOPBACK
        && flags(r) & FLAG_FILTER == 0
}

/// Memanggil `f` untuk tiap baris tabel antarmuka; tabel dibebaskan sesudahnya.
fn for_each_row(mut f: impl FnMut(&MIB_IF_ROW2)) {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    // SAFETY: GetIfTable2 mengisi `table`; baris dibaca selama tabel belum dibebaskan.
    unsafe {
        if GetIfTable2(&mut table) != 0 || table.is_null() {
            return;
        }
        let n = (*table).NumEntries as usize;
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), n);
        rows.iter().for_each(&mut f);
        FreeMibTable(table as *const _);
    }
}

/// Nama adapter yang bisa dipilih di menu (Up, bukan loopback/filter/WAN Miniport).
pub fn list_adapters() -> Vec<String> {
    let mut out = Vec::new();
    for_each_row(|r| {
        let desc = String::from_utf16_lossy(wide_slice(&r.Description));
        if is_candidate(r) && !desc.starts_with("WAN Miniport") {
            out.push(String::from_utf16_lossy(wide_slice(&r.Alias)));
        }
    });
    out
}

fn filetime(ft: &FILETIME) -> u64 {
    (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64
}

fn read_cpu_times() -> Option<(u64, u64, u64)> {
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut idle, mut kernel, mut user) = (zero, zero, zero);
    // SAFETY: tiga pointer FILETIME valid.
    let ok = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
    (ok != 0).then(|| (filetime(&idle), filetime(&kernel), filetime(&user)))
}

/// (terpakai, total) byte RAM fisik.
fn read_memory() -> (u64, u64) {
    // SAFETY: struct di-zero lalu `dwLength` diisi sesuai kontrak API.
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) == 0 {
            return (0, 0);
        }
        (m.ullTotalPhys.saturating_sub(m.ullAvailPhys), m.ullTotalPhys)
    }
}

const IOCTL_DISK_PERFORMANCE: u32 = 0x0007_0020;
const MAX_PHYSICAL_DRIVES: u32 = 16;
/// Disk dicari ulang tiap sekian sampel (USB/eksternal bisa dicolok/dicabut).
const DISK_RESCAN_SAMPLES: u32 = 30;

/// Handle ke semua `\\.\PhysicalDriveN` untuk membaca counter byte lewat `IOCTL_DISK_PERFORMANCE`.
/// Dibuka dengan akses 0 (hanya kueri), jadi tidak butuh hak admin.
struct DiskCounters {
    handles: Vec<HANDLE>,
    /// (IdleTime, QueryTime) pembacaan sebelumnya per handle, untuk persen waktu aktif.
    prev: Vec<Option<(i64, i64)>>,
}

impl DiskCounters {
    fn open() -> Self {
        let mut handles = Vec::new();
        for n in 0..MAX_PHYSICAL_DRIVES {
            let path = crate::wide(&format!("\\\\.\\PhysicalDrive{n}"));
            // SAFETY: `path` null-terminated; tanpa SECURITY_ATTRIBUTES/template.
            let h = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if h != INVALID_HANDLE_VALUE {
                handles.push(h);
            }
        }
        DiskCounters { prev: vec![None; handles.len()], handles }
    }

    /// (total byte baca + tulis semua disk sejak boot, persen waktu aktif disk tersibuk
    /// sejak pembacaan lalu); `None` bila tak ada disk yang bisa dibaca. Persen aktif
    /// = 100 - persen IdleTime, definisi yang sama dengan kolom "Disk" Task Manager.
    fn read(&mut self) -> Option<(u64, f32)> {
        let mut total = 0u64;
        let mut busy_max = 0.0f32;
        let mut any = false;
        for (i, &h) in self.handles.iter().enumerate() {
            let mut buf = [0u8; 96]; // DISK_PERFORMANCE = 88 byte
            let mut ret = 0u32;
            // SAFETY: buffer keluaran 96 byte cukup untuk DISK_PERFORMANCE.
            let ok = unsafe {
                DeviceIoControl(h, IOCTL_DISK_PERFORMANCE, std::ptr::null(), 0, buf.as_mut_ptr().cast(), 96, &mut ret, std::ptr::null_mut())
            };
            if ok != 0 && ret >= 64 {
                let field = |o: usize| i64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
                total = total.saturating_add(field(0).max(0) as u64).saturating_add(field(8).max(0) as u64);
                let (idle, query) = (field(32), field(56));
                if let Some((pi, pq)) = self.prev[i] {
                    busy_max = busy_max.max(busy_percent(idle - pi, query - pq));
                }
                self.prev[i] = Some((idle, query));
                any = true;
            }
        }
        any.then_some((total, busy_max))
    }
}

/// Persen waktu aktif dari selisih IdleTime dan QueryTime (satuan sama, 100 ns).
fn busy_percent(idle_delta: i64, query_delta: i64) -> f32 {
    if query_delta <= 0 {
        return 0.0;
    }
    (100.0 * (1.0 - idle_delta as f64 / query_delta as f64)).clamp(0.0, 100.0) as f32
}

impl Drop for DiskCounters {
    fn drop(&mut self) {
        for &h in &self.handles {
            // SAFETY: handle milik kita dari CreateFileW.
            unsafe {
                CloseHandle(h);
            }
        }
    }
}

pub struct Sampler {
    sel: AdapterSel,
    name_w: Vec<u16>,
    net: NetTracker,
    rows: Vec<Row>,
    last_net: Option<Instant>,
    cpu_prev: Option<(u64, u64, u64)>,
    disk: DiskCounters,
    disk_prev: Option<u64>,
    samples: u32,
}

impl Sampler {
    pub fn new(sel: AdapterSel) -> Self {
        let mut s = Sampler {
            sel: AdapterSel::Auto,
            name_w: Vec::new(),
            net: NetTracker::default(),
            rows: Vec::new(),
            last_net: None,
            cpu_prev: read_cpu_times(),
            disk: DiskCounters::open(),
            disk_prev: None,
            samples: 0,
        };
        s.set_adapter(sel);
        s
    }

    pub fn adapter(&self) -> &AdapterSel {
        &self.sel
    }

    pub fn set_adapter(&mut self, sel: AdapterSel) {
        self.name_w = match &sel {
            AdapterSel::Named(n) => n.encode_utf16().collect(),
            AdapterSel::Auto => Vec::new(),
        };
        self.sel = sel;
        self.net.reset();
        self.last_net = None;
    }

    fn read_rows(&mut self) {
        let (auto, name_w) = (self.sel == AdapterSel::Auto, &self.name_w);
        let rows = &mut self.rows;
        rows.clear();
        for_each_row(|r| {
            let wanted = if auto {
                flags(r) & FLAG_HARDWARE != 0
            } else {
                eq_wide_ci(wide_slice(&r.Alias), name_w)
            };
            if wanted && is_candidate(r) {
                rows.push(Row { luid: unsafe { r.InterfaceLuid.Value }, rx: r.InOctets, tx: r.OutOctets });
            }
        });
    }

    /// Mengambil satu sampel; panggil sekali per tick.
    pub fn sample(&mut self) -> Sample {
        let now = Instant::now();
        self.read_rows();
        let (drx, dtx) = self.net.update(&self.rows);
        // Selang valid sejak sampel lalu (None pada sampel pertama atau setelah sleep/resume).
        let dt = self.last_net.replace(now).map(|t| now.duration_since(t)).filter(|d| *d <= MAX_GAP);
        let (down, up) = match dt {
            Some(dt) => (bytes_per_sec(drx, dt), bytes_per_sec(dtx, dt)),
            None => (0, 0),
        };

        self.samples = self.samples.wrapping_add(1);
        if self.samples.is_multiple_of(DISK_RESCAN_SAMPLES) {
            self.disk = DiskCounters::open();
            self.disk_prev = None;
        }
        let disk_now = self.disk.read();
        let disk = match (self.disk_prev, disk_now, dt) {
            (Some(p), Some((c, _)), Some(dt)) => bytes_per_sec(c.saturating_sub(p), dt),
            _ => 0,
        };
        let disk_busy = disk_now.map_or(0.0, |(_, b)| b);
        self.disk_prev = disk_now.map(|(bytes, _)| bytes);

        let cur = read_cpu_times();
        let cpu = match (self.cpu_prev, cur) {
            (Some(p), Some(c)) => cpu_percent(p, c),
            _ => 0.0,
        };
        self.cpu_prev = cur;

        let (mem_used, mem_total) = read_memory();
        // Byte tetap dihitung walau kecepatan dibuang (mis. setelah sleep): trafiknya nyata.
        Sample { down, up, cpu, mem_used, mem_total, rx_bytes: drx, tx_bytes: dtx, disk, disk_busy }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_busy_percent_from_idle_time() {
        assert_eq!(busy_percent(0, 1_000), 100.0); // tidak pernah idle
        assert_eq!(busy_percent(1_000, 1_000), 0.0); // idle penuh
        assert!((busy_percent(570, 1_000) - 43.0).abs() < 0.01);
        assert_eq!(busy_percent(2_000, 1_000), 0.0); // dijepit
        assert_eq!(busy_percent(5, 0), 0.0); // selang nol/negatif aman
    }

    #[test]
    fn counter_reset_is_zero() {
        assert_eq!(counter_delta(100, 150), 50);
        assert_eq!(counter_delta(500, 10), 0);
    }

    #[test]
    fn rate_uses_real_elapsed_time() {
        assert_eq!(bytes_per_sec(1000, Duration::from_millis(500)), 2000);
        assert_eq!(bytes_per_sec(1000, Duration::ZERO), 0);
    }

    #[test]
    fn cpu_percent_basic() {
        // 1000 total (kernel+user), 250 idle -> 75 % sibuk
        let p = (0, 0, 0);
        let c = (250, 400, 600);
        assert!((cpu_percent(p, c) - 75.0).abs() < 0.01);
        assert_eq!(cpu_percent(c, c), 0.0);
    }

    #[test]
    fn tracker_new_and_vanished_adapters_do_not_spike() {
        let mut t = NetTracker::default();
        let a = Row { luid: 1, rx: 1_000, tx: 100 };
        assert_eq!(t.update(&[a]), (0, 0)); // belum ada baseline
        let a2 = Row { rx: 1_500, tx: 130, ..a };
        let b = Row { luid: 2, rx: 9_999_999, tx: 9_999_999 }; // adapter baru
        assert_eq!(t.update(&[a2, b]), (500, 30));
        // adapter `a` hilang, `b` ada: hanya delta b
        let b2 = Row { rx: b.rx + 7, tx: b.tx + 3, ..b };
        assert_eq!(t.update(&[b2]), (7, 3));
        // counter b reset -> 0, bukan lonjakan
        let b3 = Row { rx: 5, tx: 5, ..b };
        assert_eq!(t.update(&[b3]), (0, 0));
    }

    #[test]
    fn alias_compare_ignores_ascii_case() {
        let a: Vec<u16> = "Wi-Fi".encode_utf16().collect();
        let b: Vec<u16> = "wi-fi".encode_utf16().collect();
        assert!(eq_wide_ci(&a, &b));
        assert!(!eq_wide_ci(&a, &b[..3]));
    }

    #[test]
    fn live_sample_is_sane() {
        let mut s = Sampler::new(AdapterSel::Auto);
        std::thread::sleep(Duration::from_millis(200));
        let v = s.sample();
        assert!(v.mem_total > 0 && v.mem_used <= v.mem_total);
        assert!((0.0..=100.0).contains(&v.cpu));
    }
}
