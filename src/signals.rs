use std::sync::atomic::{AtomicI32, Ordering};

static CHILD: AtomicI32 = AtomicI32::new(0);
static RECEIVED: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(signal: libc::c_int) {
    RECEIVED.store(signal, Ordering::Relaxed);
    let pid = CHILD.load(Ordering::Relaxed);
    if pid > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe { libc::kill(pid, signal) };
    }
}

extern "C" fn absorb(signal: libc::c_int) {
    RECEIVED.store(signal, Ordering::Relaxed);
}

fn install(signal: libc::c_int, handler: extern "C" fn(libc::c_int)) {
    // SAFETY: installing a plain handler through sigaction with an initialised struct.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = libc::SA_RESTART;
        libc::sigaction(signal, &action, std::ptr::null_mut());
    }
}

pub fn supervise(child_pid: u32) {
    RECEIVED.store(0, Ordering::Relaxed);
    CHILD.store(child_pid as i32, Ordering::Relaxed);
    install(libc::SIGINT, absorb);
    install(libc::SIGQUIT, absorb);
    install(libc::SIGTERM, forward);
    install(libc::SIGHUP, forward);
}

pub fn clear() {
    CHILD.store(0, Ordering::Relaxed);
}

pub fn received() -> Option<i32> {
    let signal = RECEIVED.load(Ordering::Relaxed);
    (signal != 0).then_some(signal)
}

pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| status.signal().map(|s| 128 + s).unwrap_or(1))
}
