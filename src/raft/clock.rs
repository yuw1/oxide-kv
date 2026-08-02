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

/// Virtual clock for deterministic simulation testing (P7 DST).
///
/// Holds an `epoch` (a real `Instant` captured at construction) and
/// a `virtual_offset` (the cumulative amount of virtual time that
/// has been consumed via `sleep`). `now()` returns `epoch +
/// virtual_offset`, which means:
///
///   - The returned `Instant` is always >= `epoch`, so existing
///     monotonic-clock comparisons (`b >= a`, `duration_since`,
///     `elapsed`) still work as long as they compare within a
///     single `SimClock`'s timeline.
///   - Virtual time is **deterministic** given a fixed `sleep`
///     schedule. Two `SimClock`s consumed by the same virtual
///     advance schedule produce identical `now()` sequences — this
///     is the property that makes a future fault-injection harness
///     replayable.
///
/// The `sleep` future is `tokio::time::Sleep` under the hood, so it
/// cooperates with `tokio::time::pause()` + `tokio::time::advance`.
/// Test authors should use `#[tokio::test(start_paused = true)]` so
/// that advancing the runtime's virtual clock triggers `sleep`
/// wakeups without burning real wall-clock seconds.
///
/// The inner state is `Arc<Mutex<...>>` so the `sleep` future can
/// bump the offset from inside its `poll` future without holding
/// a borrow back to `SimClock` itself.
pub struct SimClock {
    inner: Arc<Mutex<SimClockInner>>,
}

struct SimClockInner {
    epoch: Instant,
    offset: Duration,
}

impl SimClock {
    /// Construct a fresh `SimClock`. Captures `Instant::now()` as
    /// the epoch and starts the virtual offset at zero. Cheap;
    /// safe to call many times per test.
    ///
    /// **Determinism caveat**: two clocks built with `new()` will
    /// have *different* epochs (real wall-clock instants captured
    /// at each construction). For deterministic comparison in a
    /// test ("both clocks see the same `now()` for the same
    /// advance schedule"), share an epoch via `with_epoch` or
    /// pass the same `epoch` argument to both constructors.
    pub fn new() -> Self {
        Self::with_epoch(Instant::now())
    }

    /// Construct a `SimClock` whose epoch is the given `Instant`.
    /// Useful for tests that want multiple clocks to agree on a
    /// common starting point (so `now()` sequences can be
    /// compared directly).
    pub fn with_epoch(epoch: Instant) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SimClockInner {
                epoch,
                offset: Duration::ZERO,
            })),
        }
    }

    /// Read the current virtual offset (sum of consumed virtual
    /// time). Useful for assertions in tests ("the harness spent
    /// 30s of virtual time before deciding to crash peer B").
    pub fn virtual_offset(&self) -> Duration {
        self.inner.lock().unwrap().offset
    }
}

impl Default for SimClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SimClock {
    fn now(&self) -> Instant {
        let inner = self.inner.lock().unwrap();
        inner.epoch + inner.offset
    }

    fn sleep(&self, duration: Duration) -> futures::Sleep {
        // Box the future so we don't have to worry about
        // `tokio::time::Sleep` being `!Unpin` — the box becomes the
        // `Pin<Box<dyn Future + Send>>` payload that
        // `futures::into_sleep` expects. The wrapper future's only
        // job is to bump the virtual offset exactly once on first
        // `Poll::Ready`.
        let inner = self.inner.clone();
        futures::into_sleep(async move {
            tokio::time::sleep(duration).await;
            let mut guard = inner.lock().unwrap();
            guard.offset += duration;
            // lock auto-drops at end of scope
            drop(guard);
        })
    }
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

    /// Determinism check: two `SimClock`s seeded identically must
    /// produce identical `now()` sequences for the same `advance`
    /// schedule. This is the entire point of the abstraction — if
    /// it ever stops holding, the DST harness can't replay.
    #[tokio::test(start_paused = true)]
    async fn sim_clock_is_deterministic_under_same_seed() {
        // Share an explicit epoch so the two clocks start at the
        // same `Instant`. (Without this, each `new()` captures a
        // different wall-clock instant, so `now()` differs even
        // when virtual offsets agree.)
        let epoch = Instant::now();
        let c1 = SimClock::with_epoch(epoch);
        let c2 = SimClock::with_epoch(epoch);

        let schedule = vec![
            Duration::from_millis(0),     // tick immediately
            Duration::from_millis(250),   // election timeout region
            Duration::from_millis(500),
            Duration::from_millis(1000),
            Duration::from_millis(2_500),
            Duration::from_millis(4_500),
        ];

        let mut times1 = Vec::new();
        let mut times2 = Vec::new();
        for d in &schedule {
            // sleep first so the future resolves via virtual advance
            let cf1 = c1.sleep(*d);
            let cf2 = c2.sleep(*d);
            tokio::join!(cf1, cf2);
            times1.push(c1.now());
            times2.push(c2.now());
        }

        assert_eq!(
            times1, times2,
            "two SimClocks with the same advance schedule must agree on every Instant"
        );
    }

    /// Virtual-time semantics: under `start_paused`, real wall
    /// clock does NOT advance, but SimClock `now()` reflects the
    /// virtual advance that consumed the sleep futures.
    #[tokio::test(start_paused = true)]
    async fn sim_clock_advance_moves_virtual_time_without_wall_clock() {
        let clock = SimClock::new();
        let real_before = Instant::now();
        // Capture the SimClock's view of `now()` *before* any sleep.
        let sim_before = clock.now();
        clock.sleep(Duration::from_secs(5)).await;
        clock.sleep(Duration::from_secs(3)).await;
        let real_after = Instant::now();
        // Wall clock is paused (this tokio::test start_paused). The
        // two sleeps consumed 8 seconds of *virtual* time but real
        // elapsed must be near-zero. (We allow a tiny epsilon for
        // scheduling jitter on the host OS.)
        let real_elapsed = real_after.duration_since(real_before);
        assert!(
            real_elapsed < Duration::from_millis(50),
            "start_paused should not advance real wall clock, got {:?}",
            real_elapsed
        );
        // SimClock's `now()` should reflect the 8 virtual seconds
        // since `sim_before`. (Use `virtual_offset` rather than
        // `elapsed()` because the latter subtracts real wall
        // instants, which doesn't move under `start_paused`.)
        let offset_after = clock.virtual_offset();
        assert!(
            offset_after >= Duration::from_secs(8) - Duration::from_millis(50),
            "SimClock should reflect virtual advance, got offset {:?}",
            offset_after
        );
        let _ = sim_before; // keep variable used for documentation
    }

    /// `last_quorum_heartbeat_at` style usage: stamp now, advance
    /// past a deadline, check `now() - stamp >= deadline`. This
    /// mirrors how the leader's ReadIndex lease will use SimClock
    /// in the future DST harness.
    #[tokio::test(start_paused = true)]
    async fn sim_clock_supports_elapsed_style_deadline_checks() {
        let clock = SimClock::new();
        let stamp = clock.now();
        clock.sleep(Duration::from_secs(30)).await;
        // Use `virtual_offset` (not `stamp.elapsed()`, which is
        // real wall-clock) to measure how far virtual time has
        // moved past the stamp.
        let elapsed = clock.now().duration_since(stamp);
        assert!(
            elapsed >= Duration::from_secs(30),
            "SimClock::now() should advance after sleep, got elapsed {:?}",
            elapsed
        );
    }
}