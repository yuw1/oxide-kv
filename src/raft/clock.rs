//! Clock abstraction for deterministic simulation testing (P7).
//!
//! Production code uses [`SystemClock`], which simply delegates to
//! `std::time::Instant::now()` and `tokio::time::sleep`. Future
//! simulation work (the P7 deterministic simulation testing harness)
//! will provide a `SimClock` implementation that wraps
//! `tokio::time::pause()` + controlled advance; the trait exists so
//! the rest of the codebase can be wired against the abstraction
//! **before** the simulation impl ships.
//!
//! ## Why a trait (not just `tokio::time::pause()`)
//!
//! `tokio::time::pause()` only virtualizes sleeps that go through
//! `tokio::time::sleep` / `tokio::time::interval`. It does **not**
//! virtualize `std::time::Instant::now()` — and the codebase has
//! ~24 production + test call sites that read the wall clock for
//! `last_heartbeat`, `ReadIndex::issued_at`, and election-timer
//! comparisons. Funneling those through `Clock::now()` is the only
//! way the simulation harness can drive them.
//!
//! ## Why `Instant` (not a custom `Time` type)
//!
//! `RaftNode::last_heartbeat` and `ReadIndex::issued_at` are typed
//! as `std::time::Instant`. Introducing a new `Time` trait would
//! cascade through every consumer and every test. Keeping
//! `std::time::Instant` as the return type, and just routing the
//! *production* of that instant through `Clock`, is the smallest
//! change that gives the simulation harness what it needs.
//!
//! ## Construction
//!
//! `RaftNode::new` and `RaftNode::new_with_storage` default to a
//! fresh `SystemClock` for production. Tests that want to inject a
//! custom clock use `RaftNode::new_with_clock(...)` (added in P7).
//! Future sim tests will construct a `SimClock` and inject it the
//! same way. Production code paths never see the injection seam.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Abstract clock. Production: [`SystemClock`]. Future: a `SimClock`
/// for the deterministic simulation testing harness.
pub trait Clock: Send + Sync + 'static {
    /// Return the current monotonic instant. Used for
    /// `last_heartbeat` stamping and `ReadIndex::issued_at`.
    fn now(&self) -> Instant;

    /// Sleep for `duration`. Used for election-timer polls, heartbeat
    /// cadences, coordinator poll intervals, and any future periodic
    /// work. Implementations may advance virtual time instead of
    /// actually sleeping.
    fn sleep(&self, duration: Duration) -> futures::Sleep;
}

/// The return type of [`Clock::sleep`]. Modeled as an opaque future
/// so the trait stays object-safe (Rust object safety forbids
/// `async fn` in trait methods without `async_trait`).
pub mod futures {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    /// Boxed sleep future. `SystemClock` produces
    /// `tokio::time::Sleep` boxes; `SimClock` will produce a future
    /// that resolves on virtual-time advance.
    pub struct Sleep(pub Pin<Box<dyn Future<Output = ()> + Send>>);

    impl Future for Sleep {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            // Safety: the inner future is `Pin<Box<dyn Future + Send>>`
            // which is itself `Unpin`-friendly to poll through.
            self.0.as_mut().poll(cx)
        }
    }

    /// Helper to box any sleep-shaped future. `tokio::time::Sleep`
    /// implements `Future` and is `Send`, so this Just Works.
    pub fn into_sleep<F>(fut: F) -> Sleep
    where
        F: Future<Output = ()> + Send + 'static,
    {
        Sleep(Box::pin(fut))
    }

    /// Convenience constructor for `Duration`-shaped sleeps.
    pub fn sleep_for(duration: Duration) -> Sleep {
        into_sleep(tokio::time::sleep(duration))
    }
}

/// Production clock. Wraps `std::time::Instant::now()` and
/// `tokio::time::sleep`. Stateless and cheaply cloneable.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) -> futures::Sleep {
        futures::sleep_for(duration)
    }
}

/// Convenience: a fresh `Arc<dyn Clock>` wrapping `SystemClock`.
///
/// Production callers (`RaftNode::new`, `RaftNode::new_with_storage`)
/// use this; tests / simulation use `new_with_clock` with a custom
/// impl.
pub fn system_clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock)
}

// `Arc<dyn Clock>` clone is cheap, but trait objects of `dyn Clock`
// don't auto-derive `Clone`. We don't need it to be `Clone` — `Arc`
// clone is the correct way to share. The `Mutex` here is reserved
// for the future `SimClock` impl that will need interior mutability
// to advance virtual time; production `SystemClock` never touches it.
#[allow(dead_code)]
pub(crate) type ClockShared<T = ()> = Arc<Mutex<T>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_now_is_monotonic_within_a_thread() {
        let clock = SystemClock;
        let a = clock.now();
        let b = clock.now();
        assert!(b >= a, "SystemClock::now must be monotonically non-decreasing");
    }

    #[test]
    fn system_clock_arc_is_object_safe() {
        // Compile-time check: Arc<dyn Clock> can be constructed and
        // the trait object has both methods dispatchable.
        let clock: Arc<dyn Clock> = system_clock();
        let _ = clock.now();
        // (We don't await the sleep here; this test just exercises the
        // type plumbing.)
    }

    #[tokio::test]
    async fn system_clock_sleep_actually_sleeps() {
        let clock = SystemClock;
        let start = Instant::now();
        clock.sleep(Duration::from_millis(20)).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(15),
            "expected at least ~20ms of sleep, got {:?}",
            elapsed
        );
    }
}