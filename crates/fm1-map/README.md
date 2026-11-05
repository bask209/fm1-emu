# fm1-map: any MIDI controller as the FM-1's panel

The FM-1's workflow is on its panel: hold a function button and its keys and knobs change
roles. A **controller profile** binds your controller's notes, CCs and pads to the panel's
27 keys, 14 buttons, 7 encoders and the MASTER pot, or passes them to the firmware's MIDI IN.
It maps the hardware, not a firmware, so one profile works for every firmware.

Both the standalone player (`fm1-live`) and the plugin (VST3 / CLAP) use it.

## Using it

**fm1-live**: `--map auto` (the default) picks a profile by your MIDI input's name, else the
generic one. `--map none` turns it off (MIDI goes to MIDI IN as before), `--map "Akai MPK mini 3"`
picks one by name, `--map my.json` reads a file.

| Key | What it does |
| --- | --- |
| right-click a control | MIDI learn: then move or press a control of yours. On a panel key, press the note that should play it. Right-click again to cancel, Delete to unbind |
| F1 | show / hide what is bound where |
| F2 | keys: panel keys → MIDI IN → auto |
| F3 | record a WAV (to Music/fm1-emu) |
| F4 | open another firmware |

**Plugin**: the *Controller* bar picks the profile (the project keeps it; new projects take your
last choice), sets the keys' mode, turns on *Learn* (click a panel control, then move one of
yours), shows *Labels*, and *Record*s a WAV.

Learned bindings are saved as your own profile in `%APPDATA%\fm1-emu\controllers` (Windows),
`~/Library/Application Support/fm1-emu/controllers` (macOS) or `~/.config/fm1-emu/controllers`.
A profile there replaces a built-in one of the same name.

## Keys: three modes

- **panel keys**: your keys press the FM-1's keys, as if you pressed them. Every firmware, every
  layer (GLO + a key, steps in SEQ…). The panel has 27 keys, F3 to G5; notes outside move by
  octaves onto them, and the FM-1's OCT buttons shift what they play.
- **MIDI IN**: your keys play the firmware through its MIDI driver, with your keyboard's range
  and velocity (firmwares that read MIDI IN only).
- **auto**: MIDI IN, but the panel's keys while you hold a function button (FX … SEQ).

## Akai MPK mini 3 (factory settings, program 1)

| MPK | FM-1 |
| --- | --- |
| knobs 1–4 (CC 70–73) | KNOB1–KNOB4 |
| knob 5, 6, 7 (CC 74–76) | SELECT, PRESETS, ALGORITHM |
| knob 8 (CC 77) | MASTER (volume) |
| pads bank A, top row | FX, ENV, LFO, GLO |
| pads bank A, bottom row | PLAY, REC, SEQ, ARP |
| pads bank B, top row | SCL, EDIT, SAVE, HOME |
| pads bank B, bottom row | OCT−, OCT+, keys mode, record |
| keys | the panel's keys |
| joystick, sustain | the firmware's MIDI IN |

The knobs are absolute out of the box, so a knob stops turning an encoder at its end. For endless
turning, set knobs 1–7 to *Relative* in Akai's MPK mini 3 program editor and change their `mode`
to `relative` in the profile (if they turn the wrong way or jump, try `relative-64` or
`relative-signed`: controllers encode relative knobs in one of those three ways).

## The profile format

```json
{
  "name": "My controller",
  "device": "part of its MIDI input name",
  "pass_unmapped": true,
  "keys_mode": "panel",
  "bindings": [
    { "src": { "ch": 1, "cc": 70 }, "to": "KNOB1", "mode": "relative", "label": "K1" },
    { "src": { "ch": 10, "note": 36 }, "to": "PLAY" },
    { "src": { "ch": 10, "note": 37 }, "to": "GLO", "mode": "toggle" },
    { "src": { "cc": 20 }, "to": "SELECT", "scale": 0.5 },
    { "src": { "note": 38 }, "to": "PRESETS", "mode": "step", "scale": -1 },
    { "src": { "ch": 1, "notes": [0, 127] }, "to": "keys", "base": 53, "fold": true },
    { "src": { "bend": true }, "to": "midi" }
  ]
}
```

- `src`: `ch` (1–16, left out: any), and one of `note`, `notes` [lo, hi], `cc`, `bend`.
- `to`: a button (`OCT-` `OCT+` `FX` `SCL` `ENV` `LFO` `EDIT` `GLO` `HOME` `SAVE` `ARP` `SEQ`
  `PLAY` `REC`), an encoder (`SELECT` `PRESETS` `ALGORITHM` `KNOB1`–`KNOB4`), `MASTER`, `keys`
  (a key zone), `key:C4` (one key), `midi` (to MIDI IN), `ignore`, or the front end's own
  `keys-mode`, `record`, `labels`.
- `mode`: knobs `absolute` (the default for a CC), `relative` (two's complement),
  `relative-64`, `relative-signed`; buttons `momentary` (the default) or `toggle`; `step` turns
  an encoder `scale` clicks a press.
- `scale`: encoder clicks per step. `base`: the note that plays the panel's F3 (53 = F3).
  `fold`: notes outside the panel move by octaves onto it. `label`: what the overlay shows.
- The first binding that matches a message takes it; with `pass_unmapped`, the rest go to MIDI IN.
