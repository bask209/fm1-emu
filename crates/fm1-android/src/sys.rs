//! A few system calls: the emulator's thread on the fastest cores at audio priority, and
//! whether this process may map memory read-write-execute (the JIT's code cache).

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::os::raw::{c_int, c_long, c_void};

    extern "C" {
        fn setpriority(which: c_int, who: u32, prio: c_int) -> c_int;
        fn syscall(n: c_long, ...) -> c_long;
        fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
    }

    #[cfg(target_arch = "aarch64")]
    const SYS_GETTID: c_long = 178;
    #[cfg(target_arch = "x86_64")]
    const SYS_GETTID: c_long = 186;
    #[cfg(target_arch = "aarch64")]
    const SYS_SCHED_SETAFFINITY: c_long = 122;
    #[cfg(target_arch = "x86_64")]
    const SYS_SCHED_SETAFFINITY: c_long = 203;

    /// The CPUs with the highest top clock (a phone's big cores), as a mask; None if all are alike.
    fn big_cores() -> Option<u64> {
        let mut f = Vec::new();
        for cpu in 0..64 {
            let p = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/cpuinfo_max_freq");
            match std::fs::read_to_string(p) {
                Ok(s) => f.push((cpu, s.trim().parse::<u64>().unwrap_or(0))),
                Err(_) if cpu > 0 && !std::path::Path::new(&format!("/sys/devices/system/cpu/cpu{cpu}")).exists() => break,
                Err(_) => {}
            }
        }
        let top = f.iter().map(|x| x.1).max()?;
        // the top tier, and the one under it if the top is a single prime core (it may be busy)
        let mut tiers: Vec<u64> = f.iter().map(|x| x.1).collect();
        tiers.sort_unstable();
        tiers.dedup();
        let n_top = f.iter().filter(|x| x.1 == top).count();
        let cut = if n_top == 1 && tiers.len() >= 3 { tiers[tiers.len() - 2] } else { top };
        let mask = f.iter().filter(|x| x.1 >= cut).fold(0u64, |m, x| m | 1 << x.0);
        if f.iter().all(|x| x.1 >= cut) { None } else { Some(mask) }
    }

    pub fn fast_thread() {
        // SAFETY: plain system calls on this thread
        unsafe {
            let tid = syscall(SYS_GETTID) as u32;
            // THREAD_PRIORITY_URGENT_AUDIO (-19) is what Android gives audio threads; take what is allowed
            for prio in [-19, -16, -10, -4] {
                if setpriority(0, tid, prio) == 0 {
                    log::info!("emulator thread: nice {prio}");
                    break;
                }
            }
            if let Some(mask) = big_cores() {
                let r = syscall(SYS_SCHED_SETAFFINITY, tid as c_long, 8 as c_long, &mask as *const u64);
                log::info!("emulator thread: on cores {mask:#x} ({})", if r == 0 { "ok" } else { "refused" });
            }
        }
    }

    /// Can this process map read-write-execute memory? (SELinux's execmem; the JIT needs it, and
    /// falls back to the interpreter by itself when it cannot have it.)
    pub fn rwx_ok() -> bool {
        // SAFETY: a fresh anonymous mapping, unmapped again
        unsafe {
            let p = mmap(std::ptr::null_mut(), 4096, 7, 0x02 | 0x20, -1, 0);
            if p as isize == -1 || p.is_null() {
                return false;
            }
            munmap(p, 4096);
            true
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod imp {
    pub fn fast_thread() {}
    pub fn rwx_ok() -> bool {
        true
    }
}

pub use imp::{fast_thread, rwx_ok};
