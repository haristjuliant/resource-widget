//! Proses teratas per metrik (CPU, RAM, disk, jaringan).
//!
//! Satu panggilan `NtQuerySystemInformation(SystemProcessInformation)` mengembalikan semua
//! proses beserta waktu CPU, working set privat, dan hitungan byte I/O. Proses digabung
//! menurut nama image (mis. semua `chrome.exe`) agar "aplikasi" yang dilaporkan, bukan satu PID.
//!
//! Catatan akurasi:
//! - Disk: byte baca+tulis per proses menurut I/O manager (termasuk yang dilayani cache),
//!   jadi ini "aktivitas I/O", bukan byte fisik di disk.
//! - Jaringan: Windows tidak memberi byte jaringan per proses tanpa hak admin. Dipakai
//!   pendekatan: byte "Other I/O" (IOCTL, termasuk soket) hanya dari proses yang punya
//!   soket TCP/UDP. Ini perkiraan.

use std::ffi::c_void;
use std::time::Instant;

use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCPROW_OWNER_PID, MIB_UDPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
    UDP_TABLE_OWNER_PID,
};
use windows_sys::Win32::Networking::WinSock::AF_INET;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(class: i32, buf: *mut c_void, len: u32, ret: *mut u32) -> i32;
}

const SYSTEM_PROCESS_INFORMATION: i32 = 5;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;
/// Cukup untuk ±1500 proses tanpa panggilan ulang; tumbuh otomatis bila kurang.
const INITIAL_BUF: usize = 1 << 20;

/// Awal `SYSTEM_PROCESS_INFORMATION` (x64) sampai `OtherTransferCount`; deretan thread diabaikan.
#[repr(C)]
struct ProcEntry {
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private: i64,
    hard_fault_count: u32,
    threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    name_len: u16, // byte
    name_max: u16,
    name_buf: *const u16,
    base_priority: i32,
    pid: usize,
    parent_pid: usize,
    handle_count: u32,
    session_id: u32,
    unique_process_key: usize,
    peak_virtual: usize,
    virtual_size: usize,
    page_fault_count: u32,
    peak_working_set: usize,
    working_set: usize,
    quota_peak_paged: usize,
    quota_paged: usize,
    quota_peak_nonpaged: usize,
    quota_nonpaged: usize,
    pagefile: usize,
    peak_pagefile: usize,
    private_pages: usize,
    read_ops: i64,
    write_ops: i64,
    other_ops: i64,
    read_bytes: i64,
    write_bytes: i64,
    other_bytes: i64,
}
const _: () = assert!(std::mem::size_of::<ProcEntry>() == 256);

/// Pemenang satu metrik.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Top {
    /// Nama image tanpa ".exe".
    pub name: String,
    /// CPU: persen × 10 (0..=1000). RAM: byte. Disk/jaringan: byte per detik.
    pub value: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcTop {
    pub cpu: Option<Top>,
    pub mem: Option<Top>,
    pub disk: Option<Top>,
    pub net: Option<Top>,
}

/// Satu proses pada pemindaian ini.
struct Cur<'a> {
    pid: u32,
    create: i64,
    cpu: u64, // kernel + user, 100 ns
    ws: u64,  // working set privat, byte
    io: u64,  // baca + tulis, byte
    other: u64,
    name: &'a [u16],
    // delta sejak pemindaian lalu (0 bila proses baru)
    cpu_d: u64,
    io_d: u64,
    other_d: u64,
}

/// Catatan pemindaian sebelumnya, terurut menurut `pid`.
#[derive(Clone, Copy)]
struct Prev {
    pid: u32,
    create: i64,
    cpu: u64,
    io: u64,
    other: u64,
}

// ---------------------------------------------------------------- logika murni

fn lower(c: u16) -> u16 {
    if (b'A' as u16..=b'Z' as u16).contains(&c) { c + 32 } else { c }
}

fn cmp_name_ci(a: &[u16], b: &[u16]) -> std::cmp::Ordering {
    a.iter().map(|&c| lower(c)).cmp(b.iter().map(|&c| lower(c)))
}

fn eq_ci(name: &[u16], s: &str) -> bool {
    name.len() == s.len() && name.iter().zip(s.bytes()).all(|(&a, b)| lower(a) == b.to_ascii_lowercase() as u16)
}

/// Proses semu kernel yang bukan "aplikasi": tidak layak jadi pemenang RAM.
fn is_pseudo(name: &[u16]) -> bool {
    ["Memory Compression", "Registry", "Secure System", "System Idle Process", "System"].iter().any(|s| eq_ci(name, s))
}

/// Di bawah kecepatan antarmuka ini (byte/dtk) proses jaringan tidak ditampilkan:
/// perkiraan "Other I/O" terlalu dikuasai derau saat jaringan nyaris diam.
const NET_MIN_BPS: u64 = 256;

/// "chrome.exe" -> "chrome".
pub fn short_name(name: &[u16]) -> String {
    let s = String::from_utf16_lossy(name);
    match s.len().checked_sub(4) {
        Some(i) if s.is_char_boundary(i) && s[i..].eq_ignore_ascii_case(".exe") => s[..i].to_string(),
        _ => s,
    }
}

/// Mengisi delta `cur` terhadap `prev`, lalu mengganti `prev` dengan keadaan sekarang.
/// `cur` harus terurut menurut `pid`; `prev` juga.
fn apply_deltas(cur: &mut [Cur], prev: &mut Vec<Prev>) {
    for c in cur.iter_mut() {
        if let Ok(i) = prev.binary_search_by_key(&c.pid, |p| p.pid) {
            let p = prev[i];
            if p.create == c.create {
                c.cpu_d = c.cpu.saturating_sub(p.cpu);
                c.io_d = c.io.saturating_sub(p.io);
                c.other_d = c.other.saturating_sub(p.other);
            }
        }
    }
    prev.clear();
    prev.extend(cur.iter().map(|c| Prev { pid: c.pid, create: c.create, cpu: c.cpu, io: c.io, other: c.other }));
}

/// Menggabungkan menurut nama dan memilih pemenang tiap metrik.
/// `elapsed_s` = detik sejak pemindaian lalu (0 = belum ada pembanding).
/// `socket_pids` terurut; hanya proses di dalamnya yang dihitung untuk jaringan.
/// `iface_bps` = trafik antarmuka (masuk + keluar): batas atas untuk perkiraan jaringan per proses.
fn pick_tops(cur: &mut [Cur], elapsed_s: f64, ncpu: u32, socket_pids: &[u32], iface_bps: u64) -> ProcTop {
    cur.sort_by(|a, b| cmp_name_ci(a.name, b.name));
    let mut best_cpu = (0u64, None::<usize>);
    let mut best_mem = (0u64, None::<usize>);
    let mut best_io = (0u64, None::<usize>);
    let mut best_net = (0u64, None::<usize>);

    let mut i = 0;
    while i < cur.len() {
        let mut j = i;
        let (mut cpu, mut mem, mut io, mut net) = (0u64, 0u64, 0u64, 0u64);
        while j < cur.len() && cmp_name_ci(cur[j].name, cur[i].name).is_eq() {
            let c = &cur[j];
            if c.pid != 0 {
                cpu += c.cpu_d;
            }
            mem += c.ws;
            io += c.io_d;
            if socket_pids.binary_search(&c.pid).is_ok() {
                net += c.other_d;
            }
            j += 1;
        }
        if cur[i].pid != 0 || j - i > 1 {
            if cpu > best_cpu.0 {
                best_cpu = (cpu, Some(i));
            }
            if mem > best_mem.0 && !is_pseudo(cur[i].name) {
                best_mem = (mem, Some(i));
            }
            if io > best_io.0 {
                best_io = (io, Some(i));
            }
            if net > best_net.0 {
                best_net = (net, Some(i));
            }
        }
        i = j;
    }

    let name_of = |idx: Option<usize>| idx.map(|k| short_name(cur[k].name)).unwrap_or_default();
    let pick = |best: (u64, Option<usize>), value: u64| best.1.map(|_| Top { name: name_of(best.1), value });
    let have_prev = elapsed_s > 0.0;
    let rate = |bytes: u64| (bytes as f64 / elapsed_s) as u64;
    ProcTop {
        mem: pick(best_mem, best_mem.0),
        cpu: have_prev
            .then(|| {
                let pct10 = best_cpu.0 as f64 / (elapsed_s * 1e7 * ncpu.max(1) as f64) * 1000.0;
                pick(best_cpu, pct10.round().clamp(0.0, 1000.0) as u64)
            })
            .flatten(),
        disk: have_prev.then(|| pick(best_io, rate(best_io.0))).flatten(),
        net: (have_prev && iface_bps >= NET_MIN_BPS)
            .then(|| pick(best_net, rate(best_net.0).min(iface_bps)))
            .flatten(),
    }
}

// ------------------------------------------------------------------- Win32

/// PID pemilik soket TCP/UDP IPv4 (terurut, tanpa duplikat).
fn socket_owner_pids(out: &mut Vec<u32>, buf: &mut Vec<u8>) {
    out.clear();
    for udp in [false, true] {
        // Dua pemanggilan: pertama menanyakan ukuran, kedua mengambil data.
        if buf.is_empty() {
            buf.resize(64 * 1024, 0);
        }
        for _ in 0..3 {
            let mut size = buf.len() as u32;
            let rc = unsafe {
                if udp {
                    GetExtendedUdpTable(buf.as_mut_ptr().cast(), &mut size, 0, AF_INET as u32, UDP_TABLE_OWNER_PID, 0)
                } else {
                    GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut size, 0, AF_INET as u32, TCP_TABLE_OWNER_PID_ALL, 0)
                }
            };
            match rc {
                0 => {
                    // Tabel: DWORD jumlah entri, lalu baris-barisnya.
                    let n = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
                    let rows = unsafe {
                        if udp {
                            let p = buf.as_ptr().add(4).cast::<MIB_UDPROW_OWNER_PID>();
                            (0..n).map(|k| (*p.add(k)).dwOwningPid).collect::<Vec<_>>()
                        } else {
                            let p = buf.as_ptr().add(4).cast::<MIB_TCPROW_OWNER_PID>();
                            (0..n).map(|k| (*p.add(k)).dwOwningPid).collect::<Vec<_>>()
                        }
                    };
                    out.extend(rows);
                    break;
                }
                122 => buf.resize(size as usize + 4096, 0), // ERROR_INSUFFICIENT_BUFFER
                _ => break,
            }
        }
    }
    out.sort_unstable();
    out.dedup();
}

pub struct ProcMonitor {
    /// Kosong di antara pemindaian (dilepas agar memori proses kembali kecil).
    buf: Vec<u8>,
    /// Ukuran buffer yang dibutuhkan pemindaian terakhir.
    buf_hint: usize,
    sock_buf: Vec<u8>,
    sockets: Vec<u32>,
    prev: Vec<Prev>,
    last: Option<Instant>,
    ncpu: u32,
    top: ProcTop,
}

impl Default for ProcMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcMonitor {
    pub fn new() -> Self {
        ProcMonitor {
            buf: Vec::new(),
            buf_hint: INITIAL_BUF,
            sock_buf: Vec::new(),
            sockets: Vec::new(),
            prev: Vec::new(),
            last: None,
            ncpu: std::thread::available_parallelism().map_or(1, |n| n.get() as u32),
            top: ProcTop::default(),
        }
    }

    pub fn top(&self) -> &ProcTop {
        &self.top
    }

    fn query(&mut self) -> bool {
        if self.buf.is_empty() {
            self.buf = vec![0; self.buf_hint];
        }
        for _ in 0..4 {
            let mut need = 0u32;
            let st = unsafe {
                NtQuerySystemInformation(
                    SYSTEM_PROCESS_INFORMATION,
                    self.buf.as_mut_ptr().cast(),
                    self.buf.len() as u32,
                    &mut need,
                )
            };
            if st >= 0 {
                self.buf_hint = need as usize + (64 << 10);
                return true;
            }
            if st != STATUS_INFO_LENGTH_MISMATCH {
                self.buf = Vec::new();
                return false;
            }
            self.buf.resize((need as usize + (64 << 10)).max(self.buf.len() * 2), 0);
        }
        self.buf = Vec::new();
        false
    }

    /// Memindai proses dan memperbarui `top()`. Mahal dibanding sampel lain (≈8 ms pada
    /// 450 proses): panggil jarang. `iface_bps` = trafik antarmuka saat ini (masuk + keluar).
    pub fn scan(&mut self, iface_bps: u64) -> &ProcTop {
        if !self.query() {
            return &self.top;
        }
        let now = Instant::now();
        let elapsed_s = self.last.map_or(0.0, |t| now.duration_since(t).as_secs_f64());
        self.last = Some(now);
        socket_owner_pids(&mut self.sockets, &mut self.sock_buf);

        // SAFETY: buffer berisi rantai SYSTEM_PROCESS_INFORMATION yang valid dari kernel;
        // `name_buf` menunjuk ke dalam buffer yang sama dan tetap hidup selama `cur`.
        let mut cur: Vec<Cur> = Vec::with_capacity(self.prev.len().max(256));
        unsafe {
            let mut off = 0usize;
            loop {
                let e = &*(self.buf.as_ptr().add(off) as *const ProcEntry);
                let name = if e.name_buf.is_null() {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(e.name_buf, e.name_len as usize / 2)
                };
                cur.push(Cur {
                    pid: e.pid as u32,
                    create: e.create_time,
                    cpu: (e.kernel_time + e.user_time).max(0) as u64,
                    ws: e.working_set_private.max(0) as u64,
                    io: (e.read_bytes.max(0) as u64).saturating_add(e.write_bytes.max(0) as u64),
                    other: e.other_bytes.max(0) as u64,
                    name,
                    cpu_d: 0,
                    io_d: 0,
                    other_d: 0,
                });
                if e.next_entry_offset == 0 {
                    break;
                }
                off += e.next_entry_offset as usize;
            }
        }
        cur.sort_by_key(|c| c.pid);
        apply_deltas(&mut cur, &mut self.prev);
        self.top = pick_tops(&mut cur, elapsed_s, self.ncpu, &self.sockets, iface_bps);
        drop(cur);
        self.buf = Vec::new(); // lepas ±1 MB sampai pemindaian berikutnya
        &self.top
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn mk<'a>(pid: u32, name: &'a [u16], cpu: u64, ws: u64, io: u64, other: u64) -> Cur<'a> {
        Cur { pid, create: 1, cpu, ws, io, other, name, cpu_d: 0, io_d: 0, other_d: 0 }
    }

    #[test]
    fn short_name_strips_exe_case_insensitively() {
        assert_eq!(short_name(&w("chrome.exe")), "chrome");
        assert_eq!(short_name(&w("Code.EXE")), "Code");
        assert_eq!(short_name(&w("System")), "System");
        assert_eq!(short_name(&w("exe")), "exe");
        assert_eq!(short_name(&[]), "");
    }

    #[test]
    fn deltas_ignore_new_processes_and_pid_reuse() {
        let (a, b) = (w("a.exe"), w("b.exe"));
        let mut prev = vec![Prev { pid: 10, create: 1, cpu: 100, io: 50, other: 5 }, Prev { pid: 20, create: 7, cpu: 100, io: 0, other: 0 }];
        let mut cur = vec![mk(10, &a, 600, 0, 150, 25), mk(20, &b, 900, 0, 10, 0), mk(30, &a, 5_000, 0, 7, 7)];
        cur[1].create = 8; // PID 20 dipakai ulang proses lain
        apply_deltas(&mut cur, &mut prev);
        assert_eq!((cur[0].cpu_d, cur[0].io_d, cur[0].other_d), (500, 100, 20));
        assert_eq!(cur[1].cpu_d, 0); // create_time beda: bukan proses yang sama
        assert_eq!(cur[2].cpu_d, 0); // proses baru: belum ada pembanding
        assert_eq!(prev.len(), 3);
    }

    #[test]
    fn tops_group_by_name_and_filter_network_by_sockets() {
        let (chrome, code, idle) = (w("chrome.exe"), w("Code.exe"), w(""));
        let mut cur = vec![
            mk(0, &idle, 0, 0, 0, 0),
            mk(100, &chrome, 0, 400, 0, 0),
            mk(101, &chrome, 0, 500, 0, 0), // chrome total 900 byte RAM
            mk(200, &code, 0, 700, 0, 0),
        ];
        // delta CPU: chrome 2 x 30 + Code 50 -> chrome menang menurut total gabungan
        cur[1].cpu_d = 30;
        cur[2].cpu_d = 30;
        cur[3].cpu_d = 50;
        cur[3].io_d = 5_000;
        cur[1].other_d = 900; // chrome punya soket
        cur[3].other_d = 9_000_000; // Code tidak punya soket -> diabaikan untuk jaringan
        let top = pick_tops(&mut cur, 1.0, 1, &[100], 1_000_000);
        assert_eq!(top.mem, Some(Top { name: "chrome".into(), value: 900 }));
        assert_eq!(top.cpu.as_ref().map(|t| t.name.as_str()), Some("chrome"));
        assert_eq!(top.disk, Some(Top { name: "Code".into(), value: 5_000 }));
        assert_eq!(top.net, Some(Top { name: "chrome".into(), value: 900 }));
    }

    #[test]
    fn cpu_percent_is_share_of_all_cpus_and_idle_is_excluded() {
        let (idle, app) = (w("System Idle Process"), w("app.exe"));
        let mut cur = vec![mk(0, &idle, 0, 0, 0, 0), mk(5, &app, 0, 10, 0, 0)];
        cur[0].cpu_d = 100_000_000; // Idle tidak boleh jadi pemenang
        cur[1].cpu_d = 10_000_000; // 1 dtk CPU dalam 1 dtk pada 4 CPU = 25 %
        let top = pick_tops(&mut cur, 1.0, 4, &[], 0);
        assert_eq!(top.cpu, Some(Top { name: "app".into(), value: 250 }));
    }

    #[test]
    fn pseudo_processes_never_win_ram() {
        let (mc, app) = (w("Memory Compression"), w("app.exe"));
        let mut cur = vec![mk(2, &mc, 0, 3_000, 0, 0), mk(5, &app, 0, 100, 0, 0)];
        let top = pick_tops(&mut cur, 1.0, 4, &[], 0);
        assert_eq!(top.mem, Some(Top { name: "app".into(), value: 100 }));
    }

    #[test]
    fn network_estimate_is_capped_by_interface_and_hidden_when_idle() {
        let app = w("app.exe");
        let mut cur = vec![mk(5, &app, 0, 10, 0, 0)];
        cur[0].other_d = 5_000_000; // perkiraan proses jauh di atas trafik antarmuka
        let top = pick_tops(&mut cur, 1.0, 4, &[5], 40_000);
        assert_eq!(top.net, Some(Top { name: "app".into(), value: 40_000 }));
        // antarmuka nyaris diam: tidak ada proses jaringan yang dilaporkan
        let top = pick_tops(&mut cur, 1.0, 4, &[5], 100);
        assert!(top.net.is_none());
    }

    #[test]
    fn first_scan_reports_memory_only() {
        let app = w("app.exe");
        let mut cur = vec![mk(5, &app, 99, 1234, 99, 99)];
        let top = pick_tops(&mut cur, 0.0, 4, &[5], 1_000_000);
        assert!(top.cpu.is_none() && top.disk.is_none() && top.net.is_none());
        assert_eq!(top.mem, Some(Top { name: "app".into(), value: 1234 }));
    }

    #[test]
    fn live_scan_sees_this_process() {
        let mut m = ProcMonitor::new();
        m.scan(0);
        let me = std::process::id();
        let mem = m.top().mem.clone().expect("harus ada pemenang RAM");
        assert!(mem.value > 0);
        // proses ini harus muncul di buffer: cek lewat tabel PID
        assert!(m.prev.binary_search_by_key(&me, |p| p.pid).is_ok());
    }
}
