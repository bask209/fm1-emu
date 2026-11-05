//! A MIDI controller as the panel (crates/fm1-map), the WAV recorder, and the labels overlay.
//!
//! The MIDI inputs' callbacks feed one [`Mapper`]; what it asks for goes to the machine as
//! [`Action`]s, and what the window shows (keys held, knobs turned, the recorder, learning) to a
//! [`View`] the window takes each frame.

use crate::panel::{self, ENCS};
use fm1_map::{KeysMode, Mapper, Out, Profile, Target};
use fm1_soc::Action;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{mpsc, Mutex};

/// What happened that the window shows or acts on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ui {
    Record,
    Labels,
    KeysMode(KeysMode),
    Learned,
}

#[derive(Default)]
pub struct View {
    /// keys and buttons the controller holds
    pub down: [bool; 41],
    /// encoder clicks since the window last took them, by panel::ENCS index
    pub enc: [i32; 7],
    pub pot: Option<u16>,
    pub events: Vec<Ui>,
}

pub struct Ctl {
    pub mapper: Mutex<Mapper>,
    pub view: Mutex<View>,
    /// the MIDI inputs in use, for the title
    pub inputs: Mutex<Vec<String>>,
    /// false: --map none, MIDI goes to the jack as it is
    pub on: bool,
    /// --midi-ch: what passes to the jack moved to this channel (1..16; 0: as sent)
    pub midi_ch: u8,
}

impl Ctl {
    pub fn new(profile: Option<Profile>, midi_ch: u8) -> Ctl {
        let on = profile.is_some();
        Ctl {
            midi_ch,
            mapper: Mutex::new(Mapper::new(profile.unwrap_or_default())),
            view: Mutex::new(View::default()),
            inputs: Mutex::new(Vec::new()),
            on,
        }
    }

    /// One message from a MIDI input.
    pub fn feed(&self, msg: &[u8], tx: &mpsc::Sender<Action>) {
        let mut m = self.mapper.lock().unwrap();
        let mut v = self.view.lock().unwrap();
        let ch = self.midi_ch;
        m.feed(msg, &mut |o| apply(o, tx, &mut v, ch));
    }

    /// A target's tag for the overlay.
    pub fn tag(&self, t: Target) -> Option<String> {
        self.mapper.lock().unwrap().profile().tag_of(t)
    }
}

/// What the mapper asked for, to the machine and the view.
pub fn apply(o: Out, tx: &mpsc::Sender<Action>, v: &mut View, midi_ch: u8) {
    match o {
        Out::Key { id, down, vel } => {
            if down && id >= fm1_map::KEY0 {
                let _ = tx.send(Action::KeyVel { vel: vel.clamp(1, 127) });
            }
            let _ = tx.send(Action::Key { id, down });
            if let Some(d) = v.down.get_mut(id as usize) {
                *d = down;
            }
        }
        Out::Enc { enc, clicks } => {
            let _ = tx.send(Action::Enc { enc, clicks });
            if let Some(i) = ENCS.iter().position(|e| e.1 == enc) {
                v.enc[i] += clicks;
            }
        }
        Out::Adc { ch, value } => {
            let _ = tx.send(Action::Adc { ch, value });
            if ch == fm1_map::POT_ADC {
                v.pot = Some(value);
            }
        }
        Out::Midi { mut bytes, len } => {
            if midi_ch != 0 && (0x80..0xf0).contains(&bytes[0]) {
                bytes[0] = (bytes[0] & 0xf0) | (midi_ch - 1);
            }
            for &b in &bytes[..len as usize] {
                let _ = tx.send(Action::Midi { b });
            }
        }
        Out::Record => v.events.push(Ui::Record),
        Out::Labels => v.events.push(Ui::Labels),
        Out::KeysMode(k) => v.events.push(Ui::KeysMode(k)),
        Out::Learned => v.events.push(Ui::Learned),
    }
}

/// The target of a panel control (a key: the key zone, learned so the note pressed plays it).
pub fn target_of(c: panel::Ctl) -> Target {
    match c {
        panel::Ctl::Enc(i) => Target::Enc(ENCS[i].1),
        panel::Ctl::Pot => Target::Master,
        panel::Ctl::Btn(id) => Target::Button(id),
        panel::Ctl::Key(id) => Target::Key(id - fm1_map::KEY0),
    }
}

/// The profile `--map` asks for: "auto" (by the MIDI inputs' names), "none", a profile's name,
/// or a JSON file.
pub fn choose(want: &str, inputs: &[String]) -> Result<Option<Profile>, String> {
    if want == "none" {
        return Ok(None);
    }
    let (all, errs) = fm1_map::all_profiles();
    for e in errs {
        eprintln!("controller profile: {e}");
    }
    if want.ends_with(".json") {
        let s = std::fs::read_to_string(want).map_err(|e| format!("{want}: {e}"))?;
        return Profile::from_json(&s).map(Some).map_err(|e| format!("{want}: {e}"));
    }
    if want != "auto" {
        return all.iter().find(|p| p.name.eq_ignore_ascii_case(want)).cloned().map(Some)
            .ok_or_else(|| format!("no controller profile {want:?} (have: {})", all.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")));
    }
    // the first input with a profile of its own, else the generic one
    for i in inputs {
        if let Some(p) = all.iter().find(|p| !p.device.is_empty() && i.to_lowercase().contains(&p.device.to_lowercase())) {
            return Ok(Some(p.clone()));
        }
    }
    Ok(fm1_map::pick(&all, ""))
}

// ------------------------------------------------------------------ recorder --

/// A 44.1 kHz 16-bit stereo WAV being written.
pub struct Wav {
    file: std::io::BufWriter<std::fs::File>,
    pub path: PathBuf,
    pub frames: u64,
}

impl Wav {
    /// A new file in the user's Music folder (fm1-emu/), else the current folder.
    pub fn create(name: &str) -> std::io::Result<Wav> {
        let dir = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(|h| PathBuf::from(h).join("Music").join("fm1-emu"))
            .filter(|d| std::fs::create_dir_all(d).is_ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let clean: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let path = dir.join(format!("fm1-{}-{secs}.wav", clean.trim_matches('-')));
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);
        file.write_all(&header(0))?;
        Ok(Wav { file, path, frames: 0 })
    }
    /// Interleaved stereo samples.
    pub fn write(&mut self, s: &[i16]) {
        let mut b = Vec::with_capacity(s.len() * 2);
        for x in s {
            b.extend_from_slice(&x.to_le_bytes());
        }
        if self.file.write_all(&b).is_ok() {
            self.frames += s.len() as u64 / 2;
        }
    }
    /// Close it with its length in the header.
    pub fn finish(mut self) -> std::io::Result<PathBuf> {
        self.file.flush()?;
        let mut f = self.file.into_inner().map_err(|e| e.into_error())?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&header((self.frames * 4).min(u32::MAX as u64 - 36) as u32))?;
        Ok(self.path)
    }
}

fn header(data: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    let (rate, ch, bits) = (44_100u32, 2u16, 16u16);
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&ch.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * ch as u32 * bits as u32 / 8).to_le_bytes());
    h[32..34].copy_from_slice(&(ch * bits / 8).to_le_bytes());
    h[34..36].copy_from_slice(&bits.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

// ------------------------------------------------------------------- overlay --

/// The tags of what the controller is bound to, over each panel control.
pub fn draw_labels(buf: &mut [u32], ctl: &Ctl, learning: Option<Target>) {
    let tag_color = 0x8fd3ff;
    let learn_color = 0xff6b6b;
    let col = |t: Target| if learning == Some(t) { learn_color } else { tag_color };
    let tag = |t: Target| -> Option<String> {
        if learning == Some(t) {
            return Some("LEARN".into());
        }
        ctl.tag(t).map(|s| s.to_ascii_uppercase().chars().filter(|c| c.is_ascii_alphanumeric() || "+-/.".contains(*c)).take(7).collect())
    };
    for (i, (_, _, cx, cy, r)) in ENCS.iter().enumerate() {
        let t = Target::Enc(ENCS[i].1);
        if let Some(s) = tag(t) {
            panel::text(buf, *cx, cy + r + 6.0, &s, 2, col(t));
        }
    }
    if let Some(s) = tag(Target::Master) {
        panel::text(buf, panel::POT.0, panel::POT.1 + panel::POT.2 + 6.0, &s, 2, col(Target::Master));
    }
    for (_, id, x, y, w, _) in panel::buttons() {
        let t = Target::Button(id);
        if let Some(s) = tag(t) {
            panel::text(buf, (x + w / 2) as f32, y as f32 - 14.0, &s, 1, col(t));
        }
    }
    for (id, x, y, w, h, _) in panel::keys() {
        let t = Target::Key(id - fm1_map::KEY0);
        if let Some(s) = tag(t) {
            panel::text(buf, (x + w / 2) as f32, (y + h) as f32 - 14.0, &s, 1, col(t));
        }
    }
}
