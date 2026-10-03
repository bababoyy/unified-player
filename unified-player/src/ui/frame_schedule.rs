//! Time-driven frame requests for the render loop.
//!
//! State changes wake the render loop through `RedrawSignal`. Content that
//! changes with time alone (spinners, playback progress, synced lyrics) asks
//! for its next frame here while it is being drawn; the loop sleeps until the
//! earliest request.

use std::{cell::Cell, time::Duration, time::Instant};

/// Upper bound on idle sleep, so state changed outside the tracked locks
/// (diagnostics counters, atomics) still appears within a second.
pub(super) const MAX_IDLE_FRAME_INTERVAL: Duration = Duration::from_secs(1);

thread_local! {
    static NEXT_FRAME: Cell<Option<Instant>> = const { Cell::new(None) };
    static MARQUEE_SCROLLED: Cell<bool> = const { Cell::new(false) };
}

/// Request another frame within `delay` of now.
pub(crate) fn request_frame_within(delay: Duration) {
    let due = Instant::now() + delay;
    NEXT_FRAME.with(|next| {
        next.set(Some(next.get().map_or(due, |current| current.min(due))));
    });
}

/// Request the frame that shows the next whole second of `progress` while
/// playback advances.
pub(crate) fn request_progress_frame(progress: chrono::Duration) {
    let millis = progress.num_milliseconds().max(0);
    // A small margin keeps the redraw after the second boundary, not before.
    let until_next_second = 1_000 - millis % 1_000 + 5;
    request_frame_within(Duration::from_millis(until_next_second.unsigned_abs()));
}

/// Record that a focused-row marquee scrolled in this frame. The render loop
/// owns the marquee clock, so it schedules the next phase after drawing.
pub(crate) fn mark_marquee_scrolled() {
    MARQUEE_SCROLLED.with(|scrolled| scrolled.set(true));
}

/// Report and clear whether a marquee scrolled since the last call.
pub(super) fn take_marquee_scrolled() -> bool {
    MARQUEE_SCROLLED.with(Cell::take)
}

/// Take the earliest requested frame time, defaulting to the idle bound.
pub(super) fn take_next_frame(now: Instant) -> Instant {
    let idle = now + MAX_IDLE_FRAME_INTERVAL;
    NEXT_FRAME
        .with(Cell::take)
        .map_or(idle, |due| due.min(idle))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        mark_marquee_scrolled, request_frame_within, request_progress_frame, take_marquee_scrolled,
        take_next_frame, MAX_IDLE_FRAME_INTERVAL,
    };

    #[test]
    fn without_requests_the_loop_sleeps_for_the_idle_bound() {
        let now = Instant::now();
        assert_eq!(take_next_frame(now), now + MAX_IDLE_FRAME_INTERVAL);
    }

    #[test]
    fn the_earliest_request_wins_and_is_consumed() {
        request_frame_within(Duration::from_millis(500));
        request_frame_within(Duration::from_millis(100));
        let now = Instant::now();

        assert!(take_next_frame(now) <= now + Duration::from_millis(100));
        assert_eq!(take_next_frame(now), now + MAX_IDLE_FRAME_INTERVAL);
    }

    #[test]
    fn marquee_scrolling_is_reported_once() {
        assert!(!take_marquee_scrolled());
        mark_marquee_scrolled();
        assert!(take_marquee_scrolled());
        assert!(!take_marquee_scrolled());
    }

    #[test]
    fn progress_frames_land_just_after_the_next_second() {
        request_progress_frame(chrono::Duration::milliseconds(12_900));
        let now = Instant::now();
        let due = take_next_frame(now);

        assert!(due <= now + Duration::from_millis(105));
        assert!(due + Duration::from_millis(50) >= now + Duration::from_millis(105));
    }
}
