//! Ripples at the bearing of a context whose turn ended.

use bevy::prelude::Resource;

/// Maximum ripples in the deck material and shader.
pub const MAX_RIPPLES: usize = 8;

/// Time for a ripple to cross the deck and fade.
pub const RIPPLE_LIFETIME: f32 = 1.8;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ripple {
    /// Bearing in the deck plane, in radians.
    pub angle: f32,
    pub age: f32,
    pub intensity: f32,
}

#[derive(Resource, Default)]
pub struct RingActivity {
    /// Live ripples in order of arrival.
    pub ripples: Vec<Ripple>,
}

impl RingActivity {
    pub(super) fn ripple(&mut self, angle: f32, intensity: f32) {
        if self.ripples.len() == MAX_RIPPLES {
            self.ripples.remove(0);
        }
        self.ripples.push(Ripple {
            angle,
            age: 0.0,
            intensity: intensity.clamp(0.0, 1.0),
        });
    }

    pub fn tick(&mut self, dt: f32) {
        for ripple in &mut self.ripples {
            ripple.age += dt;
        }
        self.ripples.retain(|r| r.age < RIPPLE_LIFETIME);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ripple_spawns_ages_and_expires() {
        let mut a = RingActivity::default();
        a.ripple(1.23, 1.0);
        assert_eq!(a.ripples.len(), 1);
        assert_eq!(a.ripples[0].age, 0.0);
        assert_eq!(a.ripples[0].angle, 1.23, "ripple carries the context angle");

        a.tick(RIPPLE_LIFETIME * 0.5);
        assert_eq!(a.ripples.len(), 1);
        assert!(a.ripples[0].age > 0.0, "ripple ages");

        a.tick(RIPPLE_LIFETIME); // total age now > lifetime
        assert!(a.ripples.is_empty(), "ripple expires after its lifetime");
    }

    #[test]
    fn ripples_cap_at_max_evicting_the_oldest() {
        let mut a = RingActivity::default();
        // Fire MAX_RIPPLES, aging between each so they have distinct ages.
        for i in 0..MAX_RIPPLES {
            a.ripple(i as f32, 1.0);
            a.tick(0.01);
        }
        assert_eq!(a.ripples.len(), MAX_RIPPLES);
        let oldest_angle = a.ripples[0].angle; // the first fired = oldest

        // One more over the cap: the oldest is evicted, the newest survives.
        a.ripple(9.99, 1.0);
        assert_eq!(a.ripples.len(), MAX_RIPPLES, "stays capped");
        assert!(
            !a.ripples.iter().any(|r| r.angle == oldest_angle),
            "the oldest ripple was evicted"
        );
        assert!(
            a.ripples.iter().any(|r| r.angle == 9.99),
            "the freshest ripple survives"
        );
    }

    #[test]
    fn ripples_keep_the_newest_eight_from_one_frame() {
        let mut activity = RingActivity::default();
        for n in 0..MAX_RIPPLES + 3 {
            activity.ripple(n as f32, 1.0);
        }
        let angles: Vec<_> = activity.ripples.iter().map(|ripple| ripple.angle).collect();
        assert_eq!(angles, (3..MAX_RIPPLES + 3).map(|n| n as f32).collect::<Vec<_>>());
    }
}
