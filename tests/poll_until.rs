//! Tests for the shared `poll_until` polling primitive. These live in their own
//! integration-test binary on purpose: `harness/mod.rs` is compiled into every
//! integration test, so a `#[test]` placed there would be duplicated dozens of
//! times.

mod harness;

use harness::{Observation, poll_until};
use std::cell::Cell;
use std::time::{Duration, Instant};

/// The first probe runs immediately — no leading sleep — so an already-true
/// condition returns without waiting an interval.
#[test]
fn probes_immediately_without_initial_sleep() {
    let start = Instant::now();
    let r: Result<u32, _> = poll_until(Duration::from_secs(5), Duration::from_secs(1), || {
        Observation::Ready(42)
    });
    assert_eq!(r.unwrap(), 42);
    assert!(
        start.elapsed() < Duration::from_millis(100),
        "must not sleep before the first probe"
    );
}

/// A timeout returns `Err(WaitTimeout)` carrying the last `Pending` description
/// and the elapsed time; `Display` surfaces both.
#[test]
fn deadline_trips_and_records_last_state() {
    let n = Cell::new(0u32);
    let err = poll_until::<()>(Duration::from_millis(80), Duration::from_millis(10), || {
        n.set(n.get() + 1);
        Observation::Pending(format!("attempt {}", n.get()))
    })
    .unwrap_err();

    assert!(
        err.elapsed >= Duration::from_millis(80),
        "elapsed reaches the deadline"
    );
    assert!(
        err.last.starts_with("attempt "),
        "records the last observed state: {}",
        err.last
    );
    assert!(
        err.to_string().contains(&err.last),
        "Display surfaces the last state: {err}"
    );
}

/// The final wait is clamped to the remaining time, so a long interval cannot
/// overshoot a short deadline.
#[test]
fn final_sleep_clamped_to_remaining() {
    let start = Instant::now();
    let _ = poll_until::<()>(Duration::from_millis(100), Duration::from_secs(5), || {
        Observation::Pending("still".into())
    });
    // Unclamped, the single sleep would be 5s; clamped, we return in ~100ms.
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "returned at the ~100ms deadline, not the 5s interval"
    );
}

/// Returns the `Ready` value after a few `Pending` probes.
#[test]
fn returns_ready_after_pending() {
    let n = Cell::new(0u32);
    let r = poll_until(Duration::from_secs(5), Duration::from_millis(5), || {
        n.set(n.get() + 1);
        if n.get() >= 3 {
            Observation::Ready(n.get())
        } else {
            Observation::Pending("waiting".into())
        }
    });
    assert_eq!(r.unwrap(), 3);
}
