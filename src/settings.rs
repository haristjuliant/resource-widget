//! Pengaturan (`%APPDATA%\ResourceWidget\settings.ini`) dan autostart (HKCU Run).

use std::path::PathBuf;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, PROCESS_INFORMATION,
    STARTUPINFOW, WaitForSingleObject,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use crate::metrics::AdapterSel;
use crate::wide;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "ResourceWidget";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Posisi pojok kiri atas jendela (piksel layar); `None` = posisi awal default.
    pub pos: Option<(i32, i32)>,
    /// Opasitas 20..=100 (persen).
    pub opacity: u8,
    /// `true` = bit (Kbps/Mbps), `false` = byte (KB/s, MB/s).
    pub bits: bool,
    pub adapter: AdapterSel,
    pub topmost: bool,
    /// Skala ukuran kartu dalam persen (`SCALE_MIN..=SCALE_MAX`), di atas skala DPI.
    pub scale: u32,
    /// Selang update dalam milidetik (`INTERVAL_MIN..=INTERVAL_MAX`).
    pub interval_ms: u32,
    /// Tampilkan proses teratas tiap metrik (butuh pemindaian proses berkala yang paling mahal).
    pub procs: bool,
    /// Mode ringkas: tanpa grafik dan kolom proses (kartu jadi lebih sempit).
    pub compact: bool,
    /// Suhu CPU dan SSD di baris masing-masing; hanya tampil bila ada sensor yang terbaca.
    pub temps: bool,
}

pub const SCALE_MIN: u32 = 50;
pub const SCALE_MAX: u32 = 300;
pub const INTERVAL_MIN: u32 = 250;
pub const INTERVAL_MAX: u32 = 10_000;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            pos: None,
            opacity: 90,
            bits: false,
            adapter: AdapterSel::Auto,
            topmost: true,
            scale: 100,
            interval_ms: 1000,
            procs: true,
            compact: false,
            temps: true,
        }
    }
}

/// Folder data aplikasi: `%APPDATA%\ResourceWidget`.
pub fn data_dir() -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os("APPDATA")?);
    p.push("ResourceWidget");
    Some(p)
}

impl Settings {
    pub fn parse(text: &str) -> Self {
        let mut s = Settings::default();
        let (mut x, mut y) = (None, None);
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "x" => x = v.parse().ok(),
                "y" => y = v.parse().ok(),
                "opacity" => {
                    if let Ok(o) = v.parse::<u8>() {
                        s.opacity = o.clamp(20, 100);
                    }
                }
                "units" => s.bits = v == "bits",
                "adapter" => {
                    s.adapter = if v.is_empty() || v == "auto" {
                        AdapterSel::Auto
                    } else {
                        AdapterSel::Named(v.to_string())
                    }
                }
                "topmost" => s.topmost = v != "0",
                "procs" => s.procs = v != "0",
                "compact" => s.compact = v == "1",
                "temps" => s.temps = v != "0",
                "scale" => {
                    if let Ok(p) = v.parse::<u32>() {
                        s.scale = p.clamp(SCALE_MIN, SCALE_MAX);
                    }
                }
                "interval" => {
                    if let Ok(ms) = v.parse::<u32>() {
                        s.interval_ms = ms.clamp(INTERVAL_MIN, INTERVAL_MAX);
                    }
                }
                _ => {}
            }
        }
        s.pos = x.zip(y);
        s
    }

    pub fn serialize(&self) -> String {
        let mut out = String::new();
        if let Some((x, y)) = self.pos {
            out += &format!("x={x}\ny={y}\n");
        }
        out += &format!("opacity={}\n", self.opacity);
        out += if self.bits { "units=bits\n" } else { "units=bytes\n" };
        match &self.adapter {
            AdapterSel::Auto => out += "adapter=auto\n",
            AdapterSel::Named(n) => out += &format!("adapter={n}\n"),
        }
        out += &format!("topmost={}\n", self.topmost as u8);
        out += &format!("scale={}\n", self.scale);
        out += &format!("interval={}\n", self.interval_ms);
        out += &format!("procs={}\n", self.procs as u8);
        out += &format!("compact={}\n", self.compact as u8);
        out += &format!("temps={}\n", self.temps as u8);
        out
    }

    fn path() -> Option<PathBuf> {
        Some(data_dir()?.join("settings.ini"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| Self::parse(&t))
            .unwrap_or_default()
    }

    /// Kegagalan menulis diabaikan: pengaturan hanya kenyamanan.
    pub fn save(&self) {
        let Some(p) = Self::path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, self.serialize());
    }
}

pub fn autostart_enabled() -> bool {
    autostart_enabled_for(RUN_VALUE)
}

/// Menyalakan/mematikan autostart untuk exe yang sedang berjalan.
pub fn set_autostart(on: bool) {
    set_autostart_for(RUN_VALUE, on)
}

fn autostart_enabled_for(value_name: &str) -> bool {
    let (key, name) = (wide(RUN_KEY), wide(value_name));
    // SAFETY: string null-terminated; buffer output null berarti hanya cek keberadaan.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) == 0
    }
}

fn set_autostart_for(value_name: &str, on: bool) {
    let (key, name) = (wide(RUN_KEY), wide(value_name));
    // SAFETY: semua pointer menunjuk buffer null-terminated yang hidup selama pemanggilan.
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let value = wide(&format!("\"{}\"", exe.display()));
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            );
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr());
        }
    }
}

// ------------------------------------------------- autostart sebagai admin

/// Nama tugas Task Scheduler untuk autostart sebagai admin.
const TASK_NAME: &str = "ResourceWidget";
/// Argumen yang diberikan tugas: instance baru menunggu instance lama keluar (lihat `main`).
pub const HANDOFF_ARG: &str = "--handoff";

/// `true` bila proses ini berjalan dengan token admin (elevated).
pub fn is_elevated() -> bool {
    // SAFETY: handle token milik kita dan ditutup; buffer keluaran sebesar TOKEN_ELEVATION.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// Menjalankan `schtasks.exe <args>` tanpa jendela konsol; `true` bila berhasil (exit code 0).
/// Langsung lewat `CreateProcessW`: `std::process::Command` menambah ±85 KB ke exe.
fn schtasks(args: &str) -> bool {
    let mut cmd = wide(&format!("schtasks.exe {args}"));
    // SAFETY: `cmd` null-terminated dan boleh ditulis (syarat CreateProcessW); handle ditutup.
    unsafe {
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NO_WINDOW,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        );
        if ok == 0 {
            return false;
        }
        WaitForSingleObject(pi.hProcess, 10_000);
        let mut code = 1u32;
        GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        code == 0
    }
}

/// Menjalankan `schtasks.exe` sebagai admin (prompt UAC bila proses ini belum admin) lalu
/// menunggu selesai. `false` bila prompt dibatalkan atau perintahnya gagal.
fn schtasks_elevated(params: &str) -> bool {
    type ShellExecuteExWFn = unsafe extern "system" fn(*mut SHELLEXECUTEINFOW) -> i32;
    let (verb, file, params) = (wide("runas"), wide("schtasks.exe"), wide(params));
    // SAFETY: shell32 dimuat hanya di sini (impor statis akan memuatnya, beserta rantai
    // dependensinya, di setiap start); string null-terminated hidup selama pemanggilan.
    unsafe {
        let shell32 = LoadLibraryW(windows_sys::core::w!("shell32.dll"));
        if shell32.is_null() {
            return false;
        }
        let Some(f) = GetProcAddress(shell32, windows_sys::core::s!("ShellExecuteExW")) else { return false };
        let shell_execute: ShellExecuteExWFn = std::mem::transmute(f);
        let mut sei: SHELLEXECUTEINFOW = std::mem::zeroed();
        sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOCLOSEPROCESS;
        sei.lpVerb = verb.as_ptr();
        sei.lpFile = file.as_ptr();
        sei.lpParameters = params.as_ptr();
        sei.nShow = SW_HIDE;
        if shell_execute(&mut sei) == 0 || sei.hProcess.is_null() {
            return false;
        }
        WaitForSingleObject(sei.hProcess, 30_000);
        let mut code = 1u32;
        GetExitCodeProcess(sei.hProcess, &mut code);
        CloseHandle(sei.hProcess);
        code == 0
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Definisi tugas: saat `user` logon, jalankan `exe` dengan hak tertinggi, juga saat memakai
/// baterai dan tanpa batas waktu (bawaan `schtasks` menghentikan tugas setelah 3 hari dan tidak
/// menjalankannya saat memakai baterai).
pub fn task_xml(user: &str, exe: &str) -> String {
    let (user, exe) = (xml_escape(user), xml_escape(exe));
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Resource Widget (admin, untuk suhu CPU)</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>"{exe}"</Command><Arguments>{HANDOFF_ARG}</Arguments></Exec></Actions>
</Task>
"#
    )
}

/// `true` bila tugas autostart-sebagai-admin terdaftar (±40 ms; dipanggil saat menu dibuka).
pub fn admin_autostart_enabled() -> bool {
    schtasks(&format!("/Query /TN {TASK_NAME}"))
}

/// Mendaftarkan/menghapus tugas autostart sebagai admin untuk exe yang sedang berjalan.
/// Memunculkan prompt UAC bila proses ini belum admin; `false` bila dibatalkan atau gagal.
pub fn set_admin_autostart(on: bool) -> bool {
    if !on {
        return schtasks_elevated(&format!("/Delete /TN {TASK_NAME} /F"));
    }
    let Ok(exe) = std::env::current_exe() else { return false };
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(d), Ok(u)) => format!("{d}\\{u}"),
        _ => return false,
    };
    // schtasks membaca XML sebagai UTF-16 LE dengan BOM.
    let path = std::env::temp_dir().join("ResourceWidget-task.xml");
    let bytes: Vec<u8> = std::iter::once(0xFEFFu16)
        .chain(task_xml(&user, &exe.display().to_string()).encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect();
    if std::fs::write(&path, bytes).is_err() {
        return false;
    }
    let ok = schtasks_elevated(&format!("/Create /TN {TASK_NAME} /XML \"{}\" /F", path.display()));
    let _ = std::fs::remove_file(&path);
    ok
}

/// Menjalankan tugas sekarang (tanpa prompt UAC: tugas sudah terdaftar dengan hak tertinggi).
pub fn run_admin_task() -> bool {
    schtasks(&format!("/Run /TN {TASK_NAME}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schtasks_reports_the_exit_code() {
        assert!(schtasks("/?"));
        assert!(!schtasks("/Query /TN ResourceWidgetNoSuchTask"));
    }

    #[test]
    fn task_xml_runs_elevated_on_logon_without_power_or_time_limits() {
        let x = task_xml("PC\\budi & co", "C:\\Tools\\R&D <x>\\resource-widget.exe");
        assert!(x.contains("<UserId>PC\\budi &amp; co</UserId>"));
        assert!(x.contains("<Command>\"C:\\Tools\\R&amp;D &lt;x&gt;\\resource-widget.exe\"</Command>"));
        assert!(x.contains("<Arguments>--handoff</Arguments>"));
        assert!(x.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(x.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(x.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(x.contains("<LogonTrigger>"));
    }

    #[test]
    fn roundtrip() {
        let s = Settings {
            pos: Some((-1200, 40)),
            opacity: 70,
            bits: true,
            adapter: AdapterSel::Named("Wi-Fi".into()),
            topmost: false,
            scale: 150,
            interval_ms: 2000,
            procs: false,
            compact: true,
            temps: false,
        };
        assert_eq!(Settings::parse(&s.serialize()), s);
    }

    #[test]
    fn garbage_and_missing_keys_fall_back_to_defaults() {
        // nilai tak terbaca (termasuk 999 yang tidak muat di u8) diabaikan
        assert_eq!(
            Settings::parse("???\n=\nx=abc\nopacity=999\nunknown=1"),
            Settings::default()
        );
        assert_eq!(Settings::parse(""), Settings::default());
        // posisi hanya berlaku jika x dan y valid
        assert_eq!(Settings::parse("x=5").pos, None);
        // file lama tanpa kunci `temps`: baris suhu tetap menyala
        assert!(Settings::parse("procs=1\ncompact=0").temps);
        assert!(!Settings::parse("temps=0").temps);
    }

    /// Menghapus nilai tes dari registry walau tes gagal di tengah jalan.
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            set_autostart_for(&self.0, false);
        }
    }

    #[test]
    fn autostart_registry_roundtrip() {
        let name = format!("ResourceWidgetTest{}", std::process::id());
        let _cleanup = Cleanup(name.clone());
        assert!(!autostart_enabled_for(&name));
        set_autostart_for(&name, true);
        assert!(autostart_enabled_for(&name));
        set_autostart_for(&name, false);
        assert!(!autostart_enabled_for(&name));
    }

    #[test]
    fn opacity_is_clamped() {
        assert_eq!(Settings::parse("opacity=5").opacity, 20);
        assert_eq!(Settings::parse("opacity=150").opacity, 100);
        assert_eq!(Settings::parse("scale=10").scale, SCALE_MIN);
        assert_eq!(Settings::parse("scale=9999").scale, SCALE_MAX);
        assert_eq!(Settings::parse("scale=abc").scale, 100);
        assert_eq!(Settings::parse("interval=1").interval_ms, INTERVAL_MIN);
        assert_eq!(Settings::parse("interval=999999").interval_ms, INTERVAL_MAX);
        assert_eq!(Settings::parse("interval=x").interval_ms, 1000);
    }
}
