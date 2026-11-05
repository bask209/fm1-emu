//! The activity's main loop: the window, the fingers, the firmware, the audio stream.

use crate::aaudio::Output;
use crate::emu::{self, Machine, Shared};
use crate::ui::{Tool, Ui, UiOut};
use crate::{midi, Settings};
use android_activity::input::{InputEvent, MotionAction};
use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent};
use fm1_soc::Action;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

#[repr(C)]
struct WindowBuffer {
    width: i32,
    height: i32,
    stride: i32,
    format: i32,
    bits: *mut c_void,
    reserved: [u32; 6],
}

#[link(name = "android")]
extern "C" {
    fn ANativeWindow_acquire(w: *mut c_void);
    fn ANativeWindow_release(w: *mut c_void);
    fn ANativeWindow_getWidth(w: *mut c_void) -> i32;
    fn ANativeWindow_getHeight(w: *mut c_void) -> i32;
    fn ANativeWindow_setBuffersGeometry(w: *mut c_void, width: i32, height: i32, format: i32) -> i32;
    fn ANativeWindow_lock(w: *mut c_void, out: *mut WindowBuffer, dirty: *mut c_void) -> i32;
    fn ANativeWindow_unlockAndPost(w: *mut c_void) -> i32;
}

const WINDOW_FORMAT_RGBX_8888: i32 = 2;
const MHZ: u64 = 240;

enum Ev {
    InitWindow,
    TermWindow,
    Resized,
    Pause,
    Resume,
    Save,
    Destroy,
}

struct Running {
    shared: Arc<Shared>,
    tx: mpsc::Sender<Action>,
    thread: Option<std::thread::JoinHandle<()>>,
    name: String,
    fm1: bool,
}

struct State {
    files: PathBuf,
    window: Option<*mut c_void>,
    ui: Option<Ui>,
    run: Option<Running>,
    audio: Option<Output>,
    audio_err: Option<String>,
    settings: Settings,
    jit: bool,
    paused: bool,
    picker_asked: bool,
    status: Vec<String>,
    last_status: Instant,
    last_log: Instant,
    last_draw: Instant,
    last_frame: u64,
    last_poll: Instant,
}

pub fn main(app: AndroidApp) {
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("fm1"));
    std::panic::set_hook(Box::new(|p| log::error!("panic: {p}")));
    let files = app.internal_data_path().unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    let settings = std::fs::read_to_string(files.join("settings.txt")).map(|s| Settings::parse(&s)).unwrap_or_default();
    let rwx = crate::sys::rwx_ok();
    log::info!("fm1-android {}: files in {}, JIT {} (read-write-execute memory {})", env!("CARGO_PKG_VERSION"), files.display(),
               if rwx { "on" } else { "off: the interpreter" }, if rwx { "allowed" } else { "refused" });
    log::info!("settings: {settings:?}");
    let mut st = State {
        files,
        window: None,
        ui: None,
        run: None,
        audio: None,
        audio_err: None,
        settings,
        jit: rwx && std::env::var_os("FM1_NO_JIT").is_none(),
        paused: false,
        picker_asked: false,
        status: Vec::new(),
        last_status: Instant::now() - Duration::from_secs(1),
        last_log: Instant::now(),
        last_draw: Instant::now(),
        last_frame: 0,
        last_poll: Instant::now(),
    };
    st.boot_saved();
    let mut quit = false;
    let mut outs = Vec::new();
    while !quit {
        let mut evs = Vec::new();
        app.poll_events(Some(Duration::from_millis(4)), |e| {
            if let PollEvent::Main(m) = e {
                match m {
                    MainEvent::InitWindow { .. } => evs.push(Ev::InitWindow),
                    MainEvent::TerminateWindow { .. } => evs.push(Ev::TermWindow),
                    MainEvent::WindowResized { .. } | MainEvent::ConfigChanged { .. } | MainEvent::ContentRectChanged { .. } => evs.push(Ev::Resized),
                    MainEvent::Pause => evs.push(Ev::Pause),
                    MainEvent::Resume { .. } => evs.push(Ev::Resume),
                    MainEvent::SaveState { .. } | MainEvent::Stop => evs.push(Ev::Save),
                    MainEvent::Destroy => evs.push(Ev::Destroy),
                    _ => {}
                }
            }
        });
        for e in evs {
            match e {
                Ev::InitWindow => {
                    if let Some(w) = app.native_window() {
                        let p = w.ptr().as_ptr() as *mut c_void;
                        // SAFETY: our own reference to the window, released on TerminateWindow
                        unsafe { ANativeWindow_acquire(p) };
                        st.window = Some(p);
                        st.relayout();
                    }
                    if st.run.is_none() && !st.picker_asked {
                        st.picker_asked = true;
                        pick_firmware(&app);
                    }
                }
                Ev::TermWindow => {
                    if let Some(p) = st.window.take() {
                        // SAFETY: the reference taken at InitWindow
                        unsafe { ANativeWindow_release(p) };
                    }
                }
                Ev::Resized => st.relayout(),
                Ev::Pause => st.pause(),
                Ev::Resume => st.resume(),
                Ev::Save => {
                    if let Some(r) = &st.run {
                        r.shared.save.store(true, Ordering::Relaxed);
                    }
                }
                Ev::Destroy => quit = true,
            }
        }
        // the fingers
        if let Some(ui) = st.ui.as_mut() {
            if let Ok(mut iter) = app.input_events_iter() {
                loop {
                    let more = iter.next(|ev| {
                        let InputEvent::MotionEvent(m) = ev else { return InputStatus::Unhandled };
                        let idx = m.pointer_index();
                        match m.action() {
                            MotionAction::Down | MotionAction::PointerDown => {
                                let p = m.pointer_at_index(idx);
                                ui.pointer_down(p.pointer_id(), p.x(), p.y(), &mut outs);
                            }
                            MotionAction::Move => {
                                for p in m.pointers() {
                                    ui.pointer_move(p.pointer_id(), p.x(), p.y(), &mut outs);
                                }
                            }
                            MotionAction::Up | MotionAction::PointerUp => {
                                let p = m.pointer_at_index(idx);
                                ui.pointer_up(p.pointer_id(), &mut outs);
                            }
                            MotionAction::Cancel => ui.cancel(&mut outs),
                            _ => {}
                        }
                        InputStatus::Handled
                    });
                    if !more {
                        break;
                    }
                }
            }
            ui.tick(&mut outs);
        }
        for o in outs.drain(..) {
            match o {
                UiOut::Act(a) => {
                    if let Action::Key { id, down } = a {
                        log::info!("touch: key {id} {}", if down { "down" } else { "up" });
                    }
                    if let Some(r) = &st.run {
                        let _ = r.tx.send(a);
                    }
                }
                UiOut::Tool(t) => st.tool(t, &app),
            }
        }
        st.service();
        st.draw();
    }
    st.stop_machine();
    if let Some(p) = st.window.take() {
        // SAFETY: the reference taken at InitWindow
        unsafe { ANativeWindow_release(p) };
    }
}

/// The activity's file picker (MainActivity.pickFirmware); what is picked lands in firmware.new.
fn pick_firmware(app: &AndroidApp) {
    let r = (|| -> Result<(), jni::errors::Error> {
        // SAFETY: the VM and the activity android-activity holds for the app's life
        let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr() as *mut jni::sys::JavaVM)? };
        let mut env = vm.attach_current_thread()?;
        let act = unsafe { jni::objects::JObject::from_raw(app.activity_as_ptr() as jni::sys::jobject) };
        env.call_method(&act, "pickFirmware", "()V", &[])?;
        Ok(())
    })();
    if let Err(e) = r {
        log::error!("file picker: {e}");
    }
}

impl State {
    fn path(&self, name: &str) -> PathBuf {
        self.files.join(name)
    }

    fn save_settings(&self) {
        let _ = std::fs::write(self.path("settings.txt"), self.settings.to_text());
    }

    /// The firmware booted last time, if there is one.
    fn boot_saved(&mut self) {
        if self.path("firmware.fwsc").exists() {
            self.start_machine();
        } else {
            log::info!("no firmware yet: the file picker opens");
        }
    }

    fn start_machine(&mut self) {
        self.stop_machine();
        let name = std::fs::read_to_string(self.path("firmware.name")).map(|s| s.trim().trim_end_matches(".fwsc").to_string()).unwrap_or_else(|_| "firmware".into());
        let pkg = match std::fs::read(self.path("firmware.fwsc")) {
            Ok(p) => p,
            Err(e) => return log::error!("firmware.fwsc: {e}"),
        };
        let flash = std::fs::read(self.path("flash.bin")).ok();
        let t = Instant::now();
        let m = match Machine::from_package(&pkg, flash.as_deref(), MHZ, self.jit) {
            Ok(m) => m,
            Err(e) => {
                log::error!("{name}: {e}");
                self.status_note(format!("NO GO {e}"));
                return;
            }
        };
        log::info!("booting {name} ({} bytes{}, board {:?}) in {:.0} ms", pkg.len(),
                   if flash.is_some() { ", its saved flash" } else { "" }, m.soc.board, t.elapsed().as_secs_f64() * 1000.0);
        let fm1 = m.is_fm1();
        let pot = m.soc.periph.adc_value[4];
        if let Some(ui) = self.ui.as_mut() {
            ui.external(&[0; 7], Some(pot));
        }
        let shared = Arc::new(Shared::new(self.settings.floor_ms));
        let (tx, rx) = mpsc::channel::<Action>();
        midi::set_machine(tx.clone());
        let (sh, flash_path) = (shared.clone(), self.path("flash.bin"));
        let thread = std::thread::Builder::new()
            .name("fm1-emu".into())
            .spawn(move || emu::run_thread(m, sh, rx, flash_path))
            .ok();
        self.run = Some(Running { shared, tx, thread, name, fm1 });
        self.open_audio();
    }

    fn stop_machine(&mut self) {
        self.audio = None;
        if let Some(mut r) = self.run.take() {
            r.shared.quit.store(true, Ordering::Relaxed);
            if let Some(t) = r.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn open_audio(&mut self) {
        self.audio = None;
        let Some(r) = &self.run else { return };
        r.shared.audio_on.store(false, Ordering::Relaxed);
        let ring = r.shared.ring.clone();
        let mut out = Output::open(ring.clone(), self.settings.bursts, self.settings.exclusive);
        if out.is_err() && self.settings.exclusive {
            out = Output::open(ring, self.settings.bursts, false);
        }
        match out.and_then(|mut o| o.start().map(|_| o)) {
            Ok(o) => {
                self.audio_err = None;
                if self.paused {
                    let mut o = o;
                    o.stop();
                    self.audio = Some(o);
                } else {
                    self.audio = Some(o);
                    r.shared.audio_on.store(true, Ordering::Relaxed);
                }
            }
            Err(e) => {
                log::error!("audio off: {e}");
                self.audio_err = Some(e);
            }
        }
    }

    fn pause(&mut self) {
        self.paused = true;
        if let Some(r) = &self.run {
            r.shared.save.store(true, Ordering::Relaxed);
            r.shared.paused.store(true, Ordering::Relaxed);
            r.shared.audio_on.store(false, Ordering::Relaxed);
        }
        if let Some(a) = self.audio.as_mut() {
            a.stop();
        }
    }

    fn resume(&mut self) {
        self.paused = false;
        if let Some(r) = &self.run {
            r.shared.ring.flush.store(true, Ordering::Relaxed);
            r.shared.paused.store(false, Ordering::Relaxed);
            match self.audio.as_mut().map(|a| a.start()) {
                Some(Ok(())) => r.shared.audio_on.store(true, Ordering::Relaxed),
                Some(Err(e)) => log::warn!("audio: {e}"),
                None => {}
            }
        }
        if self.audio.is_none() && self.run.is_some() {
            self.open_audio();
        }
    }

    fn tool(&mut self, t: Tool, app: &AndroidApp) {
        match t {
            Tool::Open => pick_firmware(app),
            Tool::LatDown | Tool::LatUp => {
                let d = if t == Tool::LatUp { 1.0 } else { -1.0 };
                self.settings.floor_ms = (self.settings.floor_ms + d).clamp(1.0, 60.0);
                if let Some(r) = &self.run {
                    r.shared.set_floor(self.settings.floor_ms);
                }
                log::info!("queue floor {} ms", self.settings.floor_ms);
                self.save_settings();
            }
            Tool::BufDown | Tool::BufUp => {
                let n = if t == Tool::BufUp { self.settings.bursts + 1 } else { self.settings.bursts.saturating_sub(1) };
                self.settings.bursts = n.clamp(1, 16);
                if let Some(a) = self.audio.as_mut() {
                    a.min_bursts = self.settings.bursts;
                    a.set_bursts(self.settings.bursts);
                    log::info!("device buffer {} frames ({} bursts)", a.buffer(), self.settings.bursts);
                }
                self.save_settings();
            }
        }
        self.last_status = Instant::now() - Duration::from_secs(1);
    }

    fn relayout(&mut self) {
        let Some(p) = self.window else { return };
        // SAFETY: a window we hold a reference to; geometry 0 x 0 = the window's own size
        let (w, h) = unsafe {
            ANativeWindow_setBuffersGeometry(p, 0, 0, 0);
            (ANativeWindow_getWidth(p).max(1) as u32, ANativeWindow_getHeight(p).max(1) as u32)
        };
        let pot = 512;
        match self.ui.as_mut() {
            Some(ui) => ui.resize(w, h),
            None => self.ui = Some(Ui::new(w, h, pot)),
        }
        let ui = self.ui.as_ref().unwrap();
        log::info!("window {w} x {h}: canvas {} x {} (panel at {}, {}), bar {}", ui.layout.cw, ui.layout.ch, ui.layout.ox, ui.layout.oy,
                   if ui.layout.side { "beside" } else { "above" });
        // SAFETY: as above; the compositor scales the buffers to the window
        unsafe { ANativeWindow_setBuffersGeometry(p, ui.layout.cw as i32, ui.layout.ch as i32, WINDOW_FORMAT_RGBX_8888) };
    }

    fn status_note(&mut self, s: String) {
        self.status = vec![s];
    }

    /// A new firmware from the picker, the audio stream's health, MIDI, the status lines.
    fn service(&mut self) {
        if self.last_poll.elapsed() >= Duration::from_millis(250) {
            self.last_poll = Instant::now();
            let new = self.path("firmware.new");
            if new.exists() {
                self.stop_machine();
                let _ = std::fs::rename(&new, self.path("firmware.fwsc"));
                let _ = std::fs::rename(self.path("firmware.new.name"), self.path("firmware.name"));
                // a new firmware starts from an empty flash, as a fresh install does
                let _ = std::fs::remove_file(self.path("flash.bin"));
                self.start_machine();
            }
        }
        if self.audio.as_ref().is_some_and(|a| a.lost.load(Ordering::Acquire)) {
            log::warn!("audio: the stream was lost (a device change?); a new one");
            self.open_audio();
        }
        if let Some(a) = self.audio.as_mut() {
            a.tune();
        }
        // what a MIDI controller did, for the drawing
        let (enc, pot) = midi::with(|m| (std::mem::take(&mut m.enc), m.pot.take()));
        if let Some(ui) = self.ui.as_mut() {
            ui.external(&enc, pot);
        }
        if self.last_status.elapsed() >= Duration::from_millis(250) {
            self.last_status = Instant::now();
            self.status = self.status_lines();
            if let Some(ui) = self.ui.as_mut() {
                ui.dirty = true;
            }
            if self.last_log.elapsed() >= Duration::from_secs(2) {
                self.last_log = Instant::now();
                log::info!("status: {}", self.status.join(" | "));
            }
        }
    }

    fn status_lines(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        let Some(r) = &self.run else {
            return vec!["NO FIRMWARE".into(), "TAP OPEN TO PICK".into(), "A .FWSC FILE".into(), "".into(), "OR HOLD THE SCREEN".into()];
        };
        let s = r.shared.status.lock().unwrap().clone();
        v.push(r.name.to_uppercase().chars().take(20).collect());
        if let Some(h) = &s.halted {
            v.push("HALTED".into());
            v.push(h.chars().take(20).collect());
        }
        if !r.fm1 {
            v.push("NO SMK-37 PANEL".into());
        }
        v.push(format!("SPEED {:.2}X", s.speed));
        let queued = r.shared.ring.ms();
        let device = self.audio.as_ref().and_then(|a| a.latency_ms());
        match (&self.audio, device) {
            (Some(a), dev) => {
                let dev = dev.unwrap_or(a.buffer() as f64 * 1000.0 / a.rate as f64);
                v.push(format!("OUT {:.0} MS", queued + dev));
                v.push(format!("QUEUE {:.1}/{:.0}", queued, r.shared.target()));
                v.push(format!("DEVICE {dev:.1} MS"));
                v.push(format!("BUF {}X{} {}", a.buffer().div_ceil(a.burst), a.burst, a.rate));
                v.push(format!("{} {}", if a.exclusive { "EXCLUSIVE" } else { "SHARED" }, if a.low_latency { "FAST" } else { "SLOW" }));
                v.push(format!("XRUN {} UNDER {}", a.xruns(), r.shared.ring.underruns.load(Ordering::Relaxed)));
            }
            (None, _) => {
                v.push("NO AUDIO".into());
                if let Some(e) = &self.audio_err {
                    v.push(e.chars().take(20).collect());
                }
            }
        }
        v.push(format!("FLOOR {:.0} MS {}B", self.settings.floor_ms, self.settings.bursts));
        v.push(format!("JIT {}", if self.jit { "ON" } else { "OFF" }));
        let (devs, msgs) = midi::with(|m| (m.devices.clone(), m.messages));
        v.push(match devs.first() {
            Some(d) => format!("MIDI {}", d.to_uppercase().chars().take(15).collect::<String>()),
            None => "MIDI NONE".into(),
        });
        if msgs > 0 {
            v.push(format!("MIDI IN {msgs}"));
        }
        if s.peak > 0 {
            v.push(format!("PEAK {}", s.peak));
        }
        let side = self.ui.as_ref().is_none_or(|u| u.layout.side);
        if !side {
            // one bar above the panel: three lines
            let per = v.len().div_ceil(3);
            return v.chunks(per.max(1)).map(|c| c.join("  ")).collect();
        }
        v
    }

    fn draw(&mut self) {
        let (Some(p), Some(ui)) = (self.window, self.ui.as_mut()) else { return };
        let frame = self.run.as_ref().map_or(0, |r| r.shared.frame.load(Ordering::Acquire));
        let changed = frame != self.last_frame || ui.dirty || ui.animating();
        if !changed || self.last_draw.elapsed() < Duration::from_millis(15) {
            return;
        }
        self.last_draw = Instant::now();
        self.last_frame = frame;
        let midi_down = midi::with(|m| m.down);
        match &self.run {
            Some(r) => {
                let lcd = r.shared.lcd.lock().unwrap().clone();
                let leds = *r.shared.leds.lock().unwrap();
                ui.draw(&lcd, &leds, &midi_down, &self.status);
            }
            None => ui.draw(&vec![0u16; 240 * 240], &[0.0; 41], &midi_down, &self.status),
        }
        let (cw, ch) = (ui.layout.cw, ui.layout.ch);
        let mut b = WindowBuffer { width: 0, height: 0, stride: 0, format: 0, bits: std::ptr::null_mut(), reserved: [0; 6] };
        // SAFETY: the locked buffer is ours until unlockAndPost; written inside its stride and height
        unsafe {
            if ANativeWindow_lock(p, &mut b, std::ptr::null_mut()) != 0 {
                return;
            }
            if b.width as usize == cw && b.height as usize == ch && (b.format == 1 || b.format == 2) && !b.bits.is_null() {
                for y in 0..ch {
                    let dst = std::slice::from_raw_parts_mut((b.bits as *mut u32).add(y * b.stride as usize), cw);
                    for (d, &s) in dst.iter_mut().zip(&ui.canvas[y * cw..(y + 1) * cw]) {
                        // 0x00RRGGBB -> bytes R G B X
                        *d = 0xff00_0000 | (s & 0xff) << 16 | (s & 0xff00) | (s >> 16) & 0xff;
                    }
                }
            } else {
                log::warn!("window buffer {} x {} (format {}), canvas {cw} x {ch}: relayout", b.width, b.height, b.format);
                ANativeWindow_unlockAndPost(p);
                self.relayout();
                return;
            }
            ANativeWindow_unlockAndPost(p);
        }
    }
}
