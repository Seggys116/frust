use std::sync::atomic::{AtomicI32, Ordering};

use crate::sys::{self, signal};

static CHILD: AtomicI32 = AtomicI32::new(0);
static RECEIVED: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(sig: signal::Signal) {
    RECEIVED.store(sig, Ordering::Relaxed);
    let pid = CHILD.load(Ordering::Relaxed);
    if pid > 0 {
        signal::forward(pid as u32, sig);
    }
}

extern "C" fn absorb(sig: signal::Signal) {
    RECEIVED.store(sig, Ordering::Relaxed);
}

fn install(sig: signal::Signal, handler: signal::Handler) {
    let _ = signal::install(sig, handler);
}

// Absorb terminal INT/QUIT to avoid redelivering group signals; forward process-directed TERM/HUP where supported.
pub fn supervise(child_pid: u32) {
    RECEIVED.store(0, Ordering::Relaxed);
    CHILD.store(child_pid as i32, Ordering::Relaxed);
    install(signal::SIGINT, absorb);
    install(signal::SIGQUIT, absorb);
    install(signal::SIGTERM, forward);
    install(signal::SIGHUP, forward);
}

pub fn clear() {
    CHILD.store(0, Ordering::Relaxed);
}

pub fn received() -> Option<i32> {
    let signal = RECEIVED.load(Ordering::Relaxed);
    (signal != 0).then_some(signal)
}

pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    sys::exit_code(status)
}
