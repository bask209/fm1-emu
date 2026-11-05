//! Headless: boot a firmware package as the app does and report how fast it runs.
//!
//!   fm1-bench FIRMWARE.fwsc [MS] [--no-jit] [--panel OUT.ppm]
//!
//! It holds C4-E4-G4 from 0.8 s (sound for the peak) and prints the speed of the whole run and
//! of its steady part (after the first second, when the JIT has compiled the hot code); with
//! --panel the panel as the app draws it at the end.

use fm1_android::emu::Machine;
use fm1_soc::Action;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(path) = args.get(1) else {
        eprintln!("usage: fm1-bench FIRMWARE.fwsc [MS] [--no-jit] [--panel OUT.ppm]");
        std::process::exit(2);
    };
    let ms: u64 = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(6000);
    let jit = !args.iter().any(|a| a == "--no-jit") && fm1_android::sys::rwx_ok();
    let panel = args.iter().position(|a| a == "--panel").and_then(|i| args.get(i + 1)).cloned();
    let pkg = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(1)
    });
    fm1_android::sys::fast_thread();
    let mut m = Machine::from_package(&pkg, None, 240, jit).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(1)
    });
    for (at, id) in [(800, 21u8), (900, 25), (1000, 28)] {
        let c = m.soc.periph.cycle_of_ms(at);
        m.soc.periph.script.push_back((c, Action::Key { id, down: true }));
    }
    let t = Instant::now();
    let mut steady = None;
    let mut peak = 0u16;
    let mut frames = 0usize;
    while m.soc.periph.ms() < ms {
        if steady.is_none() && m.soc.periph.ms() >= 1000 {
            steady = Some((Instant::now(), m.soc.periph.ms()));
        }
        if let Err(e) = m.run(100_000) {
            eprintln!("halt at {} ms: {e}", m.soc.periph.ms());
            std::process::exit(1);
        }
        let a = &m.soc.periph.audio;
        if m.soc.periph.ms() > 1200 {
            peak = peak.max(a.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0));
        }
        frames += a.len() / 2;
        m.soc.periph.audio.clear();
    }
    let wall = t.elapsed().as_secs_f64();
    let steady_rt = steady.map(|(t, start)| (m.soc.periph.ms() - start) as f64 / 1000.0 / t.elapsed().as_secs_f64());
    println!("{} ms emulated in {wall:.2} s: {:.2}x real time; steady {:.2}x; JIT {}; {frames} audio frames, peak {peak} with a chord held",
             m.soc.periph.ms(), m.soc.periph.ms() as f64 / 1000.0 / wall, steady_rt.unwrap_or(0.0), if jit { "on" } else { "off" });
    if let Some(out) = panel {
        let mut ui = fm1_android::ui::Ui::new(2400, 1080, m.soc.periph.adc_value[4]);
        let leds = if m.is_fm1() { m.soc.periph.led_take() } else { [0.0; 41] };
        let lines = vec![format!("SPEED {:.2}X", steady_rt.unwrap_or(0.0)), format!("PEAK {peak}")];
        ui.draw(&m.soc.lcd.fb, &leds, &[false; 41], &lines);
        let (w, h) = (ui.layout.cw, ui.layout.ch);
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        for p in &ui.canvas {
            ppm.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
        }
        match std::fs::write(&out, ppm) {
            Ok(()) => println!("panel -> {out}"),
            Err(e) => eprintln!("{out}: {e}"),
        }
    }
}
