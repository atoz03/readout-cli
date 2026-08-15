//! Animation.
//!
//! Every animated quantity is an [`Eased`]: a current value chasing a target.
//! The loop ticks on a fixed interval and each tick moves current a fixed
//! fraction of the remaining distance, so motion is frame-rate independent in
//! feel and always settles. Nothing here interpolates *data* — the numbers
//! shown are always the real ones once settled; the easing only governs how
//! they arrive.

use std::time::{Duration, Instant};

/// Frame interval. ~16 fps is enough for bar growth and cheap on a remote
/// terminal, where every frame is bytes over the wire.
pub const TICK: Duration = Duration::from_millis(60);

/// A value that eases toward a target.
#[derive(Debug, Clone, Copy)]
pub struct Eased {
    current: f64,
    target: f64,
    /// Fraction of the remaining distance covered per tick, 0..1.
    rate: f64,
}

impl Eased {
    #[cfg(test)]
    pub fn new(value: f64) -> Self {
        Eased { current: value, target: value, rate: 0.28 }
    }

    /// Start at zero so the first render grows into place.
    pub fn from_zero(target: f64) -> Self {
        Eased { current: 0.0, target, rate: 0.28 }
    }

    pub fn with_rate(mut self, rate: f64) -> Self {
        self.rate = rate.clamp(0.01, 1.0);
        self
    }

    #[cfg(test)]
    pub fn set_target(&mut self, target: f64) {
        self.target = target;
    }

    /// Jump to the target with no animation — used when the user would
    /// otherwise watch a long count-up they did not ask for.
    pub fn snap_to(&mut self, target: f64) {
        self.current = target;
        self.target = target;
    }

    pub fn value(&self) -> f64 {
        self.current
    }

    pub fn settled(&self) -> bool {
        (self.target - self.current).abs() <= self.epsilon()
    }

    fn epsilon(&self) -> f64 {
        // Scale-relative, so a 12-billion-token target does not spend hundreds
        // of frames creeping the last few units.
        (self.target.abs() * 1e-4).max(1e-6)
    }

    /// Advance one tick. Returns true while still moving.
    pub fn tick(&mut self) -> bool {
        if self.settled() {
            self.current = self.target;
            return false;
        }
        self.current += (self.target - self.current) * self.rate;
        if self.settled() {
            self.current = self.target;
            return false;
        }
        true
    }
}

/// A looping frame counter for indeterminate motion (spinners, shimmer).
#[derive(Debug, Clone, Copy)]
pub struct Pulse {
    start: Instant,
}

impl Default for Pulse {
    fn default() -> Self {
        Pulse { start: Instant::now() }
    }
}

impl Pulse {
    /// Position in the cycle, 0.0..1.0.
    pub fn phase(&self, period: Duration) -> f64 {
        let p = period.as_secs_f64().max(0.001);
        (self.start.elapsed().as_secs_f64() % p) / p
    }

    /// Index into a frame list.
    pub fn frame(&self, frames: usize, period: Duration) -> usize {
        ((self.phase(period) * frames as f64) as usize).min(frames.saturating_sub(1))
    }
}

/// Braille spinner frames — one cell wide, reads as motion at any size.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easing_converges_and_stops() {
        let mut e = Eased::from_zero(100.0);
        assert_eq!(e.value(), 0.0);
        let mut ticks = 0;
        while e.tick() {
            ticks += 1;
            assert!(ticks < 500, "easing must terminate");
        }
        assert_eq!(e.value(), 100.0, "it must land exactly on the target");
    }

    #[test]
    fn a_huge_target_still_settles_promptly() {
        let mut e = Eased::from_zero(12_000_000_000.0);
        let mut ticks = 0;
        while e.tick() {
            ticks += 1;
        }
        assert!(ticks < 60, "scale-relative epsilon keeps large values from creeping: {ticks}");
        assert_eq!(e.value(), 12_000_000_000.0);
    }

    #[test]
    fn retargeting_mid_flight_redirects_rather_than_restarts() {
        let mut e = Eased::from_zero(100.0);
        e.tick();
        let mid = e.value();
        assert!(mid > 0.0 && mid < 100.0);
        e.set_target(0.0);
        while e.tick() {}
        assert_eq!(e.value(), 0.0);
    }

    #[test]
    fn snapping_skips_the_animation() {
        let mut e = Eased::from_zero(100.0);
        e.snap_to(42.0);
        assert_eq!(e.value(), 42.0);
        assert!(!e.tick());
    }

    #[test]
    fn a_settled_value_reports_no_motion() {
        let mut e = Eased::new(5.0);
        assert!(!e.tick());
    }

    #[test]
    fn spinner_frames_stay_in_range() {
        let p = Pulse::default();
        for _ in 0..50 {
            assert!(p.frame(SPINNER.len(), Duration::from_millis(600)) < SPINNER.len());
        }
    }
}
