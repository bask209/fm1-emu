//! MIDI in: the bytes Android's MIDI service hands the activity (a USB controller), cut into
//! messages and played through the controller mapping (crates/fm1-map), as fm1-live does with
//! its inputs: the MPK mini 3 gets its own profile, anything else the generic one.

use fm1_map::{Mapper, Out, Profile};
use fm1_soc::Action;
use std::sync::{mpsc, Mutex, OnceLock};

pub struct MidiIn {
    mapper: Mapper,
    tx: Option<mpsc::Sender<Action>>,
    /// the message being read: status and data so far
    msg: [u8; 3],
    have: usize,
    need: usize,
    sysex: Vec<u8>,
    in_sysex: bool,
    /// keys and buttons the controller holds, for the drawing
    pub down: [bool; 41],
    /// knob clicks and the pot since the UI last took them
    pub enc: [i32; 7],
    pub pot: Option<u16>,
    /// the devices playing, for the status
    pub devices: Vec<String>,
    pub messages: u64,
}

fn state() -> &'static Mutex<MidiIn> {
    static S: OnceLock<Mutex<MidiIn>> = OnceLock::new();
    S.get_or_init(|| {
        let generic = fm1_map::pick(&fm1_map::builtin(), "").unwrap_or_default();
        Mutex::new(MidiIn {
            mapper: Mapper::new(generic),
            tx: None,
            msg: [0; 3],
            have: 0,
            need: 0,
            sysex: Vec::new(),
            in_sysex: false,
            down: [false; 41],
            enc: [0; 7],
            pot: None,
            devices: Vec::new(),
            messages: 0,
        })
    })
}

pub fn with<R>(f: impl FnOnce(&mut MidiIn) -> R) -> R {
    f(&mut state().lock().unwrap())
}

/// The machine to play (a new one after another firmware is opened).
pub fn set_machine(tx: mpsc::Sender<Action>) {
    with(|m| {
        m.tx = Some(tx);
        m.down = [false; 41];
    });
}

/// A device was opened: its profile from its name (the first device's wins).
pub fn device(name: &str) {
    with(|m| {
        if m.devices.is_empty() {
            let p: Profile = fm1_map::pick(&fm1_map::builtin(), name).unwrap_or_default();
            log::info!("MIDI in: {name} (controller profile {})", p.name);
            let MidiIn { mapper, tx, down, enc, pot, .. } = m;
            mapper.set_profile(p, &mut |o| apply(o, tx.as_ref(), down, enc, pot));
        } else {
            log::info!("MIDI in: {name}");
        }
        m.devices.push(name.to_string());
    });
}

/// One byte from a device.
pub fn byte(b: u8) {
    with(|m| m.byte(b));
}

impl MidiIn {
    fn byte(&mut self, b: u8) {
        if b >= 0xf8 {
            // real time: between any bytes, a message of its own
            return self.message(&[b]);
        }
        if b == 0xf0 {
            self.in_sysex = true;
            self.sysex.clear();
            self.sysex.push(b);
            return;
        }
        if self.in_sysex {
            if b < 0x80 || b == 0xf7 {
                self.sysex.push(b);
                if b == 0xf7 {
                    self.in_sysex = false;
                    // a system exclusive goes to the jack as it is (fm1-live)
                    let bytes = std::mem::take(&mut self.sysex);
                    if let Some(tx) = &self.tx {
                        for &x in &bytes {
                            let _ = tx.send(Action::Midi { b: x });
                        }
                    }
                }
                return;
            }
            self.in_sysex = false; // cut short by a status byte
        }
        if b >= 0x80 {
            self.msg[0] = b;
            self.have = 1;
            self.need = match b {
                0x80..=0xef => fm1_map::msg_len(b) as usize,
                0xf1 | 0xf3 => 2,
                0xf2 => 3,
                _ => 1,
            }
            .max(1);
        } else if self.have == 0 {
            if self.msg[0] < 0x80 || self.msg[0] >= 0xf0 {
                return; // data with no status to run on
            }
            self.have = 1; // running status
            self.msg[self.have] = b;
            self.have += 1;
        } else if self.have < 3 {
            self.msg[self.have] = b;
            self.have += 1;
        }
        if self.have >= self.need && self.have > 0 {
            let msg = self.msg;
            let n = self.need;
            self.have = 0;
            if msg[0] >= 0xf0 {
                self.msg[0] = 0; // system common ends running status
            }
            self.message(&msg[..n]);
        }
    }

    fn message(&mut self, msg: &[u8]) {
        self.messages += 1;
        let MidiIn { mapper, tx, down, enc, pot, .. } = self;
        mapper.feed(msg, &mut |o| apply(o, tx.as_ref(), down, enc, pot));
    }
}

/// What the mapper asked for, to the machine and the drawing (fm1-live's ctl::apply).
fn apply(o: Out, tx: Option<&mpsc::Sender<Action>>, down: &mut [bool; 41], enc: &mut [i32; 7], pot: &mut Option<u16>) {
    let send = |a: Action| {
        if let Some(tx) = tx {
            let _ = tx.send(a);
        }
    };
    match o {
        Out::Key { id, down: d, vel } => {
            if d && id >= fm1_map::KEY0 {
                send(Action::KeyVel { vel: vel.clamp(1, 127) });
            }
            send(Action::Key { id, down: d });
            if let Some(x) = down.get_mut(id as usize) {
                *x = d;
            }
        }
        Out::Enc { enc: e, clicks } => {
            send(Action::Enc { enc: e, clicks });
            if let Some(i) = crate::panel::ENCS.iter().position(|x| x.1 == e) {
                enc[i] += clicks;
            }
        }
        Out::Adc { ch, value } => {
            send(Action::Adc { ch, value });
            if ch == fm1_map::POT_ADC {
                *pot = Some(value);
            }
        }
        Out::Midi { bytes, len } => {
            for &b in &bytes[..len as usize] {
                send(Action::Midi { b });
            }
        }
        // (the desktop's recorder, labels and learn: not on the phone yet)
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_status_and_realtime() {
        let (tx, rx) = mpsc::channel();
        set_machine(tx);
        with(|m| m.mapper = Mapper::new(Profile::default()));
        // note on C4, running status for E4, a clock byte in between, then a note off
        for b in [0x90, 60, 100, 0xf8, 64, 90, 0x80, 60, 0] {
            byte(b);
        }
        assert_eq!(with(|m| m.messages), 4);
        drop(rx);
    }
}
