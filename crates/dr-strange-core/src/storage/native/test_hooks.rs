//! Seams the engine's own tests arm: a rendezvous a code path parks at,
//! and a fault a path returns once. Compiled only under `cfg(test)`, and
//! `pub(super)` because the fields that hold them live in the store.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};

/// An optional rendezvous a code path waits at when armed. Arriving there
/// is announced on a channel first, so a test can learn the path is parked
/// without touching any lock the path might be holding.
#[derive(Default)]
pub(super) struct Pause(Mutex<Option<(mpsc::Sender<()>, Arc<Barrier>)>>);

impl Pause {
    /// Arm the pause; the returned receiver fires when it is reached, and
    /// `b.wait()` from the test then releases it.
    pub(super) fn arm(&self, b: Arc<Barrier>) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some((tx, b));
        rx
    }

    pub(super) fn wait(&self) {
        let armed = self.0.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some((tx, b)) = armed {
            let _ = tx.send(());
            b.wait();
        }
    }
}

/// A one-shot injected I/O failure: once armed, the next `trip` returns
/// an error and disarms, so a test can fail a chosen step of a code path
/// deterministically on every platform and uid (unlike permission bits,
/// which root ignores).
#[derive(Default)]
pub(super) struct Fault(AtomicBool);

impl Fault {
    pub(super) fn arm(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub(super) fn trip(&self) -> crate::Result<()> {
        if self.0.swap(false, Ordering::SeqCst) {
            return Err(std::io::Error::other("injected fault").into());
        }
        Ok(())
    }
}
