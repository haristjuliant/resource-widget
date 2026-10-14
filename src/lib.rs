//! Resource Widget: pustaka metrik, render GDI, pengaturan, dan statistik pemakaian.

pub mod metrics;
pub mod render;
pub mod settings;
pub mod usage;

/// String UTF-16 ber-null-terminator untuk API Win32 `...W`.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
