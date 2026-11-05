//! The FM-1 emulator as an Android app (a NativeActivity: android/ builds the APK around it).
//!
//! The emulator runs on its own thread (emu.rs, fm1-live's loop), on the big cores at audio
//! priority; its sound goes through a lock-free queue (ring.rs) to an AAudio stream (aaudio.rs).
//! The UI thread draws fm1-live's panel (panel.rs) into the window's buffer, which the
//! compositor scales to the screen, and turns fingers into panel actions (ui.rs). A small Java
//! activity (android/app/src/main/java) opens the file picker and the USB MIDI devices; their
//! bytes come here through JNI (midi.rs).
//!
//! Files (the app's own folder): firmware.fwsc (the one booted), firmware.name, flash.bin (the
//! flash chip as the firmware left it), settings.txt (the latency settings).

pub mod emu;
pub mod midi;
#[path = "../../fm1-live/src/panel.rs"]
#[allow(dead_code)]
pub mod panel;
pub mod ring;
pub mod sys;
pub mod ui;

#[cfg(target_os = "android")]
mod aaudio;
#[cfg(target_os = "android")]
mod app;

/// The latency settings, kept in settings.txt.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    /// the emulator's queue target floor (ms)
    pub floor_ms: f64,
    /// the device buffer's floor, in bursts
    pub bursts: u32,
    /// ask for an exclusive (MMAP) stream
    pub exclusive: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { floor_ms: 6.0, bursts: 2, exclusive: true }
    }
}

impl Settings {
    pub fn parse(s: &str) -> Settings {
        let mut o = Settings::default();
        for line in s.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "floor_ms" => o.floor_ms = v.trim().parse().unwrap_or(o.floor_ms),
                "bursts" => o.bursts = v.trim().parse().unwrap_or(o.bursts),
                "exclusive" => o.exclusive = v.trim() != "0" && v.trim() != "false",
                _ => {}
            }
        }
        o.floor_ms = o.floor_ms.clamp(1.0, 60.0);
        o.bursts = o.bursts.clamp(1, 16);
        o
    }

    pub fn to_text(&self) -> String {
        format!("floor_ms={}\nbursts={}\nexclusive={}\n", self.floor_ms, self.bursts, self.exclusive as u8)
    }
}

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: android_activity::AndroidApp) {
    app::main(app);
}

/// MIDI bytes from the activity's MidiReceiver (one call per byte).
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_github_bask209_fm1emu_MainActivity_nativeMidi(_env: jni::JNIEnv, _cls: jni::objects::JClass, b: jni::sys::jint) {
    midi::byte(b as u8);
}

/// A MIDI device the activity opened, by name.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_github_bask209_fm1emu_MainActivity_nativeMidiDevice(mut env: jni::JNIEnv, _cls: jni::objects::JClass, name: jni::objects::JString) {
    let n: String = env.get_string(&name).map(|s| s.into()).unwrap_or_default();
    midi::device(&n);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let s = Settings { floor_ms: 9.0, bursts: 3, exclusive: false };
        assert_eq!(Settings::parse(&s.to_text()), s);
        assert_eq!(Settings::parse("bursts=99\nfloor_ms=x"), Settings { bursts: 16, ..Settings::default() });
    }
}
