//! The audio out: an AAudio stream (the NDK's C API, declared here), low-latency performance
//! mode, exclusive where the device grants it (MMAP: the app writes straight into the DSP's
//! buffer), float stereo at the device's own rate (asking for 44.1 kHz would cost exclusive
//! mode and add a resampler in the system). The callback pulls from the emulator's queue
//! (ring.rs). The device buffer starts at `bursts` bursts and grows a burst at each xrun,
//! coming back down after a calm spell, as the emulator's queue target does.

use crate::ring::{Resampler, Ring};
use std::os::raw::{c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[repr(C)]
pub struct AAudioStream {
    _p: [u8; 0],
}
#[repr(C)]
pub struct AAudioStreamBuilder {
    _p: [u8; 0],
}

type DataCb = unsafe extern "C" fn(*mut AAudioStream, *mut c_void, *mut c_void, i32) -> i32;
type ErrCb = unsafe extern "C" fn(*mut AAudioStream, *mut c_void, i32);

#[link(name = "aaudio")]
extern "C" {
    fn AAudio_createStreamBuilder(b: *mut *mut AAudioStreamBuilder) -> i32;
    fn AAudioStreamBuilder_setDirection(b: *mut AAudioStreamBuilder, d: i32);
    fn AAudioStreamBuilder_setPerformanceMode(b: *mut AAudioStreamBuilder, m: i32);
    fn AAudioStreamBuilder_setSharingMode(b: *mut AAudioStreamBuilder, m: i32);
    fn AAudioStreamBuilder_setFormat(b: *mut AAudioStreamBuilder, f: i32);
    fn AAudioStreamBuilder_setChannelCount(b: *mut AAudioStreamBuilder, n: i32);
    fn AAudioStreamBuilder_setDataCallback(b: *mut AAudioStreamBuilder, cb: DataCb, user: *mut c_void);
    fn AAudioStreamBuilder_setErrorCallback(b: *mut AAudioStreamBuilder, cb: ErrCb, user: *mut c_void);
    fn AAudioStreamBuilder_openStream(b: *mut AAudioStreamBuilder, s: *mut *mut AAudioStream) -> i32;
    fn AAudioStreamBuilder_delete(b: *mut AAudioStreamBuilder) -> i32;
    fn AAudioStream_requestStart(s: *mut AAudioStream) -> i32;
    fn AAudioStream_requestStop(s: *mut AAudioStream) -> i32;
    fn AAudioStream_close(s: *mut AAudioStream) -> i32;
    fn AAudioStream_getSampleRate(s: *mut AAudioStream) -> i32;
    fn AAudioStream_getChannelCount(s: *mut AAudioStream) -> i32;
    fn AAudioStream_getFramesPerBurst(s: *mut AAudioStream) -> i32;
    fn AAudioStream_getBufferSizeInFrames(s: *mut AAudioStream) -> i32;
    fn AAudioStream_setBufferSizeInFrames(s: *mut AAudioStream, n: i32) -> i32;
    fn AAudioStream_getBufferCapacityInFrames(s: *mut AAudioStream) -> i32;
    fn AAudioStream_getXRunCount(s: *mut AAudioStream) -> i32;
    fn AAudioStream_getSharingMode(s: *mut AAudioStream) -> i32;
    fn AAudioStream_getPerformanceMode(s: *mut AAudioStream) -> i32;
    fn AAudioStream_getFramesWritten(s: *mut AAudioStream) -> i64;
    fn AAudioStream_getTimestamp(s: *mut AAudioStream, clock: c_int, frame: *mut i64, nanos: *mut i64) -> i32;
    fn AAudio_convertResultToText(r: i32) -> *const std::os::raw::c_char;
}

const DIRECTION_OUTPUT: i32 = 0;
const SHARING_EXCLUSIVE: i32 = 0;
const SHARING_SHARED: i32 = 1;
const PERFORMANCE_LOW_LATENCY: i32 = 12;
const FORMAT_FLOAT: i32 = 2;
const CLOCK_MONOTONIC: c_int = 1;

fn text(r: i32) -> String {
    // SAFETY: AAudio returns a static string for any result
    unsafe {
        let p = AAudio_convertResultToText(r);
        if p.is_null() { format!("error {r}") } else { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
    }
}

struct Cb {
    ring: Arc<Ring>,
    rs: Resampler,
    channels: usize,
    /// the stream died (a device change: headphones in or out); open a new one
    lost: Arc<AtomicBool>,
}

unsafe extern "C" fn data_cb(_s: *mut AAudioStream, user: *mut c_void, data: *mut c_void, frames: i32) -> i32 {
    let cb = &mut *(user as *mut Cb);
    let out = std::slice::from_raw_parts_mut(data as *mut f32, frames.max(0) as usize * cb.channels);
    cb.rs.fill(&cb.ring, out, cb.channels);
    0 // AAUDIO_CALLBACK_RESULT_CONTINUE
}

unsafe extern "C" fn error_cb(_s: *mut AAudioStream, user: *mut c_void, err: i32) {
    let cb = &*(user as *mut Cb);
    log::warn!("audio: stream error {} ({err})", text(err));
    cb.lost.store(true, Ordering::Release);
}

pub struct Output {
    s: *mut AAudioStream,
    cb: *mut Cb,
    pub lost: Arc<AtomicBool>,
    pub rate: u32,
    pub burst: u32,
    pub capacity: u32,
    pub exclusive: bool,
    pub low_latency: bool,
    /// the buffer's floor, in bursts
    pub min_bursts: u32,
    xruns_seen: i32,
    calm_since: Instant,
    started: bool,
}

// SAFETY: the stream handle is used from the UI thread only; the callback's state is its own
unsafe impl Send for Output {}

impl Output {
    pub fn open(ring: Arc<Ring>, min_bursts: u32, exclusive: bool) -> Result<Output, String> {
        let lost = Arc::new(AtomicBool::new(false));
        // (the callback's state lives on the heap until close; its rate is set once the stream says it)
        let cb = Box::into_raw(Box::new(Cb { ring, rs: Resampler::new(48_000), channels: 2, lost: lost.clone() }));
        // SAFETY: AAudio's builder protocol; every failure path frees what it made
        unsafe {
            let mut b = std::ptr::null_mut();
            let r = AAudio_createStreamBuilder(&mut b);
            if r != 0 {
                drop(Box::from_raw(cb));
                return Err(format!("no stream builder: {}", text(r)));
            }
            AAudioStreamBuilder_setDirection(b, DIRECTION_OUTPUT);
            AAudioStreamBuilder_setPerformanceMode(b, PERFORMANCE_LOW_LATENCY);
            AAudioStreamBuilder_setSharingMode(b, if exclusive { SHARING_EXCLUSIVE } else { SHARING_SHARED });
            AAudioStreamBuilder_setFormat(b, FORMAT_FLOAT);
            AAudioStreamBuilder_setChannelCount(b, 2);
            AAudioStreamBuilder_setDataCallback(b, data_cb, cb as *mut c_void);
            AAudioStreamBuilder_setErrorCallback(b, error_cb, cb as *mut c_void);
            let mut s = std::ptr::null_mut();
            let r = AAudioStreamBuilder_openStream(b, &mut s);
            AAudioStreamBuilder_delete(b);
            if r != 0 || s.is_null() {
                drop(Box::from_raw(cb));
                return Err(format!("cannot open the output: {}", text(r)));
            }
            let rate = AAudioStream_getSampleRate(s).max(1) as u32;
            let channels = AAudioStream_getChannelCount(s).max(1) as usize;
            (*cb).rs = Resampler::new(rate);
            (*cb).channels = channels;
            let burst = AAudioStream_getFramesPerBurst(s).max(16) as u32;
            let capacity = AAudioStream_getBufferCapacityInFrames(s).max(burst as i32) as u32;
            let mut o = Output {
                s,
                cb,
                lost,
                rate,
                burst,
                capacity,
                exclusive: AAudioStream_getSharingMode(s) == SHARING_EXCLUSIVE,
                low_latency: AAudioStream_getPerformanceMode(s) == PERFORMANCE_LOW_LATENCY,
                min_bursts: min_bursts.max(1),
                xruns_seen: 0,
                calm_since: Instant::now(),
                started: false,
            };
            o.set_bursts(o.min_bursts);
            log::info!("audio: {} Hz, {channels} ch, burst {burst} frames ({:.2} ms), buffer {} of {capacity} frames, {}, {}",
                rate, burst as f64 * 1000.0 / rate as f64, o.buffer(),
                if o.exclusive { "exclusive (MMAP)" } else { "shared" },
                if o.low_latency { "low-latency path" } else { "NOT the low-latency path" });
            Ok(o)
        }
    }

    pub fn start(&mut self) -> Result<(), String> {
        // SAFETY: an open stream
        let r = unsafe { AAudioStream_requestStart(self.s) };
        if r != 0 {
            return Err(format!("cannot start: {}", text(r)));
        }
        self.started = true;
        Ok(())
    }

    pub fn stop(&mut self) {
        if self.started {
            // SAFETY: an open stream
            unsafe { AAudioStream_requestStop(self.s) };
            self.started = false;
        }
    }

    /// The device buffer, in frames.
    pub fn buffer(&self) -> u32 {
        // SAFETY: an open stream
        unsafe { AAudioStream_getBufferSizeInFrames(self.s).max(0) as u32 }
    }

    pub fn set_bursts(&mut self, n: u32) {
        let want = (n.max(1) * self.burst).min(self.capacity);
        // SAFETY: an open stream
        unsafe { AAudioStream_setBufferSizeInFrames(self.s, want as i32) };
    }

    pub fn xruns(&self) -> i32 {
        // SAFETY: an open stream
        unsafe { AAudioStream_getXRunCount(self.s) }
    }

    /// Once a frame: a burst more after an xrun, a burst less after a calm spell.
    pub fn tune(&mut self) {
        let x = self.xruns();
        let bursts = self.buffer().div_ceil(self.burst);
        if x > self.xruns_seen {
            self.xruns_seen = x;
            self.calm_since = Instant::now();
            if bursts * self.burst < self.capacity {
                self.set_bursts(bursts + 1);
                log::info!("audio: xrun {x}: buffer {} frames", self.buffer());
            }
        } else if bursts > self.min_bursts && self.calm_since.elapsed() >= Duration::from_secs(15) {
            self.calm_since = Instant::now();
            self.set_bursts(bursts - 1);
        }
        if bursts < self.min_bursts {
            self.set_bursts(self.min_bursts);
        }
    }

    /// How long a frame written now takes to reach the speaker (ms), from the stream's timestamp.
    pub fn latency_ms(&self) -> Option<f64> {
        let (mut frame, mut nanos) = (0i64, 0i64);
        // SAFETY: an open stream, plain out-parameters
        unsafe {
            if AAudioStream_getTimestamp(self.s, CLOCK_MONOTONIC, &mut frame, &mut nanos) != 0 {
                return None;
            }
            let written = AAudioStream_getFramesWritten(self.s);
            let mut ts = [0i64; 2];
            extern "C" {
                fn clock_gettime(clock: c_int, ts: *mut i64) -> c_int;
            }
            clock_gettime(CLOCK_MONOTONIC, ts.as_mut_ptr());
            let now = ts[0] * 1_000_000_000 + ts[1];
            // the frame written last plays (written - frame) frames after `frame`, which played at `nanos`
            let at = nanos + (written - frame) * 1_000_000_000 / self.rate as i64;
            Some((at - now) as f64 / 1e6)
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // SAFETY: stop, then close (no callback runs after close returns), then free its state
        unsafe {
            AAudioStream_requestStop(self.s);
            AAudioStream_close(self.s);
            drop(Box::from_raw(self.cb));
        }
    }
}
