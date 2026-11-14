//! Cursor blink phase. It is only a clock: it knows nothing about windows or rendering, so
//! it is tested with synthetic instants.

use std::time::{Duration, Instant};

pub struct Blink {
    visible: bool,
    interval: Duration,
    deadline: Instant,
}

impl Blink {
    pub fn new(interval: Duration, now: Instant) -> Self {
        Self {
            visible: true,
            interval,
            deadline: now + interval,
        }
    }

    /// Should the cursor be drawn in this phase?
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// When the phase must toggle next
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn set_interval(&mut self, interval: Duration, now: Instant) {
        if interval != self.interval {
            self.interval = interval;
            self.deadline = now + interval;
        }
    }

    /// Toggles the phase if it is due. Returns `true` if it changed (a redraw is needed).
    pub fn tick(&mut self, now: Instant) -> bool {
        if now < self.deadline {
            return false;
        }
        self.visible = !self.visible;
        self.deadline = now + self.interval;
        true
    }

    /// Shows the cursor again and restarts the cycle: while typing or while output arrives the
    /// cursor stays solid. Returns `true` if it was hidden (a redraw is needed).
    pub fn reset(&mut self, now: Instant) -> bool {
        let was_hidden = !self.visible;
        self.visible = true;
        self.deadline = now + self.interval;
        was_hidden
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_millis(500);

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    #[test]
    fn starts_visible_and_waits_a_full_interval() {
        let t0 = Instant::now();
        let blink = Blink::new(INTERVAL, t0);
        assert!(blink.visible());
        assert_eq!(blink.deadline(), at(t0, 500));
    }

    #[test]
    fn ticks_before_the_deadline_do_nothing() {
        let t0 = Instant::now();
        let mut blink = Blink::new(INTERVAL, t0);
        assert!(!blink.tick(at(t0, 499)));
        assert!(blink.visible());
        assert_eq!(blink.deadline(), at(t0, 500));
    }

    #[test]
    fn alternates_at_each_deadline() {
        let t0 = Instant::now();
        let mut blink = Blink::new(INTERVAL, t0);
        assert!(blink.tick(at(t0, 500)));
        assert!(!blink.visible());
        assert_eq!(blink.deadline(), at(t0, 1000));
        assert!(blink.tick(at(t0, 1000)));
        assert!(blink.visible());
    }

    #[test]
    fn a_late_tick_does_not_try_to_catch_up() {
        // If the loop fell several intervals behind, it toggles only once and the next deadline
        // counts from now: no bursts of accumulated blinks
        let t0 = Instant::now();
        let mut blink = Blink::new(INTERVAL, t0);
        assert!(blink.tick(at(t0, 5000)));
        assert!(!blink.visible());
        assert_eq!(blink.deadline(), at(t0, 5500));
        assert!(!blink.tick(at(t0, 5001)));
    }

    #[test]
    fn reset_shows_the_cursor_and_restarts_the_cycle() {
        let t0 = Instant::now();
        let mut blink = Blink::new(INTERVAL, t0);
        blink.tick(at(t0, 500));
        assert!(!blink.visible());

        assert!(
            blink.reset(at(t0, 700)),
            "it was hidden: a redraw is needed"
        );
        assert!(blink.visible());
        assert_eq!(blink.deadline(), at(t0, 1200));
        // With the cursor already visible no redraw is needed
        assert!(!blink.reset(at(t0, 800)));
        assert_eq!(blink.deadline(), at(t0, 1300));
    }

    #[test]
    fn typing_keeps_the_cursor_solid() {
        // Every keypress restarts the deadline, so while typing it never gets to toggle
        let t0 = Instant::now();
        let mut blink = Blink::new(INTERVAL, t0);
        for step in 1..=10 {
            let now = at(t0, step * 300);
            blink.reset(now);
            assert!(!blink.tick(now + Duration::from_millis(100)));
            assert!(blink.visible());
        }
    }

    #[test]
    fn changing_the_interval_restarts_the_deadline_only_when_it_differs() {
        let t0 = Instant::now();
        let mut blink = Blink::new(INTERVAL, t0);
        blink.set_interval(INTERVAL, at(t0, 100));
        assert_eq!(
            blink.deadline(),
            at(t0, 500),
            "same interval: left untouched"
        );
        blink.set_interval(Duration::from_millis(200), at(t0, 100));
        assert_eq!(blink.deadline(), at(t0, 300));
    }
}
