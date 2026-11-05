//! The machine and its thread: fm1-live's loop. Once the firmware streams audio the queue sets
//! the pace: the emulator runs whenever less than the target is queued, and the target grows
//! at each underrun and comes back down after a calm spell (crates/fm1-live/src/main.rs).

use crate::ring::Ring;
use fm1_core::{Bus, Cpu};
use fm1_soc::{machine, Action, Soc};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

pub const RAM_BASE: u32 = 0x01c0_0000;
pub const RAM_SIZE: usize = 1 << 20;
/// How far emulated time runs ahead of the wall clock before audio starts.
const LEAD_MS: f64 = 60.0;
/// The queue target grows by this much at each underrun, up to AUDIO_TARGET_MAX_MS.
pub const AUDIO_TARGET_STEP_MS: f64 = 2.0;
pub const AUDIO_TARGET_MAX_MS: f64 = 80.0;
/// After this long without an underrun the target comes back down a step (no lower than the floor).
pub const AUDIO_TARGET_CALM: Duration = Duration::from_secs(15);
/// The most queued (250 ms); past it the emulator's samples are dropped.
pub const QUEUE_CAP: usize = 44_100 * 2 / 4;

/// The machine: cpu0 + SoC, and cpu1 once the firmware releases it.
pub struct Machine {
    pub cpu: Cpu,
    pub cpu1: Option<Cpu>,
    pub soc: Soc,
    pending_tick: bool,
}

impl Machine {
    /// A firmware package (.fwsc) booted as fm1-live --fwsc boots it; `flash`: the flash chip as
    /// a run of it left it.
    pub fn from_package(pkg: &[u8], flash: Option<&[u8]>, mhz: u64, jit: bool) -> Result<Machine, String> {
        let st = fm1_soc::stock_from_package(pkg, None).map_err(|e| e.to_string())?;
        let mut soc = Soc::new(RAM_SIZE, st.xip.clone());
        soc.periph.cycle_ticks = 1;
        soc.periph.hz = mhz * 1_000_000;
        soc.periph.skip_idle = true;
        let mut cpu = Cpu::new(st.entry);
        cpu.jit_on = jit;
        // the SPL hands cpu0 over in user mode, with its boot-parameter struct (see fm1-emu)
        cpu.user = true;
        let params = RAM_BASE + 0x7fc00;
        soc.write32(params, params + 0x40);
        cpu.regs[0] = fm1_soc::stock_boot(&mut soc, &st.flash, params, st.board);
        soc.sfc_boot(st.key, st.app_end);
        if let Some(f) = flash.filter(|f| f.len() == fm1_soc::FLASH_SIZE) {
            soc.restore_flash(f);
        }
        Ok(Machine { cpu, cpu1: None, soc, pending_tick: false })
    }

    /// Run n instructions of cpu0, and of cpu1 alongside once it is running.
    pub fn run(&mut self, n: u32) -> Result<(), String> {
        let jit = self.cpu.jit_on;
        let Machine { cpu, cpu1, soc, pending_tick } = self;
        machine::run_dual(cpu1, soc, n, RAM_SIZE, jit, |soc, k| machine::run_blocks(cpu, soc, k, pending_tick))
    }

    pub fn is_fm1(&self) -> bool {
        self.soc.board != fm1_soc::Board::Smk37
    }
}

/// What the emulator thread shows the UI, and the UI asks of it.
pub struct Shared {
    pub ring: Arc<Ring>,
    pub lcd: Mutex<Vec<u16>>,
    pub leds: Mutex<[f32; 41]>,
    /// bumped at each new LCD / LED copy
    pub frame: AtomicU64,
    pub status: Mutex<Status>,
    pub quit: AtomicBool,
    /// no emulation (the app is in the background)
    pub paused: AtomicBool,
    /// the audio stream is running: the queue sets the pace
    pub audio_on: AtomicBool,
    /// write the flash chip to its file now
    pub save: AtomicBool,
    /// the queue target's floor and its current value (ms, f64 bits)
    pub floor_ms: AtomicU64,
    pub target_ms: AtomicU64,
}

#[derive(Clone, Default, Debug)]
pub struct Status {
    /// emulated seconds per wall second, over the last half second
    pub speed: f64,
    pub emu_ms: u64,
    pub halted: Option<String>,
    /// the loudest sample over the last half second (0 = silence)
    pub peak: u16,
    pub audio_frames: u64,
}

impl Shared {
    pub fn new(floor_ms: f64) -> Shared {
        Shared {
            ring: Arc::new(Ring::new(1 << 15)),
            lcd: Mutex::new(vec![0u16; 240 * 240]),
            leds: Mutex::new([0f32; 41]),
            frame: AtomicU64::new(0),
            status: Mutex::new(Status::default()),
            quit: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            audio_on: AtomicBool::new(false),
            save: AtomicBool::new(false),
            floor_ms: AtomicU64::new(floor_ms.to_bits()),
            target_ms: AtomicU64::new(floor_ms.to_bits()),
        }
    }

    pub fn floor(&self) -> f64 {
        f64::from_bits(self.floor_ms.load(Ordering::Relaxed))
    }

    pub fn target(&self) -> f64 {
        f64::from_bits(self.target_ms.load(Ordering::Relaxed))
    }

    /// A new floor; the target follows it down at once (up: from the next underrun or now).
    pub fn set_floor(&self, ms: f64) {
        self.floor_ms.store(ms.to_bits(), Ordering::Relaxed);
        self.target_ms.store(ms.to_bits(), Ordering::Relaxed);
    }
}

/// The flash chip to `path` (a new file renamed over the old: a save cut short leaves the last whole).
pub fn save_flash(m: &Machine, path: &std::path::Path) {
    let tmp = path.with_extension("tmp");
    match std::fs::write(&tmp, &m.soc.flash.mem).and_then(|_| std::fs::rename(&tmp, path)) {
        Ok(()) => log::info!("flash saved ({} erases / programs)", m.soc.flash.ops),
        Err(e) => log::warn!("{}: {e}", path.display()),
    }
}

/// The emulator's thread: until `quit`; the flash written back to `flash_path` on `save` and at the end.
pub fn run_thread(mut m: Machine, sh: Arc<Shared>, rx: mpsc::Receiver<Action>, flash_path: std::path::PathBuf) {
    crate::sys::fast_thread();
    let ring = sh.ring.clone();
    let start = Instant::now();
    let mut offset_ms = 0.0f64; // wall-clock time given up after falling behind
    let mut last_lcd = Instant::now();
    let mut last_stat = Instant::now();
    let (mut stat_emu, mut stat_wall) = (0u64, Instant::now());
    let mut underruns_seen = 0u64;
    let mut calm_since = Instant::now();
    let mut peak = 0u16;
    let mut frames = 0u64;
    let mut saved_ops = m.soc.flash.ops;
    let mut halted = false;
    while !sh.quit.load(Ordering::Relaxed) {
        while let Ok(a) = rx.try_recv() {
            m.soc.periph.apply(&a);
        }
        if sh.save.swap(false, Ordering::Relaxed) && m.soc.flash.ops != saved_ops {
            saved_ops = m.soc.flash.ops;
            save_flash(&m, &flash_path);
        }
        if halted || sh.paused.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let real = start.elapsed().as_secs_f64() * 1000.0 - offset_ms;
        let emu_ms = m.soc.periph.cycles as f64 * 1000.0 / m.soc.periph.hz as f64;
        if real - emu_ms > 250.0 {
            offset_ms += real - emu_ms - LEAD_MS; // too far behind (or back from a pause): resync
        }
        let buffered = ring.ms();
        // an underrun: a little more queued from now on; calm: a little less
        let u = ring.underruns.load(Ordering::Relaxed);
        let floor = sh.floor();
        let mut target = sh.target();
        if u > underruns_seen {
            underruns_seen = u;
            calm_since = Instant::now();
            if target < AUDIO_TARGET_MAX_MS {
                target = (target + AUDIO_TARGET_STEP_MS).min(AUDIO_TARGET_MAX_MS);
                sh.target_ms.store(target.to_bits(), Ordering::Relaxed);
            }
        } else if target > floor && calm_since.elapsed() >= AUDIO_TARGET_CALM {
            calm_since = Instant::now();
            target = (target - AUDIO_TARGET_STEP_MS / 2.0).max(floor);
            sh.target_ms.store(target.to_bits(), Ordering::Relaxed);
        }
        let audio_on = sh.audio_on.load(Ordering::Relaxed);
        let behind = if audio_on && m.soc.periph.audio_halves > 0 { buffered < target } else { emu_ms < real + LEAD_MS };
        if behind {
            // (20k instructions: ~0.1 ms emulated, so a touch waits no longer than that to land)
            if let Err(e) = m.run(20_000) {
                log::error!("halt: {e}");
                sh.status.lock().unwrap().halted = Some(e);
                halted = true;
            }
        } else {
            std::thread::sleep(Duration::from_micros(500));
        }
        if !m.soc.periph.audio.is_empty() {
            let a = &m.soc.periph.audio;
            peak = peak.max(a.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0));
            frames += a.len() as u64 / 2;
            if audio_on {
                ring.push(a, QUEUE_CAP);
                ring.started.store(true, Ordering::Relaxed);
            }
            m.soc.periph.audio.clear();
        }
        if last_lcd.elapsed() >= Duration::from_millis(16) {
            sh.lcd.lock().unwrap().copy_from_slice(&m.soc.lcd.fb);
            if m.is_fm1() {
                *sh.leds.lock().unwrap() = m.soc.periph.led_take();
            }
            sh.frame.fetch_add(1, Ordering::Release);
            last_lcd = Instant::now();
        }
        if last_stat.elapsed() >= Duration::from_millis(500) {
            let e = m.soc.periph.ms();
            let speed = (e - stat_emu) as f64 / stat_wall.elapsed().as_secs_f64() / 1000.0;
            {
                let mut s = sh.status.lock().unwrap();
                s.speed = speed;
                s.emu_ms = e;
                s.peak = peak;
                s.audio_frames = frames;
            }
            if e / 2000 != stat_emu / 2000 {
                // (the firmware's audio block: what it fills while the other half plays)
                let block = frames / m.soc.periph.audio_halves.max(1);
                log::info!("[{:5.1} s] {speed:.2}x real time, audio peak {peak}, block {block} frames, queued {buffered:.1} ms (target {target:.0}), {u} underruns",
                           e as f64 / 1000.0);
            }
            peak = 0;
            stat_emu = e;
            stat_wall = Instant::now();
            last_stat = Instant::now();
        }
    }
    if m.soc.flash.ops != saved_ops {
        save_flash(&m, &flash_path);
    }
}
