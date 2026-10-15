//! Removes the socket on Ctrl-C and SIGTERM, using `signal(2)` from the C
//! library that std links anyway instead of an extra crate.

use std::ffi::c_int;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tracing::{info, warn};

const SIGINT: c_int = 2;
const SIGTERM: c_int = 15;
const SIG_ERR: usize = usize::MAX;

static STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    fn signal(signum: c_int, handler: extern "C" fn(c_int)) -> usize;
}

extern "C" fn on_signal(_: c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Catches SIGINT and SIGTERM. A watcher thread then removes `socket` and
/// exits the process.
pub fn remove_on_signal(socket: PathBuf) -> std::io::Result<()> {
    for signum in [SIGINT, SIGTERM] {
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        if unsafe { signal(signum, on_signal) } == SIG_ERR {
            warn!("cannot catch signal {signum}, so the socket stays behind when it arrives");
        }
    }
    std::thread::Builder::new().name("signals".into()).spawn(move || {
        while !STOP.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
        info!("stopping and removing {}", socket.display());
        let _ = std::fs::remove_file(&socket);
        std::process::exit(0);
    })?;
    Ok(())
}
