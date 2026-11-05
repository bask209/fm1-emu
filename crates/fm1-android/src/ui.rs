//! The touchscreen: fm1-live's panel (panel.rs) on a canvas of the screen's shape, with a bar
//! beside it (or above it, on a squarer screen) for the firmware, the latency settings and the
//! status. Every finger is its own pointer: keys (lower on a key = harder; slide across keys for
//! a glissando, along one for aftertouch), buttons, and the knobs, turned by dragging
//! (up or right = clockwise). Holding the screen for a second opens another firmware.

use crate::panel::{self, Ctl, ENCS, H, LCD_X, LCD_Y, W};
use fm1_soc::Action;
use std::time::{Duration, Instant};

/// The bar beside the panel (a wide screen) or above it.
const BAR_W: usize = 230;
const BAR_H: usize = 96;
/// Panel pixels of drag per encoder click.
const DRAG_PER_CLICK: f32 = 16.0;
/// Hold on the screen this long for the firmware picker.
const HOLD_TO_PICK: Duration = Duration::from_millis(900);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tool {
    Open,
    LatDown,
    LatUp,
    BufDown,
    BufUp,
}

/// What the UI asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum UiOut {
    Act(Action),
    Tool(Tool),
}

pub struct Layout {
    pub win_w: f32,
    pub win_h: f32,
    /// the canvas (the window's buffer, scaled to the window by the compositor)
    pub cw: usize,
    pub ch: usize,
    /// the panel's top left on the canvas
    pub ox: usize,
    pub oy: usize,
    /// the bar: beside the panel (true) or above it
    pub side: bool,
    pub bar_w: usize,
}

impl Layout {
    pub fn new(win_w: u32, win_h: u32) -> Layout {
        let (ww, wh) = (win_w.max(1) as f32, win_h.max(1) as f32);
        let aspect = ww / wh;
        if H as f32 * aspect >= (W + BAR_W) as f32 {
            let (ch, cw) = (H, (H as f32 * aspect).round() as usize);
            let margin = (cw - W) / 2;
            let (bar_w, ox) = if margin >= BAR_W { (margin, margin) } else { (BAR_W, BAR_W + (cw - BAR_W - W) / 2) };
            Layout { win_w: ww, win_h: wh, cw, ch, ox, oy: 0, side: true, bar_w }
        } else {
            let ch = ((H + BAR_H) as f32).max(W as f32 / aspect);
            let cw = (ch * aspect).round().max(W as f32) as usize;
            let ch = ch.round() as usize;
            Layout { win_w: ww, win_h: wh, cw, ch, ox: (cw - W) / 2, oy: BAR_H + (ch - BAR_H - H) / 2, side: false, bar_w: cw }
        }
    }

    fn to_canvas(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.cw as f32 / self.win_w, y * self.ch as f32 / self.win_h)
    }

    fn to_panel(&self, x: f32, y: f32) -> (f32, f32) {
        let (cx, cy) = self.to_canvas(x, y);
        (cx - self.ox as f32, cy - self.oy as f32)
    }

    /// The bar's buttons: (tool, label, x, y, w, h) on the canvas.
    pub fn tools(&self) -> Vec<(Tool, &'static str, usize, usize, usize, usize)> {
        if self.side {
            let (x, w) = (24, self.bar_w.min(320) - 48);
            let half = (w - 12) / 2;
            vec![
                (Tool::Open, "OPEN", x, 40, w, 70),
                (Tool::LatDown, "LAT -", x, 136, half, 64),
                (Tool::LatUp, "LAT +", x + half + 12, 136, half, 64),
                (Tool::BufDown, "BUF -", x, 220, half, 64),
                (Tool::BufUp, "BUF +", x + half + 12, 220, half, 64),
            ]
        } else {
            let labels = [(Tool::Open, "OPEN"), (Tool::LatDown, "LAT -"), (Tool::LatUp, "LAT +"), (Tool::BufDown, "BUF -"), (Tool::BufUp, "BUF +")];
            labels.iter().enumerate().map(|(i, &(t, l))| (t, l, 16 + i * 112, 14, 100, 66)).collect()
        }
    }

    /// Where the status lines go: (x, y, line height, lines).
    fn status_area(&self) -> (f32, f32, f32, usize) {
        if self.side {
            (24.0, 316.0, 30.0, 15)
        } else {
            (16.0 + 5.0 * 112.0 + 8.0, 12.0, 24.0, 3)
        }
    }

    fn tool_at(&self, x: f32, y: f32) -> Option<Tool> {
        let (cx, cy) = self.to_canvas(x, y);
        self.tools().into_iter().find(|&(_, _, tx, ty, w, h)| cx >= tx as f32 && cx < (tx + w) as f32 && cy >= ty as f32 && cy < (ty + h) as f32).map(|t| t.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Grab {
    Panel(Ctl),
    Tool(Tool),
    /// the screen (or nothing): held long, the firmware picker
    Hold,
}

struct Touch {
    id: i32,
    grab: Option<Grab>,
    /// panel coordinates
    last: (f32, f32),
    start: (f32, f32),
    since: Instant,
    acc: f32,
    vel: u8,
}

pub struct Ui {
    pub layout: Layout,
    touches: Vec<Touch>,
    down: [bool; 41],
    enc_clicks: [i32; 7],
    turning: [u32; 8],
    pot: u16,
    shown_leds: [f32; 41],
    base: Vec<u32>,
    panel_buf: Vec<u32>,
    pub canvas: Vec<u32>,
    /// something changed since the last draw
    pub dirty: bool,
    /// a tool button held down (drawn pressed)
    pressed_tool: Option<Tool>,
}

impl Ui {
    pub fn new(win_w: u32, win_h: u32, pot: u16) -> Ui {
        let layout = Layout::new(win_w, win_h);
        let canvas = vec![panel::ROOM; layout.cw * layout.ch];
        Ui {
            layout,
            touches: Vec::new(),
            down: [false; 41],
            enc_clicks: [0; 7],
            turning: [0; 8],
            pot,
            shown_leds: [0.0; 41],
            base: panel::base(),
            panel_buf: vec![0; W * H],
            canvas,
            dirty: true,
            pressed_tool: None,
        }
    }

    pub fn resize(&mut self, win_w: u32, win_h: u32) {
        self.layout = Layout::new(win_w, win_h);
        self.canvas = vec![panel::ROOM; self.layout.cw * self.layout.ch];
        self.dirty = true;
    }

    fn turn(&mut self, i: usize, clicks: i32, out: &mut Vec<UiOut>) {
        self.enc_clicks[i] += clicks;
        self.turning[i] = 10;
        out.push(UiOut::Act(Action::Enc { enc: ENCS[i].1, clicks }));
    }

    /// A finger down at window coordinates (x, y).
    pub fn pointer_down(&mut self, id: i32, x: f32, y: f32, out: &mut Vec<UiOut>) {
        self.touches.retain(|t| t.id != id);
        let (px, py) = self.layout.to_panel(x, y);
        let on_lcd = px >= LCD_X as f32 && px < (LCD_X + 480) as f32 && py >= LCD_Y as f32 && py < (LCD_Y + 480) as f32;
        let grab = if let Some(t) = self.layout.tool_at(x, y) {
            Some(Grab::Tool(t))
        } else if on_lcd {
            Some(Grab::Hold)
        } else {
            Some(panel::hit(px, py).map_or(Grab::Hold, Grab::Panel))
        };
        let vel = match grab {
            Some(Grab::Panel(Ctl::Key(k))) => panel::key_vel(k, py),
            _ => 100,
        };
        if let Some(Grab::Tool(t)) = grab {
            self.pressed_tool = Some(t);
        }
        self.touches.push(Touch { id, grab, last: (px, py), start: (px, py), since: Instant::now(), acc: 0.0, vel });
        self.sync(out);
    }

    /// A finger moved.
    pub fn pointer_move(&mut self, id: i32, x: f32, y: f32, out: &mut Vec<UiOut>) {
        let (px, py) = self.layout.to_panel(x, y);
        let Some(i) = self.touches.iter().position(|t| t.id == id) else { return };
        let (lx, ly) = self.touches[i].last;
        if (px - lx).abs() < 0.01 && (py - ly).abs() < 0.01 {
            return;
        }
        match self.touches[i].grab {
            // glissando: the held note follows the finger across keys; on the same key its
            // height is the pressure (aftertouch)
            Some(Grab::Panel(Ctl::Key(k0))) => {
                if let Some(Ctl::Key(k)) = panel::hit(px, py) {
                    let v = panel::key_vel(k, py);
                    let t = &mut self.touches[i];
                    if k != k0 {
                        t.grab = Some(Grab::Panel(Ctl::Key(k)));
                        t.vel = v;
                    } else if v.abs_diff(t.vel) >= 8 {
                        t.vel = v;
                        out.push(UiOut::Act(Action::Pressure { id: k, vel: v }));
                    }
                }
            }
            Some(Grab::Panel(Ctl::Enc(e))) => {
                let t = &mut self.touches[i];
                t.acc += (ly - py) + (px - lx);
                let mut clicks = 0;
                while t.acc >= DRAG_PER_CLICK {
                    t.acc -= DRAG_PER_CLICK;
                    clicks += 1;
                }
                while t.acc <= -DRAG_PER_CLICK {
                    t.acc += DRAG_PER_CLICK;
                    clicks -= 1;
                }
                if clicks != 0 {
                    self.turn(e, clicks, out);
                }
            }
            Some(Grab::Panel(Ctl::Pot)) => {
                let v = (self.pot as f32 + ((ly - py) + (px - lx)) * 4.0).clamp(0.0, 1023.0) as u16;
                if v != self.pot {
                    self.pot = v;
                    self.turning[7] = 10;
                    out.push(UiOut::Act(Action::Adc { ch: 4, value: v }));
                }
            }
            Some(Grab::Hold) => {
                let t = &self.touches[i];
                if (px - t.start.0).hypot(py - t.start.1) > 40.0 {
                    self.touches[i].grab = None; // a drag, not a hold
                }
            }
            _ => {}
        }
        self.touches[i].last = (px, py);
        self.sync(out);
    }

    /// A finger lifted.
    pub fn pointer_up(&mut self, id: i32, out: &mut Vec<UiOut>) {
        if let Some(i) = self.touches.iter().position(|t| t.id == id) {
            let t = self.touches.remove(i);
            if let Some(Grab::Tool(tool)) = t.grab {
                out.push(UiOut::Tool(tool));
            }
        }
        self.pressed_tool = self.touches.iter().find_map(|t| match t.grab { Some(Grab::Tool(x)) => Some(x), _ => None });
        self.sync(out);
    }

    /// Every finger gone (the gesture was cancelled).
    pub fn cancel(&mut self, out: &mut Vec<UiOut>) {
        self.touches.clear();
        self.pressed_tool = None;
        self.sync(out);
    }

    /// Time passes: a long hold on the screen opens the picker; knob glows fade.
    pub fn tick(&mut self, out: &mut Vec<UiOut>) {
        for t in self.touches.iter_mut() {
            if t.grab == Some(Grab::Hold) && t.since.elapsed() >= HOLD_TO_PICK {
                t.grab = None;
                out.push(UiOut::Tool(Tool::Open));
            }
        }
    }

    /// The keys and buttons the fingers hold now, against what the machine was told.
    fn sync(&mut self, out: &mut Vec<UiOut>) {
        let mut want = [false; 41];
        let mut vel = [100u8; 41];
        for t in &self.touches {
            if let Some(Grab::Panel(Ctl::Key(id) | Ctl::Btn(id))) = t.grab {
                want[id as usize] = true;
                vel[id as usize] = t.vel;
            }
        }
        for id in 0..41 {
            if want[id] != self.down[id] {
                self.down[id] = want[id];
                if want[id] && id >= 14 {
                    out.push(UiOut::Act(Action::KeyVel { vel: vel[id] }));
                }
                out.push(UiOut::Act(Action::Key { id: id as u8, down: want[id] }));
            }
        }
        self.dirty = true;
    }

    /// Knob turns and the pot from elsewhere (a MIDI controller), for the drawing.
    pub fn external(&mut self, enc: &[i32; 7], pot: Option<u16>) {
        for i in 0..7 {
            if enc[i] != 0 {
                self.enc_clicks[i] += enc[i];
                self.turning[i] = 10;
                self.dirty = true;
            }
        }
        if let Some(p) = pot {
            self.pot = p;
            self.turning[7] = 10;
            self.dirty = true;
        }
    }

    pub fn animating(&self) -> bool {
        self.turning.iter().any(|&t| t > 0) || self.shown_leds.iter().any(|&l| l > 0.002)
    }

    /// Draw the canvas: the panel as the machine shows it, `extra_down` (MIDI) held too, and the bar.
    pub fn draw(&mut self, lcd: &[u16], leds: &[f32; 41], extra_down: &[bool; 41], status: &[String]) {
        for i in 0..41 {
            self.shown_leds[i] = leds[i].max(self.shown_leds[i] * 0.8);
        }
        let mut shown_down = self.down;
        for i in 0..41 {
            shown_down[i] |= extra_down[i];
        }
        let st = panel::State { lcd, enc_clicks: &self.enc_clicks, pot: self.pot, down: &shown_down, leds: &self.shown_leds, turning: &self.turning };
        panel::draw(&mut self.panel_buf, &self.base, &st);
        for t in self.turning.iter_mut() {
            *t = t.saturating_sub(1);
        }
        let l = &self.layout;
        let cw = l.cw;
        self.canvas.fill(panel::ROOM);
        for y in 0..H {
            let o = (l.oy + y) * cw + l.ox;
            self.canvas[o..o + W].copy_from_slice(&self.panel_buf[y * W..(y + 1) * W]);
        }
        // the bar
        panel::canvas(cw);
        let c = &mut self.canvas;
        for (t, label, x, y, w, h) in l.tools() {
            let down = self.pressed_tool == Some(t);
            let off = if down { 2.0 } else { 0.0 };
            panel::rrect(c, x as f32 + 1.0, y as f32 + 3.0, w as f32, h as f32, 10.0, 0x101113, 0x101113);
            let (top, bot) = if t == Tool::Open { (0x3c6b43, 0x2f5535) } else { (panel::BTN_TOP, panel::BTN_BOT) };
            panel::rrect(c, x as f32, y as f32 + off, w as f32, h as f32, 10.0, top, bot);
            panel::text(c, (x + w / 2) as f32, (y + h / 2) as f32 - 7.0 + off, label, 2, panel::SILK);
        }
        let (x, y, dy, n) = l.status_area();
        for (i, s) in status.iter().take(n).enumerate() {
            // (panel::text centres on x: a left edge at x)
            let cx = x + (s.len() * 12) as f32 / 2.0;
            let color = if s.starts_with("HALT") || s.starts_with("NO ") { panel::AMBER } else { 0x9c9a94 };
            panel::text(c, cx, y + i as f32 * dy, s, 2, color);
        }
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_fit() {
        for (w, h) in [(2400, 1080), (1920, 1080), (2560, 1600), (1280, 800), (2048, 1536), (800, 1280)] {
            let l = Layout::new(w, h);
            assert!(l.ox + W <= l.cw && l.oy + H <= l.ch, "{w}x{h}");
            let a = l.cw as f32 / l.ch as f32;
            assert!((a - w as f32 / h as f32).abs() < 0.01, "{w}x{h}: aspect {a}");
            for (_, _, x, y, tw, th) in l.tools() {
                assert!(x + tw <= l.cw && y + th <= l.ch);
                // the bar's buttons stay off the panel
                assert!(if l.side { x + tw <= l.ox } else { y + th <= l.oy }, "{w}x{h}");
            }
        }
    }

    /// A key pressed at the bottom is loud; slid onto the next key, the note follows; lifted, it ends.
    #[test]
    fn touch_plays_keys() {
        let mut ui = Ui::new(2400, 1080, 512);
        let (s, ox, oy) = (ui.layout.win_w / ui.layout.cw as f32, ui.layout.ox as f32, ui.layout.oy as f32);
        let (id, kx, ky, kw, kh, _) = panel::keys().into_iter().find(|k| !k.5).unwrap();
        let at = move |x: f32, y: f32| ((ox + x) * s, (oy + y) * s);
        let (x, y) = at(kx as f32 + kw as f32 / 2.0, ky as f32 + kh as f32 * 0.9);
        let mut out = Vec::new();
        ui.pointer_down(1, x, y, &mut out);
        assert!(matches!(out[0], UiOut::Act(Action::KeyVel { vel }) if vel > 100), "{out:?}");
        assert_eq!(out[1], UiOut::Act(Action::Key { id, down: true }));
        out.clear();
        // a second finger on a button
        let (_, bid, bx, by, bw, bh) = panel::buttons()[0];
        let (x2, y2) = at((bx + bw / 2) as f32, (by + bh / 2) as f32);
        ui.pointer_down(2, x2, y2, &mut out);
        assert_eq!(out, vec![UiOut::Act(Action::Key { id: bid, down: true })]);
        out.clear();
        ui.pointer_up(1, &mut out);
        assert_eq!(out, vec![UiOut::Act(Action::Key { id, down: false })]);
        out.clear();
        ui.pointer_up(2, &mut out);
        assert_eq!(out, vec![UiOut::Act(Action::Key { id: bid, down: false })]);
    }

    #[test]
    fn drag_turns_an_encoder() {
        let mut ui = Ui::new(1920, 1080, 512);
        let (s, ox, oy) = (ui.layout.win_w / ui.layout.cw as f32, ui.layout.ox as f32, ui.layout.oy as f32);
        let (_, enc, cx, cy, _) = ENCS[0];
        let (x, y) = ((ox + cx) * s, (oy + cy) * s);
        let mut out = Vec::new();
        ui.pointer_down(5, x, y, &mut out);
        ui.pointer_move(5, x, y - 3.0 * DRAG_PER_CLICK * s, &mut out);
        let clicks: i32 = out.iter().map(|o| match o { UiOut::Act(Action::Enc { enc: e, clicks }) if *e == enc => *clicks, _ => 0 }).sum();
        assert_eq!(clicks, 3);
    }

    #[test]
    fn draws() {
        let mut ui = Ui::new(2400, 1080, 512);
        ui.draw(&vec![0xffffu16; 240 * 240], &[0.0; 41], &[false; 41], &["SPEED 1.00X".into()]);
        let l = &ui.layout;
        // the LCD's white shows through at its place
        let (x, y) = (l.ox + LCD_X + 100, l.oy + LCD_Y + 100);
        assert_eq!(ui.canvas[y * l.cw + x], 0xffffff);
    }
}
