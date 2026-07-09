//! Pure geometry for the patch-bay circle scene (`docs/scenes/patchbay.md`):
//! socket placement around the rim and chord paths that bow around the open
//! center. No Bevy types — `[f32; 3]` points, unit-tested, same stance as
//! `view/time_well/card.rs`.

/// A rim socket: which endpoint it seats and where.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SocketSeat {
    /// Index into the snapshot's endpoint list.
    pub endpoint_index: usize,
    /// Bearing on the rim, radians, [0, 2π).
    pub angle: f32,
}

/// A client group's label placement (centroid of its ports' seats).
#[derive(Debug, Clone, PartialEq)]
pub struct GroupLabel {
    pub client_name: String,
    pub angle: f32,
}

/// Seat every endpoint around the rim, grouped by client: ports of one
/// client sit adjacent at even pitch; a visible gap separates groups.
/// Groups keep input order (stable across polls when topology is stable).
///
/// Slot model: the rim is divided into `total_ports + gap_slots` equal
/// slots, where `gap_slots` is one slot per group boundary (there are as
/// many boundaries as groups once we wrap back around to the first group)
/// — `n_groups` when there's more than one group, zero for 0 or 1 group
/// (a single group has no adjacent neighbor to gap against). Ports consume
/// one slot each in input order; a gap slot is skipped after each group.
/// `endpoint_index` is the running position in the flattened (group-order)
/// port sequence, which is assumed to line up with the snapshot's endpoint
/// list ordering.
pub fn layout_sockets(
    groups: &[(String, usize)], // (client_name, port_count), port_count >= 1
) -> (Vec<SocketSeat>, Vec<GroupLabel>) {
    let total_ports: usize = groups.iter().map(|(_, n)| *n).sum();
    if total_ports == 0 {
        return (Vec::new(), Vec::new());
    }

    let n_groups = groups.len();
    let gap_slots = if n_groups > 1 { n_groups } else { 0 };
    let total_slots = total_ports + gap_slots;
    let pitch = std::f32::consts::TAU / total_slots as f32;

    let mut seats = Vec::with_capacity(total_ports);
    let mut labels = Vec::with_capacity(n_groups);
    let mut slot: usize = 0;
    let mut endpoint_index: usize = 0;

    for (client_name, port_count) in groups {
        if *port_count == 0 {
            if gap_slots > 0 {
                slot += 1;
            }
            continue;
        }

        // Raw (unnormalized) angles so the midpoint below never has to
        // reason about a group straddling the 0/2π seam.
        let first_raw = slot as f32 * pitch;
        let mut last_raw = first_raw;
        for _ in 0..*port_count {
            let raw = slot as f32 * pitch;
            seats.push(SocketSeat {
                endpoint_index,
                angle: normalize_angle(raw),
            });
            last_raw = raw;
            endpoint_index += 1;
            slot += 1;
        }

        labels.push(GroupLabel {
            client_name: client_name.clone(),
            angle: normalize_angle((first_raw + last_raw) / 2.0),
        });

        if gap_slots > 0 {
            slot += 1;
        }
    }

    (seats, labels)
}

/// Normalize an angle (radians) into `[0, 2π)`.
fn normalize_angle(a: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    let a = a % tau;
    if a < 0.0 { a + tau } else { a }
}

/// Sample a chord from rim angle `a1` to rim angle `a2` (radius `rim_r`,
/// table plane y=0): the path leaves the rim, bows through a corridor that
/// NEVER comes closer to the center than `hole_r` (the open-center rule),
/// takes the angularly shorter side, and lifts to `lift` at its midpoint
/// (y eases 0 → lift → 0). Returns `n` samples (n >= 2), endpoints exactly
/// on the rim seats.
pub fn chord_points(
    a1: f32,
    a2: f32,
    rim_r: f32,
    hole_r: f32,
    lift: f32,
    n: usize,
) -> Vec<[f32; 3]> {
    let n = n.max(2);

    // Shortest signed angular delta from a1 to a2: normalize into
    // [0, 2π) then fold anything past π to the negative (other-way-round)
    // side, so |delta| <= π always.
    let tau = std::f32::consts::TAU;
    let mut delta = (a2 - a1).rem_euclid(tau);
    if delta > std::f32::consts::PI {
        delta -= tau;
    }

    // Corridor radius: never closer to center than hole_r (with margin),
    // and never further out than a fraction of the rim.
    let r_mid = (hole_r * 1.15).max(rim_r * 0.45);

    let mut points = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / (n - 1) as f32;
        let theta = a1 + delta * t;

        let r = rim_r + (r_mid - rim_r) * radial_dip(t);
        let y = lift * (std::f32::consts::PI * t).sin();

        points.push([r * theta.cos(), y, r * theta.sin()]);
    }
    points
}

/// Smoothstep, clamped to `[0, 1]`.
fn smoothstep(u: f32) -> f32 {
    let u = u.clamp(0.0, 1.0);
    u * u * (3.0 - 2.0 * u)
}

/// Radial "dip" weight for `t` in `[0, 1]`: 0 at both ends, 1 at the
/// midpoint, smoothstep-eased on each half so the corridor radius pinches
/// in and back out smoothly (zero slope at t=0, t=0.5, and t=1).
fn radial_dip(t: f32) -> f32 {
    if t <= 0.5 {
        smoothstep(t / 0.5)
    } else {
        smoothstep((1.0 - t) / 0.5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::{PI, TAU};

    fn approx(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() < eps
    }

    // -- layout_sockets ------------------------------------------------

    #[test]
    fn empty_groups_yield_empty_layout() {
        let (seats, labels) = layout_sockets(&[]);
        assert!(seats.is_empty());
        assert!(labels.is_empty());
    }

    #[test]
    fn group_with_zero_ports_only_is_empty() {
        let (seats, labels) = layout_sockets(&[("empty".to_string(), 0)]);
        assert!(seats.is_empty());
        assert!(labels.is_empty());
    }

    #[test]
    fn single_group_has_no_gap() {
        // One group of 4 ports: no gap slots, evenly spaced by 2π/4.
        let (seats, labels) = layout_sockets(&[("solo".to_string(), 4)]);
        assert_eq!(seats.len(), 4);
        let pitch = TAU / 4.0;
        for (i, seat) in seats.iter().enumerate() {
            assert_eq!(seat.endpoint_index, i);
            assert!(
                approx(seat.angle, i as f32 * pitch, 1e-5),
                "seat {i}: {} vs {}",
                seat.angle,
                i as f32 * pitch
            );
        }
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].client_name, "solo");
        // Midpoint of first (0) and last (3*pitch) seat angles.
        assert!(approx(labels[0].angle, 1.5 * pitch, 1e-5));
    }

    #[test]
    fn single_group_single_port_sits_at_zero() {
        let (seats, labels) = layout_sockets(&[("lonely".to_string(), 1)]);
        assert_eq!(seats.len(), 1);
        assert!(approx(seats[0].angle, 0.0, 1e-6));
        assert_eq!(seats[0].endpoint_index, 0);
        assert!(approx(labels[0].angle, 0.0, 1e-6));
    }

    #[test]
    fn two_groups_get_one_gap_slot_each() {
        // groups of 2 ports each: total_ports=4, gap_slots=2 -> total_slots=6.
        let groups = vec![("a".to_string(), 2), ("b".to_string(), 2)];
        let (seats, labels) = layout_sockets(&groups);
        assert_eq!(seats.len(), 4);
        let pitch = TAU / 6.0;

        // Group "a" occupies slots 0, 1.
        assert!(approx(seats[0].angle, 0.0, 1e-5));
        assert!(approx(seats[1].angle, pitch, 1e-5));
        // Gap slot 2 skipped; group "b" occupies slots 3, 4.
        assert!(approx(seats[2].angle, 3.0 * pitch, 1e-5));
        assert!(approx(seats[3].angle, 4.0 * pitch, 1e-5));

        // endpoint_index runs across the flattened port sequence, ignoring
        // gaps.
        assert_eq!(
            seats.iter().map(|s| s.endpoint_index).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );

        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0].client_name, "a");
        assert!(approx(labels[0].angle, 0.5 * pitch, 1e-5));
        assert_eq!(labels[1].client_name, "b");
        assert!(approx(labels[1].angle, 3.5 * pitch, 1e-5));
    }

    #[test]
    fn three_single_port_groups_are_evenly_gapped() {
        // Each group contributes 1 port + 1 gap slot -> total_slots = 6,
        // seats end up evenly spaced by 2*pitch = 2π/3.
        let groups = vec![
            ("a".to_string(), 1),
            ("b".to_string(), 1),
            ("c".to_string(), 1),
        ];
        let (seats, labels) = layout_sockets(&groups);
        assert_eq!(seats.len(), 3);
        let step = TAU / 3.0;
        assert!(approx(seats[0].angle, 0.0, 1e-5));
        assert!(approx(seats[1].angle, step, 1e-5));
        assert!(approx(seats[2].angle, 2.0 * step, 1e-5));
        assert_eq!(labels.len(), 3);
        for (label, seat) in labels.iter().zip(seats.iter()) {
            // A single-port group's label sits exactly on its one seat.
            assert!(approx(label.angle, seat.angle, 1e-5));
        }
    }

    #[test]
    fn all_seat_angles_are_normalized() {
        let groups = vec![
            ("a".to_string(), 3),
            ("b".to_string(), 2),
            ("c".to_string(), 5),
        ];
        let (seats, labels) = layout_sockets(&groups);
        for seat in &seats {
            assert!((0.0..TAU).contains(&seat.angle), "angle {}", seat.angle);
        }
        for label in &labels {
            assert!((0.0..TAU).contains(&label.angle), "angle {}", label.angle);
        }
    }

    // -- chord_points ----------------------------------------------------

    #[test]
    fn endpoints_are_exact() {
        let a1 = 0.4_f32;
        let a2 = 2.1_f32;
        let rim_r = 5.0;
        let hole_r = 1.0;
        let lift = 0.8;
        let pts = chord_points(a1, a2, rim_r, hole_r, lift, 10);
        let start = pts.first().unwrap();
        let end = pts.last().unwrap();
        assert!(approx(start[0], rim_r * a1.cos(), 1e-5));
        assert!(approx(start[1], 0.0, 1e-5));
        assert!(approx(start[2], rim_r * a1.sin(), 1e-5));
        assert!(approx(end[0], rim_r * a2.cos(), 1e-5));
        assert!(approx(end[1], 0.0, 1e-5));
        assert!(approx(end[2], rim_r * a2.sin(), 1e-5));
    }

    #[test]
    fn never_dips_inside_hole() {
        let rim_r = 6.0;
        let hole_r = 1.5;
        let pts = chord_points(0.2, 4.0, rim_r, hole_r, 1.0, 64);
        for p in &pts {
            let r = (p[0] * p[0] + p[2] * p[2]).sqrt();
            assert!(r >= hole_r - 1e-4, "r={r} < hole_r={hole_r}");
        }
    }

    #[test]
    fn takes_the_short_way_around_through_zero() {
        // a1 and a2 straddle angle 0 on the short side (0.2 rad apart the
        // short way, ~6.08 rad the long way through π).
        let a1 = 0.1_f32;
        let a2 = TAU - 0.1;
        let rim_r = 4.0;
        let hole_r = 1.0;
        // n=5 so t=0.5 lands exactly on a sample (index 2).
        let pts = chord_points(a1, a2, rim_r, hole_r, 1.0, 5);
        let mid = pts[2];
        let theta = mid[2].atan2(mid[0]);
        // Should be near 0 (crossing through angle 0), not near π.
        assert!(
            approx(theta, 0.0, 1e-3),
            "midpoint angle {theta} should be near 0, not π"
        );
    }

    #[test]
    fn y_profile_peaks_at_middle_and_is_zero_at_ends() {
        let lift = 2.5;
        let pts = chord_points(0.0, 1.0, 5.0, 1.0, lift, 9);
        assert!(approx(pts.first().unwrap()[1], 0.0, 1e-5));
        assert!(approx(pts.last().unwrap()[1], 0.0, 1e-5));
        // Middle sample (index 4 of 9, t=0.5) should hit the peak.
        assert!(approx(pts[4][1], lift, 1e-5));
        // Monotonic rise then fall around the peak.
        assert!(pts[3][1] < pts[4][1]);
        assert!(pts[5][1] < pts[4][1]);
        for p in &pts {
            assert!(p[1] >= -1e-5 && p[1] <= lift + 1e-5);
        }
    }

    #[test]
    fn near_equal_angles_do_not_produce_nan() {
        let a1 = 1.2345_f32;
        let a2 = a1 + 1e-7;
        let pts = chord_points(a1, a2, 5.0, 1.0, 1.0, 16);
        for p in &pts {
            for coord in p {
                assert!(coord.is_finite(), "non-finite coordinate: {p:?}");
            }
        }
    }

    #[test]
    fn n_of_two_returns_just_the_endpoints() {
        let a1 = 0.3_f32;
        let a2 = 2.0_f32;
        let rim_r = 3.0;
        let hole_r = 0.5;
        let pts = chord_points(a1, a2, rim_r, hole_r, 1.0, 2);
        assert_eq!(pts.len(), 2);
        assert!(approx(pts[0][0], rim_r * a1.cos(), 1e-5));
        assert!(approx(pts[0][2], rim_r * a1.sin(), 1e-5));
        assert!(approx(pts[1][0], rim_r * a2.cos(), 1e-5));
        assert!(approx(pts[1][2], rim_r * a2.sin(), 1e-5));
        assert!(approx(pts[0][1], 0.0, 1e-6));
        assert!(approx(pts[1][1], 0.0, 1e-6));
    }

    #[test]
    fn n_below_two_is_clamped() {
        // n=0 and n=1 should not panic (divide-by-zero on n-1) and should
        // still yield the two endpoints.
        let pts0 = chord_points(0.0, PI / 2.0, 3.0, 1.0, 1.0, 0);
        let pts1 = chord_points(0.0, PI / 2.0, 3.0, 1.0, 1.0, 1);
        assert_eq!(pts0.len(), 2);
        assert_eq!(pts1.len(), 2);
    }
}
