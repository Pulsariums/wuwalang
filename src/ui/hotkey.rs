use std::sync::mpsc::Sender;

use eframe::egui;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyEvent {
    Toggle,
    Failed(String),
}

#[cfg(windows)]
const VK_F7: u32 = 0x76;
#[cfg(windows)]
const HOTKEY_ID: i32 = 1;

#[cfg(windows)]
pub fn spawn(events: Sender<HotkeyEvent>, ctx: egui::Context) {
    let failure = events.clone();
    let spawned = std::thread::Builder::new()
        .name("wuwalang-hotkey".into())
        .spawn(move || listen(events, ctx));
    if let Err(error) = spawned {
        let _ = failure.send(HotkeyEvent::Failed(format!(
            "F7 dinleyicisi başlatılamadı: {error}"
        )));
    }
}

#[cfg(not(windows))]
pub fn spawn(_events: Sender<HotkeyEvent>, _ctx: egui::Context) {}

#[cfg(windows)]
fn listen(events: Sender<HotkeyEvent>, ctx: egui::Context) {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

    // SAFETY: RegisterHotKey/GetMessageW run on this thread with a null window (thread message queue);
    // msg is zero-initialized and only read after GetMessageW fills it.
    unsafe {
        if RegisterHotKey(std::ptr::null_mut(), HOTKEY_ID, 0, VK_F7) == 0 {
            let code = GetLastError();
            let _ = events.send(HotkeyEvent::Failed(format!(
                "F7 kısayolu kaydedilemedi (hata {code}). Başka bir program F7 kullanıyor olabilir."
            )));
            ctx.request_repaint();
            return;
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if msg.message == WM_HOTKEY && msg.wParam == HOTKEY_ID as usize {
                let _ = events.send(HotkeyEvent::Toggle);
                ctx.request_repaint();
            }
        }
        UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ID);
    }
}
