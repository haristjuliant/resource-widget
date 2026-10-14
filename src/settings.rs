//! Pengaturan (`%APPDATA%\ResourceWidget\settings.ini`) dan autostart (HKCU Run).

use std::path::PathBuf;

use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};

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

#[cfg(test)]
mod tests {
    use super::*;

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
