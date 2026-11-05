//! The audio queue between the emulator thread (it pushes) and the audio callback (it pops):
//! one writer, one reader, no lock, so the callback never waits for the emulator.

use std::sync::atomic::{AtomicBool, AtomicI16, AtomicU64, AtomicUsize, Ordering};

pub struct Ring {
    buf: Box<[AtomicI16]>,
    mask: usize,
    /// samples written (the emulator's)
    head: AtomicUsize,
    /// samples read (the callback's)
    tail: AtomicUsize,
    /// set once the firmware has produced sound (silence before that is boot)
    pub started: AtomicBool,
    /// callbacks that found the queue empty after the start
    pub underruns: AtomicU64,
    /// the reader drops what is queued at its next callback (after a pause)
    pub flush: AtomicBool,
}

impl Ring {
    /// A queue of `cap` samples (a power of two; interleaved stereo i16 at 44.1 kHz).
    pub fn new(cap: usize) -> Ring {
        assert!(cap.is_power_of_two());
        Ring {
            buf: (0..cap).map(|_| AtomicI16::new(0)).collect(),
            mask: cap - 1,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            started: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            flush: AtomicBool::new(false),
        }
    }

    /// Samples queued.
    pub fn len(&self) -> usize {
        self.head.load(Ordering::Acquire).wrapping_sub(self.tail.load(Ordering::Acquire))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Milliseconds queued (44.1 kHz stereo).
    pub fn ms(&self) -> f64 {
        self.len() as f64 / 88.2
    }

    /// The writer: queue what fits of `s` (whole frames); how many samples were taken.
    pub fn push(&self, s: &[i16], cap: usize) -> usize {
        let h = self.head.load(Ordering::Relaxed);
        let t = self.tail.load(Ordering::Acquire);
        let used = h.wrapping_sub(t);
        let room = cap.min(self.mask + 1).saturating_sub(used);
        let n = s.len().min(room) & !1;
        for (i, &v) in s[..n].iter().enumerate() {
            self.buf[(h + i) & self.mask].store(v, Ordering::Relaxed);
        }
        self.head.store(h.wrapping_add(n), Ordering::Release);
        n
    }

    /// The reader: the next frame, if one is queued.
    pub fn pop2(&self) -> Option<(i16, i16)> {
        let t = self.tail.load(Ordering::Relaxed);
        let h = self.head.load(Ordering::Acquire);
        if h.wrapping_sub(t) < 2 {
            return None;
        }
        let l = self.buf[t & self.mask].load(Ordering::Relaxed);
        let r = self.buf[(t + 1) & self.mask].load(Ordering::Relaxed);
        self.tail.store(t.wrapping_add(2), Ordering::Release);
        Some((l, r))
    }

    /// The reader: drop everything queued.
    pub fn drain(&self) {
        // (the writer queues whole frames: head - tail stays even)
        self.tail.store(self.head.load(Ordering::Acquire), Ordering::Release);
    }
}

/// The resampler in the callback: the queue's 44.1 kHz frames, linearly interpolated to the
/// device's rate (fm1-live's output_stream).
pub struct Resampler {
    step: f64,
    pos: f64,
    cur: (f32, f32),
    next: (f32, f32),
}

impl Resampler {
    pub fn new(device_rate: u32) -> Resampler {
        Resampler { step: 44_100.0 / device_rate.max(1) as f64, pos: 0.0, cur: (0.0, 0.0), next: (0.0, 0.0) }
    }

    /// Fill `out` (interleaved f32, `channels` per frame) from the ring.
    pub fn fill(&mut self, ring: &Ring, out: &mut [f32], channels: usize) {
        if ring.flush.swap(false, Ordering::AcqRel) {
            ring.drain();
        }
        let mut starved = false;
        for frame in out.chunks_mut(channels.max(1)) {
            self.pos += self.step;
            while self.pos >= 1.0 {
                self.pos -= 1.0;
                self.cur = self.next;
                self.next = match ring.pop2() {
                    Some((l, r)) => (l as f32 / 32768.0, r as f32 / 32768.0),
                    None => {
                        starved = true;
                        (0.0, 0.0)
                    }
                };
            }
            let f = self.pos as f32;
            let (l, r) = (self.cur.0 + (self.next.0 - self.cur.0) * f, self.cur.1 + (self.next.1 - self.cur.1) * f);
            for (c, s) in frame.iter_mut().enumerate() {
                *s = if c % 2 == 0 { l } else { r };
            }
        }
        if starved && ring.started.load(Ordering::Relaxed) {
            ring.underruns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_wraps() {
        let r = Ring::new(8);
        assert_eq!(r.push(&[1, 2, 3, 4, 5, 6], 8), 6);
        assert_eq!(r.pop2(), Some((1, 2)));
        assert_eq!(r.push(&[7, 8, 9, 10, 11, 12], 8), 4);
        assert_eq!(r.len(), 8);
        let all: Vec<_> = std::iter::from_fn(|| r.pop2()).collect();
        assert_eq!(all, vec![(3, 4), (5, 6), (7, 8), (9, 10)]);
        assert!(r.is_empty());
    }

    #[test]
    fn resampler_counts_underruns() {
        let r = Ring::new(64);
        r.started.store(true, Ordering::Relaxed);
        r.push(&[1000; 16], 64);
        let mut rs = Resampler::new(48_000);
        let mut out = [0f32; 64];
        rs.fill(&r, &mut out, 2);
        assert_eq!(r.underruns.load(Ordering::Relaxed), 1);
        assert!(out.iter().any(|&s| s > 0.0));
    }
}
