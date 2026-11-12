//! Retargetable animation. Pure, unit tested.
//!
//! Everything that moves (the selection highlight, the apps/windows
//! crossfade, the strip's scroll, opening and closing) is a critically damped
//! spring: changing the target mid-flight keeps position and velocity, so
//! rapid Alt+Tab presses glide instead of restarting or jumping, which a
//! fixed-duration tween can't do. Springs are advanced on frame callbacks
//! with the real elapsed time; the closed-form update is exact for any step,
//! so a dropped frame doesn't change the trajectory.

/// A critically damped spring on one value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spring {
    pub value: f64,
    pub velocity: f64,
    pub target: f64,
    /// Angular frequency, 1/s. Higher is snappier.
    omega: f64,
}

/// Closer than this (in the value's unit, and per second for the velocity)
/// counts as arrived. Values are pixels or 0-to-1 fractions; for fractions a
/// thousandth is invisible too.
const EPSILON: f64 = 1e-3;

impl Spring {
    /// At rest at `value`. `response_ms` is roughly how long it takes to get
    /// most of the way (about 98% at that time) to a new target.
    pub fn new(value: f64, response_ms: f64) -> Self {
        Self {
            value,
            velocity: 0.0,
            target: value,
            // e^(-x)(1+x) ≈ 0.02 at x ≈ 5.8.
            omega: 5.8 / (response_ms.max(1.0) / 1000.0),
        }
    }

    /// Head for `target` from wherever it is now, keeping the velocity.
    pub fn retarget(&mut self, target: f64) {
        self.target = target;
    }

    /// Jump to `value` and stop there.
    pub fn snap(&mut self, value: f64) {
        self.value = value;
        self.target = value;
        self.velocity = 0.0;
    }

    pub fn settled(&self) -> bool {
        self.value == self.target && self.velocity == 0.0
    }

    /// Advance by `dt` seconds. Returns whether it is still moving.
    pub fn advance(&mut self, dt: f64) -> bool {
        if self.settled() {
            return false;
        }
        let w = self.omega;
        let c1 = self.value - self.target;
        let c2 = self.velocity + w * c1;
        let decay = (-w * dt.max(0.0)).exp();
        self.value = self.target + (c1 + c2 * dt) * decay;
        self.velocity = (c2 - w * (c1 + c2 * dt)) * decay;
        if (self.value - self.target).abs() < EPSILON && self.velocity.abs() < EPSILON {
            self.snap(self.target);
            return false;
        }
        true
    }
}

/// Linear interpolation, `t` in 0 to 1 (not clamped: springs may overshoot
/// only when retargeted against their velocity, and following that looks
/// right).
pub fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &mut Spring, dt: f64, steps: usize) {
        for _ in 0..steps {
            s.advance(dt);
        }
    }

    #[test]
    fn rests_until_retargeted() {
        let mut s = Spring::new(5.0, 200.0);
        assert!(s.settled());
        assert!(!s.advance(0.016));
        assert_eq!(s.value, 5.0);
    }

    #[test]
    fn arrives_within_its_response_time_without_overshoot() {
        let mut s = Spring::new(0.0, 200.0);
        s.retarget(100.0);
        let mut last = 0.0;
        for _ in 0..12 {
            s.advance(1.0 / 60.0); // 200 ms
            assert!(s.value >= last && s.value <= 100.0, "{}", s.value);
            last = s.value;
        }
        assert!(s.value > 97.0, "{}", s.value);
        run(&mut s, 1.0 / 60.0, 60);
        assert!(s.settled());
        assert_eq!(s.value, 100.0);
    }

    #[test]
    fn step_size_does_not_change_the_path() {
        let mut a = Spring::new(0.0, 250.0);
        let mut b = a;
        a.retarget(1.0);
        b.retarget(1.0);
        run(&mut a, 0.001, 100);
        b.advance(0.1);
        assert!((a.value - b.value).abs() < 1e-9);
        assert!((a.velocity - b.velocity).abs() < 1e-9);
    }

    #[test]
    fn retargeting_keeps_momentum() {
        let mut s = Spring::new(0.0, 200.0);
        s.retarget(100.0);
        run(&mut s, 1.0 / 60.0, 4);
        let (v, x) = (s.velocity, s.value);
        assert!(v > 0.0);
        s.retarget(200.0);
        assert_eq!((s.value, s.velocity), (x, v));
        s.advance(1.0 / 60.0);
        assert!(s.value > x);
    }

    #[test]
    fn snap_stops() {
        let mut s = Spring::new(0.0, 200.0);
        s.retarget(1.0);
        s.advance(0.01);
        s.snap(0.5);
        assert!(s.settled());
        assert_eq!(s.value, 0.5);
    }

    #[test]
    fn lerps() {
        assert_eq!(lerp(10.0, 20.0, 0.0), 10.0);
        assert_eq!(lerp(10.0, 20.0, 0.5), 15.0);
        assert_eq!(lerp(10.0, 20.0, 1.0), 20.0);
    }
}
