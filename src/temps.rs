//! Suhu dari sumber bawaan Windows, tanpa driver tambahan.
//!
//! - SSD: `IOCTL_STORAGE_QUERY_PROPERTY` + `StorageDeviceTemperatureProperty` pada
//!   `\\.\PhysicalDriveN` (sumber yang sama dengan Task Manager), tanpa hak admin. Hanya disk
//!   internal tanpa seek penalty (SSD) yang ditanya, jadi HDD yang sedang tidur tidak dibangunkan.
//! - CPU: blok data WMI "EsifDeviceInformation" dari driver Intel Dynamic Tuning (DTT/IPF),
//!   dibaca langsung lewat `WmiOpenBlock`/`WmiQueryAllDataW` advapi32 tanpa tumpukan COM/WMI.
//!   Hanya ada di laptop Intel dengan DTT, dan driver hanya mengizinkan admin membacanya.
//! - Cadangan CPU: zona termal ACPI, counter set perflib V2 "Thermal Zone Information" lewat
//!   API `Perf*` advapi32, tanpa hak admin. Tidak ada di semua mesin, dan di sebagian mesin
//!   nilainya statis; di banyak laptop zona ini mengikuti panas CPU.
//!
//! PDH sengaja tidak dipakai: memuat dan menyiapkannya saja menambah ±3 MB memori privat.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, ERROR_NOT_ENOUGH_MEMORY, HANDLE};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Performance::{
    PERF_COUNTER_IDENTIFIER, PERF_MULTIPLE_INSTANCES, PERF_WILDCARD_COUNTER, PerfAddCounters, PerfCloseQueryHandle,
    PerfEnumerateCounterSetInstances, PerfOpenQueryHandle, PerfQueryCounterData,
};
use windows_sys::core::GUID;

use crate::metrics::{MAX_PHYSICAL_DRIVES, open_physical_drive};

/// Daftar SSD dicari ulang tiap sekian pembacaan (≈1 menit pada selang 5 detik).
const DRIVE_RESCAN_POLLS: u32 = 12;

// ---------------------------------------------------------------- logika murni

/// Nilai di luar rentang wajar (0, negatif, nilai sentinel driver) dianggap tidak ada.
fn valid(c: f32) -> Option<f32> {
    (1.0..=150.0).contains(&c).then_some(c)
}

/// Suhu tertinggi (°C) dari `STORAGE_TEMPERATURE_DATA_DESCRIPTOR`: kepala 24 byte, lalu
/// `InfoCount` (u16 di offset 12) entri `STORAGE_TEMPERATURE_INFO` 16 byte dengan
/// `Temperature` (i16, °C) di offset 2 tiap entri.
pub fn parse_storage_temp(buf: &[u8]) -> Option<f32> {
    let count = u16::from_le_bytes(buf.get(12..14)?.try_into().ok()?) as usize;
    (0..count)
        .map_while(|i| buf.get(24 + i * 16 + 2..24 + i * 16 + 4))
        .filter_map(|b| valid(i16::from_le_bytes([b[0], b[1]]) as f32))
        .reduce(f32::max)
}

/// Kelvin → °C. "High Precision Temperature" memakai persepuluh kelvin (`tenths`).
pub fn kelvin_to_c(v: u64, tenths: bool) -> f32 {
    let k = if tenths { v as f64 / 10.0 } else { v as f64 };
    (k - 273.15) as f32
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// Nilai counter pertama tiap instance dari keluaran `PerfQueryCounterData`:
/// `PERF_DATA_HEADER` (48 byte), `PERF_COUNTER_HEADER` (16 byte; dwType di offset 4),
/// `PERF_MULTI_INSTANCES` (8 byte; jumlah instance di offset 4), lalu tiap instance:
/// `PERF_INSTANCE_HEADER` (Size mencakup nama) + `PERF_COUNTER_DATA` (DataSize, Size) + nilai.
pub fn parse_multi_instance(buf: &[u8]) -> Vec<u64> {
    let mut out = Vec::new();
    const DATA_HEADER: usize = 48;
    if u32_at(buf, 4).unwrap_or(0) == 0 || u32_at(buf, DATA_HEADER + 4) != Some(PERF_MULTIPLE_INSTANCES as u32) {
        return out;
    }
    let count = u32_at(buf, DATA_HEADER + 16 + 4).unwrap_or(0);
    let mut o = DATA_HEADER + 16 + 8;
    for _ in 0..count {
        let Some(inst_size) = u32_at(buf, o).filter(|&s| s >= 8) else { break };
        o += inst_size as usize;
        let (Some(data_size), Some(size)) = (u32_at(buf, o), u32_at(buf, o + 4)) else { break };
        let v = match data_size {
            4 => u32_at(buf, o + 8).map(u64::from),
            8 => buf.get(o + 8..o + 16).map(|b| u64::from_le_bytes(b.try_into().unwrap())),
            _ => None,
        };
        out.extend(v);
        if size < 8 {
            break;
        }
        o += size as usize;
    }
    out
}

/// Instance prosesor ESIF: "processor participant" Intel adalah perangkat PCI (B0:D4:F0);
/// participant lain (sensor kulit, baterai, ...) adalah perangkat ACPI dan tidak dihitung.
fn is_processor_instance(name: &[u16]) -> bool {
    const PREFIX: &[u8] = b"PCI\\VEN_8086";
    name.len() >= PREFIX.len() && name.iter().zip(PREFIX).all(|(&c, &p)| c < 128 && (c as u8).eq_ignore_ascii_case(&p))
}

/// Suhu (°C) tertinggi dari domain processor participant dalam keluaran `WmiQueryAllDataW`
/// untuk "EsifDeviceInformation": rantai `WNODE_ALL_DATA` (sambungan di `Linkage`, offset 12).
/// Tiap node: `Flags` (44), `DataBlockOffset` (48), `InstanceCount` (52),
/// `OffsetInstanceNameOffsets` (56), lalu `FixedInstanceSize` atau pasangan (offset, panjang)
/// per instance (60). Data instance diawali `Temperature` (u32, °C); nama instance berupa
/// panjang byte (u16) diikuti UTF-16.
pub fn parse_esif_cpu(buf: &[u8]) -> Option<f32> {
    const FIXED_INSTANCE_SIZE: u32 = 0x10;
    const STATIC_INSTANCE_NAMES: u32 = 0x40;
    let mut best: Option<f32> = None;
    let mut node = 0usize;
    for _ in 0..64 {
        let at = |o: usize| u32_at(buf, node + o);
        let (Some(flags), Some(data_off), Some(count), Some(names_off)) = (at(44), at(48), at(52), at(56)) else { break };
        for i in 0..count as usize {
            let data = if flags & FIXED_INSTANCE_SIZE != 0 {
                at(60).map(|fixed| data_off as usize + i * (fixed as usize).next_multiple_of(8))
            } else {
                at(60 + i * 8).map(|o| o as usize)
            };
            // Tanpa nama instance (statis), semua instance dianggap prosesor.
            let is_cpu = flags & STATIC_INSTANCE_NAMES != 0
                || at(names_off as usize + i * 4).is_some_and(|no| {
                    let n = node + no as usize;
                    let len = buf.get(n..n + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]) as usize);
                    let name: Vec<u16> = buf.get(n + 2..n + 2 + len).unwrap_or(&[]).as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect();
                    is_processor_instance(&name)
                });
            if is_cpu && let Some(c) = data.and_then(at).and_then(|t| valid(t as f32)) {
                best = Some(best.map_or(c, |b| b.max(c)));
            }
        }
        match at(12) {
            Some(link) if link > 0 => node += link as usize,
            _ => break,
        }
    }
    best
}

// ------------------------------------------------------------------- SSD

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
const STORAGE_DEVICE_PROPERTY: i32 = 0;
const STORAGE_DEVICE_SEEK_PENALTY_PROPERTY: i32 = 7;
const STORAGE_DEVICE_TEMPERATURE_PROPERTY: i32 = 52;
/// `STORAGE_BUS_TYPE` disk internal: ATA, RAID, SAS, SATA, NVMe, SCM, UFS. USB, SD/MMC,
/// dan disk virtual sengaja tidak termasuk.
const INTERNAL_BUSES: [u32; 7] = [3, 8, 10, 11, 17, 18, 19];

/// `IOCTL_STORAGE_QUERY_PROPERTY` (PropertyStandardQuery) ke `out`; jumlah byte yang ditulis.
fn query_property(h: HANDLE, id: i32, out: &mut [u8]) -> Option<usize> {
    // STORAGE_PROPERTY_QUERY: PropertyId, QueryType = 0 (standard), AdditionalParameters
    let mut q = [0u8; 12];
    q[..4].copy_from_slice(&id.to_le_bytes());
    let mut ret = 0u32;
    // SAFETY: buffer masukan/keluaran hidup selama pemanggilan dan panjangnya sesuai.
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            q.as_ptr().cast(),
            q.len() as u32,
            out.as_mut_ptr().cast(),
            out.len() as u32,
            &mut ret,
            null_mut(),
        )
    };
    (ok != 0).then_some(ret as usize)
}

/// SSD internal: bukan media lepasan, bus internal, dan tanpa seek penalty. Semua dari
/// deskriptor yang di-cache driver, jadi tidak mengirim perintah ke disk.
fn is_internal_ssd(h: HANDLE) -> bool {
    let mut d = [0u8; 1024]; // STORAGE_DEVICE_DESCRIPTOR + string
    let Some(n) = query_property(h, STORAGE_DEVICE_PROPERTY, &mut d) else { return false };
    if n < 32 {
        return false;
    }
    let removable = d[10] != 0;
    let bus = u32::from_le_bytes(d[28..32].try_into().unwrap());
    let mut sp = [0u8; 12]; // DEVICE_SEEK_PENALTY_DESCRIPTOR: Version, Size, IncursSeekPenalty
    let no_seek = query_property(h, STORAGE_DEVICE_SEEK_PENALTY_PROPERTY, &mut sp).is_some_and(|n| n >= 9 && sp[8] == 0);
    !removable && INTERNAL_BUSES.contains(&bus) && no_seek
}

fn read_drive_temp(h: HANDLE) -> Option<f32> {
    let mut buf = [0u8; 512];
    let n = query_property(h, STORAGE_DEVICE_TEMPERATURE_PROPERTY, &mut buf)?;
    parse_storage_temp(&buf[..n])
}

/// Handle SSD internal yang terbukti melaporkan suhu.
struct Drives(Vec<HANDLE>);

impl Drives {
    fn open() -> Self {
        let mut v = Vec::new();
        for h in (0..MAX_PHYSICAL_DRIVES).filter_map(open_physical_drive) {
            if is_internal_ssd(h) && read_drive_temp(h).is_some() {
                v.push(h);
            } else {
                // SAFETY: handle milik kita dari CreateFileW.
                unsafe {
                    CloseHandle(h);
                }
            }
        }
        Drives(v)
    }

    fn hottest(&self) -> Option<f32> {
        self.0.iter().filter_map(|&h| read_drive_temp(h)).reduce(f32::max)
    }
}

impl Drop for Drives {
    fn drop(&mut self) {
        for &h in &self.0 {
            // SAFETY: handle milik kita dari CreateFileW.
            unsafe {
                CloseHandle(h);
            }
        }
    }
}

// ------------------------------------------------------- counter perflib V2

/// Counter set "Thermal Zone Information" (penyedia kernel ACPI).
const THERMAL_ZONE_SET: GUID = GUID::from_u128(0x52bc5412_dac2_449c_8bc2_96443888fe6b);
/// Counter id di set itu: 3 = "High Precision Temperature" (persepuluh K), 0 = "Temperature" (K).
const ZONE_COUNTERS: [(u32, bool); 2] = [(3, true), (0, false)];

/// Query perflib V2 untuk satu counter di semua instance counter set.
struct CounterQuery(HANDLE);

impl CounterQuery {
    /// `true` bila counter set punya minimal satu instance (murah; tanpa membuka query).
    fn has_instances(set: &GUID) -> bool {
        let mut needed = 0u32;
        // SAFETY: buffer null dengan ukuran 0 hanya menanyakan ukuran yang dibutuhkan.
        let r = unsafe { PerfEnumerateCounterSetInstances(null(), set, null_mut(), 0, &mut needed) };
        r == ERROR_NOT_ENOUGH_MEMORY && needed > 0
    }

    fn open(set: &GUID, counter: u32) -> Option<Self> {
        let mut h: HANDLE = null_mut();
        // SAFETY: pointer keluaran valid; mesin lokal (null).
        if unsafe { PerfOpenQueryHandle(null(), &mut h) } != 0 {
            return None;
        }
        let q = CounterQuery(h); // menutup handle bila langkah berikut gagal
        // PERF_COUNTER_IDENTIFIER (40 byte) diikuti nama instance "*" (semua instance), dibulatkan ke 8 byte.
        const ID_SIZE: usize = size_of::<PERF_COUNTER_IDENTIFIER>();
        let mut buf = [0u64; (ID_SIZE + 8) / 8];
        // SAFETY: `buf` (u64, rata 8) cukup untuk struct + nama; ditulis lewat pointer bertipe.
        unsafe {
            let id = buf.as_mut_ptr().cast::<PERF_COUNTER_IDENTIFIER>();
            *id = PERF_COUNTER_IDENTIFIER {
                CounterSetGuid: *set,
                Status: 0,
                Size: (ID_SIZE + 8) as u32,
                CounterId: counter,
                InstanceId: PERF_WILDCARD_COUNTER,
                Index: 0,
                Reserved: 0,
            };
            let name = buf.as_mut_ptr().cast::<u8>().add(ID_SIZE).cast::<u16>();
            *name = b'*' as u16;
            *name.add(1) = 0;
            if PerfAddCounters(q.0, id, (ID_SIZE + 8) as u32) != 0 || (*id).Status != 0 {
                return None;
            }
        }
        Some(q)
    }

    /// Nilai counter tiap instance; kosong bila gagal.
    fn values(&self) -> Vec<u64> {
        let mut buf = vec![0u64; 64]; // 512 byte; tumbuh bila kurang
        for _ in 0..3 {
            let mut needed = 0u32;
            let len = (buf.len() * 8) as u32;
            // SAFETY: buffer milik kita sepanjang `len` byte, rata 8.
            let r = unsafe { PerfQueryCounterData(self.0, buf.as_mut_ptr().cast(), len, &mut needed) };
            if r == 0 {
                // SAFETY: memandang buffer u64 yang sama sebagai byte.
                let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), (needed.min(len)) as usize) };
                return parse_multi_instance(bytes);
            }
            if r != ERROR_NOT_ENOUGH_MEMORY || needed <= len {
                break;
            }
            buf = vec![0u64; (needed as usize).div_ceil(8)];
        }
        Vec::new()
    }
}

impl Drop for CounterQuery {
    fn drop(&mut self) {
        // SAFETY: handle milik kita dari PerfOpenQueryHandle.
        unsafe {
            PerfCloseQueryHandle(self.0);
        }
    }
}

/// Zona termal ACPI terpanas.
struct Zones {
    query: CounterQuery,
    tenths: bool,
}

impl Zones {
    /// `None` bila mesin tidak punya zona termal yang memberi nilai.
    fn open() -> Option<Self> {
        if !CounterQuery::has_instances(&THERMAL_ZONE_SET) {
            return None;
        }
        // "High Precision" belum ada di Windows 10 lama; jatuh ke counter K bulat.
        ZONE_COUNTERS.iter().find_map(|&(id, tenths)| {
            let z = Zones { query: CounterQuery::open(&THERMAL_ZONE_SET, id)?, tenths };
            z.hottest().map(|_| z)
        })
    }

    fn hottest(&self) -> Option<f32> {
        self.query.values().into_iter().filter_map(|v| valid(kelvin_to_c(v, self.tenths))).reduce(f32::max)
    }
}

// ------------------------------------------------------------- CPU (Intel DTT)

/// GUID blok data WMI "EsifDeviceInformation" (root\wmi) dari driver Intel Dynamic Tuning.
const ESIF_GUID: GUID = GUID::from_u128(0xab27db29_db25_42e6_a3e7_28bd46bdb999);
const WMIGUID_QUERY: u32 = 0x0001;

type WmiOpenBlockFn = unsafe extern "system" fn(*const GUID, u32, *mut HANDLE) -> u32;
type WmiQueryAllDataWFn = unsafe extern "system" fn(HANDLE, *mut u32, *mut c_void) -> u32;
type WmiCloseBlockFn = unsafe extern "system" fn(HANDLE) -> u32;

/// Hasil membuka sumber suhu CPU Intel.
enum EsifOpen {
    Ok(Esif),
    /// Ada, tetapi driver hanya mengizinkan admin.
    Denied,
    Absent,
}

/// Blok data WMI ESIF yang terbuka. Fungsi `Wmi*` diambil dari advapi32 saat runtime karena
/// tidak ada di import library SDK maupun `windows-sys`.
struct Esif {
    handle: HANDLE,
    query: WmiQueryAllDataWFn,
    close: WmiCloseBlockFn,
    /// Buffer keluaran (u64 agar rata 8), dipakai ulang antar-pembacaan.
    buf: Vec<u64>,
}

impl Esif {
    fn open() -> EsifOpen {
        // SAFETY: advapi32 selalu termuat; tipe fungsi mengikuti deklarasi wmium.h.
        unsafe {
            let module = GetModuleHandleW(windows_sys::core::w!("advapi32.dll"));
            if module.is_null() {
                return EsifOpen::Absent;
            }
            let (Some(open), Some(query), Some(close)) = (
                GetProcAddress(module, windows_sys::core::s!("WmiOpenBlock")),
                GetProcAddress(module, windows_sys::core::s!("WmiQueryAllDataW")),
                GetProcAddress(module, windows_sys::core::s!("WmiCloseBlock")),
            ) else {
                return EsifOpen::Absent;
            };
            let open: WmiOpenBlockFn = std::mem::transmute(open);
            let mut handle: HANDLE = null_mut();
            match open(&ESIF_GUID, WMIGUID_QUERY, &mut handle) {
                0 => {}
                ERROR_ACCESS_DENIED => return EsifOpen::Denied,
                _ => return EsifOpen::Absent,
            }
            type FarProc = unsafe extern "system" fn() -> isize;
            let query = std::mem::transmute::<FarProc, WmiQueryAllDataWFn>(query);
            let close = std::mem::transmute::<FarProc, WmiCloseBlockFn>(close);
            let mut e = Esif { handle, query, close, buf: Vec::new() };
            if e.cpu().is_some() { EsifOpen::Ok(e) } else { EsifOpen::Absent }
        }
    }

    fn cpu(&mut self) -> Option<f32> {
        for _ in 0..3 {
            let mut size = (self.buf.len() * 8) as u32;
            // SAFETY: buffer milik kita sepanjang `size` byte; ukuran 0 hanya menanyakan kebutuhan.
            let r = unsafe { (self.query)(self.handle, &mut size, self.buf.as_mut_ptr().cast()) };
            if r == 0 {
                // SAFETY: memandang buffer u64 yang sama sebagai byte.
                let bytes = unsafe { std::slice::from_raw_parts(self.buf.as_ptr().cast::<u8>(), (size as usize).min(self.buf.len() * 8)) };
                return parse_esif_cpu(bytes);
            }
            if size as usize <= self.buf.len() * 8 {
                return None;
            }
            self.buf = vec![0u64; (size as usize).div_ceil(8)];
        }
        None
    }
}

impl Drop for Esif {
    fn drop(&mut self) {
        // SAFETY: handle milik kita dari WmiOpenBlock.
        unsafe {
            (self.close)(self.handle);
        }
    }
}

// ---------------------------------------------------------------- pemantau

/// Sumber suhu: SSD internal terpanas, dan CPU (Intel DTT bila boleh dibaca, selain itu zona
/// termal ACPI terpanas).
pub struct Thermal {
    drives: Drives,
    esif: Option<Esif>,
    /// Sensor CPU Intel ada tetapi hanya bisa dibaca admin.
    cpu_needs_admin: bool,
    zones: Option<Zones>,
    ssd: Option<f32>,
    cpu: Option<f32>,
    polls: u32,
}

impl Thermal {
    /// Membuka semua sumber lalu langsung membaca sekali.
    pub fn new() -> Self {
        let (esif, cpu_needs_admin) = match Esif::open() {
            EsifOpen::Ok(e) => (Some(e), false),
            EsifOpen::Denied => (None, true),
            EsifOpen::Absent => (None, false),
        };
        // Zona ACPI hanya dibuka bila sensor CPU Intel tidak bisa dipakai.
        let zones = if esif.is_some() { None } else { Zones::open() };
        let mut t = Thermal { drives: Drives::open(), esif, cpu_needs_admin, zones, ssd: None, cpu: None, polls: 0 };
        t.poll();
        t
    }

    /// Membaca ulang semua sumber.
    pub fn poll(&mut self) {
        self.polls = self.polls.wrapping_add(1);
        if self.polls.is_multiple_of(DRIVE_RESCAN_POLLS) {
            self.drives = Drives::open();
        }
        self.ssd = self.drives.hottest();
        self.cpu = match self.esif.as_mut() {
            Some(e) => e.cpu(),
            None => self.zones.as_ref().and_then(Zones::hottest),
        };
    }

    /// Suhu SSD internal terpanas (°C) pada pembacaan terakhir.
    pub fn ssd(&self) -> Option<f32> {
        self.ssd
    }

    /// Suhu CPU (°C) pada pembacaan terakhir: domain prosesor terpanas menurut Intel DTT, atau
    /// zona termal ACPI terpanas bila DTT tidak tersedia.
    pub fn cpu(&self) -> Option<f32> {
        self.cpu
    }

    /// `true` bila sensor CPU Intel ada tetapi proses ini tidak berjalan sebagai admin.
    pub fn cpu_needs_admin(&self) -> bool {
        self.cpu_needs_admin
    }

    /// `true` bila ada sumber yang memberi nilai.
    pub fn any(&self) -> bool {
        self.ssd.is_some() || self.cpu.is_some()
    }
}

impl Default for Thermal {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deskriptor sintetis: kepala 24 byte + satu entri 16 byte per suhu.
    fn descriptor(temps: &[i16]) -> Vec<u8> {
        let mut b = vec![0u8; 24 + temps.len() * 16];
        b[12..14].copy_from_slice(&(temps.len() as u16).to_le_bytes());
        for (i, t) in temps.iter().enumerate() {
            b[24 + i * 16..24 + i * 16 + 2].copy_from_slice(&(i as u16).to_le_bytes());
            b[24 + i * 16 + 2..24 + i * 16 + 4].copy_from_slice(&t.to_le_bytes());
        }
        b
    }

    #[test]
    fn storage_descriptor_takes_the_hottest_valid_sensor() {
        assert_eq!(parse_storage_temp(&descriptor(&[45])), Some(45.0));
        assert_eq!(parse_storage_temp(&descriptor(&[45, 61, 50])), Some(61.0));
        // 0, negatif, dan nilai sentinel dibuang
        assert_eq!(parse_storage_temp(&descriptor(&[0, -273, 32767, 38])), Some(38.0));
        assert_eq!(parse_storage_temp(&descriptor(&[0])), None);
        assert_eq!(parse_storage_temp(&descriptor(&[])), None);
    }

    #[test]
    fn storage_descriptor_from_a_real_nvme_drive() {
        // Keluaran asli NVMe (SanDisk SN5000S): Critical/Warning = 0x8000 (tidak diisi), satu sensor 38 °C.
        let b = [
            0x28, 0, 0, 0, 0x28, 0, 0, 0, 0x00, 0x80, 0x00, 0x80, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
            0, 0, 0x26, 0, 0x53, 0, 0, 0x80, 0x01, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(parse_storage_temp(&b), Some(38.0));
    }

    #[test]
    fn storage_descriptor_never_reads_past_the_buffer() {
        let mut b = descriptor(&[40, 70]);
        b.truncate(24 + 16 + 3); // entri kedua terpotong
        assert_eq!(parse_storage_temp(&b), Some(40.0));
        assert_eq!(parse_storage_temp(&b[..10]), None);
    }

    #[test]
    fn kelvin_conversions() {
        assert!((kelvin_to_c(3332, true) - 60.05).abs() < 0.01);
        assert!((kelvin_to_c(333, false) - 59.85).abs() < 0.01);
        assert_eq!(valid(kelvin_to_c(0, true)), None);
    }

    /// Keluaran PerfQueryCounterData sintetis: satu counter, beberapa instance bernama.
    fn perf_block(values: &[(u32, &[u8])]) -> Vec<u8> {
        let mut b = vec![0u8; 48];
        b[4..8].copy_from_slice(&1u32.to_le_bytes()); // dwNumCounters
        let ch = b.len();
        b.extend_from_slice(&[0; 16]);
        b[ch + 4..ch + 8].copy_from_slice(&(PERF_MULTIPLE_INSTANCES as u32).to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&(values.len() as u32).to_le_bytes());
        for (i, (size, val)) in values.iter().enumerate() {
            // nama 2 karakter + null = 6 byte → header instance 8 + 8 = 16
            b.extend_from_slice(&16u32.to_le_bytes());
            b.extend_from_slice(&(i as u32).to_le_bytes());
            b.extend_from_slice(&[b'T', 0, b'Z', 0, 0, 0, 0, 0]);
            b.extend_from_slice(&size.to_le_bytes());
            b.extend_from_slice(&16u32.to_le_bytes());
            let mut v = val.to_vec();
            v.resize(8, 0);
            b.extend_from_slice(&v);
        }
        b
    }

    #[test]
    fn multi_instance_block_yields_every_value() {
        let b = perf_block(&[(4, &3332u32.to_le_bytes()), (8, &3001u64.to_le_bytes())]);
        assert_eq!(parse_multi_instance(&b), vec![3332, 3001]);
        // terpotong di tengah instance kedua: hanya yang utuh
        assert_eq!(parse_multi_instance(&b[..b.len() - 20]), vec![3332]);
        assert!(parse_multi_instance(&b[..40]).is_empty());
    }

    #[test]
    fn live_perflib_query_reads_processor_information() {
        // Counter set lain yang pasti punya instance (per CPU + _Total) menguji jalur V2 yang sama.
        const PROCESSOR_INFORMATION: GUID = GUID::from_u128(0xb4fc721a_0378_476f_89ba_a5a79f810b36);
        assert!(CounterQuery::has_instances(&PROCESSOR_INFORMATION));
        let q = CounterQuery::open(&PROCESSOR_INFORMATION, 0).expect("open");
        assert!(q.values().len() >= 2);
    }

    /// Satu `WNODE_ALL_DATA` sintetis berformat seperti keluaran driver Intel: data per instance
    /// (offset, panjang) dan nama instance berawalan panjang byte.
    fn wnode(instances: &[(&str, u32)], linkage: u32) -> Vec<u8> {
        let n = instances.len();
        let names_off = 60 + n * 8;
        let mut b = vec![0u8; names_off + n * 4];
        let put = |b: &mut Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put(&mut b, 12, linkage);
        put(&mut b, 44, 0x0001_0081); // flag seperti driver asli: ALL_DATA, nama dinamis
        put(&mut b, 52, n as u32);
        put(&mut b, 56, names_off as u32);
        for (i, (name, temp)) in instances.iter().enumerate() {
            while !b.len().is_multiple_of(8) {
                b.push(0);
            }
            let data = b.len();
            put(&mut b, 60 + i * 8, data as u32);
            put(&mut b, 64 + i * 8, 68);
            b.extend_from_slice(&temp.to_le_bytes());
            b.extend_from_slice(&[0; 64]); // Power, trip point
            let name_at = b.len();
            put(&mut b, names_off + i * 4, name_at as u32);
            let w: Vec<u16> = name.encode_utf16().collect();
            b.extend_from_slice(&((w.len() * 2) as u16).to_le_bytes());
            w.iter().for_each(|c| b.extend_from_slice(&c.to_le_bytes()));
        }
        let size = b.len() as u32;
        put(&mut b, 0, size);
        b
    }

    #[test]
    fn esif_takes_the_hottest_processor_domain_only() {
        let cpu = "PCI\\VEN_8086&DEV_7D03&SUBSYS_0D631028&REV_05\\3&11583659&1&20";
        let b = wnode(
            &[
                (&format!("{cpu}_0"), 57),
                (&format!("{cpu}_1"), 58),
                (&format!("{cpu}_3"), 0), // domain tanpa sensor
                ("ACPI\\INTC1046\\2_0", 70), // sensor kulit: bukan CPU
            ],
            0,
        );
        assert_eq!(parse_esif_cpu(&b), Some(58.0));
        assert_eq!(parse_esif_cpu(&wnode(&[("ACPI\\INTC1046\\2_0", 70)], 0)), None);
        assert_eq!(parse_esif_cpu(&b[..40]), None);
    }

    #[test]
    fn esif_follows_the_wnode_chain() {
        let mut first = wnode(&[("pci\\ven_8086&dev_a71d_0", 61)], 0);
        while !first.len().is_multiple_of(8) {
            first.push(0);
        }
        let link = first.len() as u32;
        first[12..16].copy_from_slice(&link.to_le_bytes());
        first.extend(wnode(&[("PCI\\VEN_8086&DEV_A71D_1", 66)], 0));
        assert_eq!(parse_esif_cpu(&first), Some(66.0));
    }

    #[test]
    fn live_poll_is_sane() {
        let th = Thermal::new();
        for c in [th.ssd(), th.cpu()].into_iter().flatten() {
            assert!((1.0..=150.0).contains(&c), "{c}");
        }
        // sensor CPU Intel yang terbaca berarti tidak "butuh admin"
        assert!(!(th.esif.is_some() && th.cpu_needs_admin()));
    }
}
