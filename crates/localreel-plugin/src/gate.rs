//! Flow control for one running plugin generation.
//!
//! A storage call can outlive the function that started it (`open_range`
//! returns a reader that streams later), so "is the plugin stopping?" cannot be
//! answered by a flag checked once. The gate counts in-flight calls and refuses
//! new ones as soon as a stop begins; the supervisor waits for the counter to
//! reach zero before it takes the provider away and kills the process.
//!
//! This is the Rust translation of Cordis's "wait for dependents before
//! disposing your own resources": the readers are the dependents.

use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use localreel_kernel::error::{KernelError, Result};

/// Admission control for one plugin generation.
#[derive(Debug)]
pub struct Gate {
    accepting: AtomicBool,
    inflight: AtomicUsize,
}

impl Gate {
    /// A gate that accepts calls (the state before a stop begins).
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            accepting: AtomicBool::new(true),
            inflight: AtomicUsize::new(0),
        })
    }

    /// Admit one call, or fail with `StorageUnavailable` once a stop started.
    pub fn enter(self: &Arc<Self>) -> Result<InflightGuard> {
        if !self.accepting.load(Ordering::SeqCst) {
            return Err(self.closed_error());
        }
        self.inflight.fetch_add(1, Ordering::SeqCst);
        // Re-check: `close()` can land between the load above and the
        // increment, and a call that slips through would never be drained.
        if !self.accepting.load(Ordering::SeqCst) {
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            return Err(self.closed_error());
        }
        Ok(InflightGuard {
            gate: Arc::clone(self),
        })
    }

    /// Refuse new calls; in-flight ones keep their guard.
    pub fn close(&self) {
        self.accepting.store(false, Ordering::SeqCst);
    }

    pub fn is_closed(&self) -> bool {
        !self.accepting.load(Ordering::SeqCst)
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }

    fn closed_error(&self) -> KernelError {
        KernelError::StorageUnavailable("the plugin is stopping".to_string())
    }
}

/// Keeps one call counted until the value it returned is dropped.
#[derive(Debug)]
pub struct InflightGuard {
    gate: Arc<Gate>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.gate.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A reader that holds its in-flight guard for as long as the stream lives.
///
/// `open_range` returns this: dropping the response body is what ends the call,
/// not returning from the function.
pub struct GuardedRead<R> {
    inner: R,
    _guard: InflightGuard,
}

impl<R> GuardedRead<R> {
    pub fn new(inner: R, guard: InflightGuard) -> Self {
        Self {
            inner,
            _guard: guard,
        }
    }
}

impl<R: Read> Read for GuardedRead<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_gate_refuses_new_calls() {
        let gate = Gate::new();
        let guard = gate.enter().unwrap();
        assert_eq!(gate.inflight(), 1);
        gate.close();
        assert!(gate.is_closed());
        assert!(matches!(
            gate.enter().unwrap_err(),
            KernelError::StorageUnavailable(_)
        ));
        drop(guard);
        assert_eq!(gate.inflight(), 0);
    }

    #[test]
    fn an_inflight_guard_cannot_leak() {
        let gate = Gate::new();
        {
            let _a = gate.enter().unwrap();
            let _b = gate.enter().unwrap();
            assert_eq!(gate.inflight(), 2);
        }
        assert_eq!(gate.inflight(), 0);
    }
}
