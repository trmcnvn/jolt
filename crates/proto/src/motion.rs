//! Loader motion — the pure math behind jolt's loading indicators.
//!
//! These are the curves and constants the gpui viewport animates with
//! (`jolt-ui/src/motion.rs`, `jolt-ui/src/loaders.rs`), lifted here so any
//! surface animates the *same* loaders rather than inventing its own spinner.
//! A loading indicator is a brand surface; two of them that disagree read as
//! two products.
//!
//! Everything is a pure function of an explicit time input: repeating loaders
//! take a phase in `0..1`, while the activity orb takes elapsed seconds. Callers can
//! use their native frame clocks and still produce identical output.

/// Jolt loader pulse period.
pub const JOLT_PULSE_MS: u64 = 2_400;
/// Gradient matrix spinner wave period.
pub const GRADIENT_SPIN_MS: u64 = 750;
/// Time multiplier for the connecting-web activity orb.
pub const ACTIVITY_WEB_SPEED: f32 = 6.63;

/// Cells in the jolt wave loader.
pub const JOLT_CELLS: usize = 5;
/// Side length of the gradient spinner matrix.
pub const MATRIX_SIDE: usize = 3;

/// Jolt loader cells rest at this opacity between pulses.
pub const PULSE_MIN_OPACITY: f32 = 0.08;
/// …and at this scale.
pub const PULSE_MIN_SCALE: f32 = 0.9;
/// Per-cell stagger, as a fraction of the pulse period (0.15s of 2.4s).
pub const PULSE_STAGGER: f32 = 0.15 / 2.4;

/// Per-row tints of the gradient matrix spinner's sunrise gradient: cool blue
/// at the top, through amber, to pink.
pub const GSPIN_ROW_TINTS: [u32; MATRIX_SIDE] = [0xB6D3EF, 0xEDB185, 0xF888A0];
/// Opacity a gradient-spinner cell rests at between pulses.
pub const GSPIN_DIM: f32 = 0.1;

/// Clockwise ring position of each `(row, col)` cell of the 2×3 mini spinner,
/// top-left first: (0,0) → (0,1) → (1,1) → (2,1) → (2,0) → (1,0). Every cell of
/// a 2×3 grid is on the ring, so the brightness chases around it.
pub const MINI_RING: [[usize; 2]; 3] = [[0, 1], [5, 2], [4, 3]];
/// Cells in the mini spinner's ring.
pub const MINI_RING_LEN: f32 = 6.0;

/// One projected dot in the activity orb, in normalized canvas coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActivityOrbDot {
    pub x: f32,
    pub y: f32,
    /// Projected depth, used to paint far dots before near dots.
    pub depth: f32,
    /// Radius in logical pixels before the painter's minimum-radius clamp.
    pub radius: f32,
    /// Monochrome ink value: 0 is darkest on a light surface.
    pub ink: f32,
    pub opacity: f32,
}

/// One edge in the connecting-web activity orb.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActivityOrbLine {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    pub width: f32,
    pub ink: f32,
    pub opacity: f32,
}

/// A complete connecting-web frame. Edges paint before nodes.
#[derive(Clone, Debug, PartialEq)]
pub struct ActivityOrbFrame {
    pub lines: Vec<ActivityOrbLine>,
    pub dots: Vec<ActivityOrbDot>,
}

#[derive(Clone, Copy)]
struct OrbNode {
    x: f32,
    y: f32,
    z: f32,
}

fn hash_2d(a: f32, b: f32) -> f32 {
    let hash = (f64::from(a) * 12.9898 + f64::from(b) * 78.233).sin() * 43_758.545_3;
    (hash - hash.floor()) as f32
}

fn value_noise(x: f32, y: f32) -> f32 {
    let xi = x.floor();
    let yi = y.floor();
    let mut fx = x - xi;
    let mut fy = y - yi;
    fx = fx * fx * (3.0 - 2.0 * fx);
    fy = fy * fy * (3.0 - 2.0 * fy);
    let a = hash_2d(xi, yi);
    let b = hash_2d(xi + 1.0, yi);
    let c = hash_2d(xi, yi + 1.0);
    let d = hash_2d(xi + 1.0, yi + 1.0);
    a + (b - a) * fx + (c - a) * fy + (a - b - c + d) * fx * fy
}

fn fibonacci_direction(index: usize, count: usize) -> OrbNode {
    let golden_angle = std::f32::consts::PI * (3.0 - 5.0_f32.sqrt());
    let y = 1.0 - 2.0 * (index as f32 + 0.5) / count as f32;
    let radial = (1.0 - y * y).sqrt();
    let angle = index as f32 * golden_angle;
    OrbNode {
        x: radial * angle.cos(),
        y,
        z: radial * angle.sin(),
    }
}

fn project_web_point(node: OrbNode, time: f32) -> (f32, f32, f32) {
    const CAMERA_TILT: f32 = 0.32;
    let yaw = time * 0.12;
    let x = node.x * yaw.cos() + node.z * yaw.sin();
    let z = -node.x * yaw.sin() + node.z * yaw.cos();
    let y = node.y * CAMERA_TILT.cos() - z * CAMERA_TILT.sin();
    let depth = node.y * CAMERA_TILT.sin() + z * CAMERA_TILT.cos();
    (0.5 + x * 0.4, 0.5 - y * 0.4, depth)
}

/// Build the compact `thinking-orbs` connecting frame used by activity
/// indicators. Nearby drifting nodes wire themselves into a constellation and
/// a bright packet travels between a deterministic pair. Sub-20px slots use
/// denser nodes and stronger strokes than the 20px preset so the
/// connections survive device-pixel quantization.
pub fn activity_orb_frame(time: f32, size_px: f32) -> ActivityOrbFrame {
    const NODE_RADIUS: f32 = 1.4 * 1.52;
    const NODE_RADIUS_DEPTH: f32 = 1.8 * 1.52;
    const LINE_OPACITY_SCALE: f32 = 1.8;

    let size_px = size_px.max(f32::EPSILON);
    let (node_count, connection_threshold, minimum_line_width) = if size_px <= 10.0 {
        (8, 1.05, 0.75)
    } else {
        (12, 0.9, 0.9)
    };
    let radius_scale = (size_px / 300.0).powf(0.6);
    let mut nodes = Vec::with_capacity(node_count);
    for index in 0..node_count {
        let base = fibonacci_direction(index, node_count);
        let x = base.x + 0.6 * (value_noise(index as f32 * 0.31 + 9.0, time * 0.24) - 0.5);
        let y = base.y + 0.6 * (value_noise(index as f32 * 0.53 + 27.0, time * 0.21) - 0.5);
        let z = base.z + 0.6 * (value_noise(index as f32 * 0.77 + 55.0, time * 0.27) - 0.5);
        let length = (x * x + y * y + z * z).sqrt();
        nodes.push(OrbNode {
            x: x / length,
            y: y / length,
            z: z / length,
        });
    }

    let mut lines = Vec::new();
    for first in 0..node_count {
        for second in first + 1..node_count {
            let dx = nodes[first].x - nodes[second].x;
            let dy = nodes[first].y - nodes[second].y;
            let dz = nodes[first].z - nodes[second].z;
            let distance = (dx * dx + dy * dy + dz * dz).sqrt();
            if distance >= connection_threshold {
                continue;
            }
            let (x1, y1, z1) = project_web_point(nodes[first], time);
            let (x2, y2, z2) = project_web_point(nodes[second], time);
            let depth = ((z1 + z2) * 0.5 + 1.0) * 0.5;
            lines.push(ActivityOrbLine {
                x1,
                y1,
                x2,
                y2,
                width: (0.8 * radius_scale).max(minimum_line_width),
                ink: 0.42,
                opacity: ((1.0 - distance / connection_threshold)
                    * (0.3 + 0.55 * depth)
                    * LINE_OPACITY_SCALE)
                    .min(0.6),
            });
        }
    }

    let mut dots = Vec::with_capacity(node_count + 1);
    for (index, node) in nodes.iter().copied().enumerate() {
        let (x, y, z) = project_web_point(node, time);
        let depth = (z + 1.0) * 0.5;
        let pulse = 1.0 + 0.25 * (time * 1.4 + index as f32 * 2.7).sin();
        dots.push(ActivityOrbDot {
            x,
            y,
            depth: z,
            radius: (NODE_RADIUS + NODE_RADIUS_DEPTH * depth) * pulse * radius_scale,
            ink: 0.55 - 0.45 * depth,
            opacity: 1.0,
        });
    }

    let segment = (time * 0.55).floor();
    let first = (hash_2d(segment, 1.7) * node_count as f32).floor() as usize;
    let second = (hash_2d(segment, 4.2) * node_count as f32).floor() as usize;
    if first != second {
        let progress = (time * 0.55).fract();
        let x = lerp(nodes[first].x, nodes[second].x, progress);
        let y = lerp(nodes[first].y, nodes[second].y, progress);
        let z = lerp(nodes[first].z, nodes[second].z, progress);
        let length = (x * x + y * y + z * z).sqrt().max(1e-6);
        let (x, y, z) = project_web_point(
            OrbNode {
                x: x / length,
                y: y / length,
                z: z / length,
            },
            time,
        );
        let depth = (z + 1.0) * 0.5;
        dots.push(ActivityOrbDot {
            x,
            y,
            depth: z,
            radius: (NODE_RADIUS * 1.5 + NODE_RADIUS_DEPTH * depth) * radius_scale,
            ink: 0.05,
            opacity: 0.5 + 0.5 * depth,
        });
    }

    dots.sort_by(|a, b| a.depth.total_cmp(&b.depth));
    ActivityOrbFrame { lines, dots }
}

/// Linear interpolation.
pub fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

/// A cell's phase, given the loader's raw phase and the cell's index.
pub fn staggered_phase(raw_delta: f32, index: usize, stagger: f32) -> f32 {
    (raw_delta - index as f32 * stagger).rem_euclid(1.0)
}

/// Cosine pulse: 0 at phase 0, 1 at phase 0.5, back to 0 at phase 1.
pub fn pulse_wave(phase: f32) -> f32 {
    0.5 - 0.5 * (phase * std::f32::consts::TAU).cos()
}

/// Jolt loader cell opacity for a phase: 0.08 → 1 → 0.08.
pub fn pulse_opacity(phase: f32) -> f32 {
    PULSE_MIN_OPACITY + (1.0 - PULSE_MIN_OPACITY) * pulse_wave(phase)
}

/// Jolt loader cell scale for a phase: 0.9 → 1 → 0.9.
pub fn pulse_scale(phase: f32) -> f32 {
    PULSE_MIN_SCALE + (1.0 - PULSE_MIN_SCALE) * pulse_wave(phase)
}

/// Gradient-spin cell opacity for a local phase `t` (0..1 of the period):
/// full at the cycle
/// start, easing down to `dim` by 45%, resting at `dim` until 92%, then rising
/// back to full — the per-cell phase offset sweeps this pulse across the grid.
pub fn gspin_opacity(t: f32, dim: f32) -> f32 {
    let t = t.rem_euclid(1.0);
    if t < 0.45 {
        lerp(1.0, dim, t / 0.45)
    } else if t < 0.92 {
        dim
    } else {
        lerp(dim, 1.0, (t - 0.92) / 0.08)
    }
}

/// The phase offset of a `(row, col)` cell in the 3×3 gradient spinner: the
/// pulse enters at the bottom edge and converges toward the top-centre cell, so
/// the wave reads as travelling upward.
pub fn gspin_cell_phase(row: usize, col: usize) -> f32 {
    let centre = (MATRIX_SIDE as f32 - 1.0) / 2.0;
    let max = MATRIX_SIDE as f32 - 1.0 + centre;
    let d = MATRIX_SIDE as f32 - 1.0 - row as f32 + (col as f32 - centre).abs();
    if max == 0.0 { 0.0 } else { d / (max + 1.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, what: &str) {
        assert!((a - b).abs() < 1e-5, "{what}: {a} vs {b}");
    }

    #[test]
    fn the_pulse_is_a_full_cosine_cycle() {
        close(pulse_wave(0.0), 0.0, "trough at 0");
        close(pulse_wave(0.5), 1.0, "crest at half");
        close(pulse_wave(1.0), 0.0, "trough at 1");
        // Opacity and scale ride the same wave between their own bounds.
        close(pulse_opacity(0.0), PULSE_MIN_OPACITY, "dim rest");
        close(pulse_opacity(0.5), 1.0, "full crest");
        close(pulse_scale(0.0), PULSE_MIN_SCALE, "small rest");
        close(pulse_scale(0.5), 1.0, "full scale");
    }

    #[test]
    fn stagger_offsets_each_cell_and_wraps() {
        close(staggered_phase(0.0, 0, PULSE_STAGGER), 0.0, "cell 0");
        close(
            staggered_phase(0.0, 1, PULSE_STAGGER),
            1.0 - PULSE_STAGGER,
            "cell 1 trails into the previous cycle",
        );
        // Phase is periodic: a whole extra turn changes nothing.
        close(
            staggered_phase(0.3, 2, PULSE_STAGGER),
            staggered_phase(1.3, 2, PULSE_STAGGER),
            "wraps",
        );
        // Always inside the unit interval, for any input.
        for raw in [-4.2f32, -0.1, 0.0, 0.5, 7.9] {
            for index in 0..JOLT_CELLS {
                let phase = staggered_phase(raw, index, PULSE_STAGGER);
                assert!((0.0..1.0).contains(&phase), "{raw} {index} -> {phase}");
            }
        }
    }

    #[test]
    fn gradient_spin_holds_dim_then_snaps_back() {
        close(gspin_opacity(0.0, GSPIN_DIM), 1.0, "starts full");
        close(gspin_opacity(0.45, GSPIN_DIM), GSPIN_DIM, "down by 45%");
        close(gspin_opacity(0.7, GSPIN_DIM), GSPIN_DIM, "rests dim");
        close(gspin_opacity(1.0, GSPIN_DIM), 1.0, "back to full");
        // Never leaves its bounds, at any phase.
        for step in 0..200 {
            let value = gspin_opacity(step as f32 / 100.0, GSPIN_DIM);
            assert!((GSPIN_DIM..=1.0).contains(&value), "{step} -> {value}");
        }
    }

    #[test]
    fn the_gradient_wave_travels_upward() {
        // The bottom row leads and the top-centre cell trails, which is what
        // makes the pulse read as rising.
        let bottom = gspin_cell_phase(MATRIX_SIDE - 1, 1);
        let top = gspin_cell_phase(0, 1);
        assert!(bottom < top, "bottom {bottom} should lead top {top}");
        // Symmetric about the centre column.
        close(gspin_cell_phase(1, 0), gspin_cell_phase(1, 2), "symmetry");
    }

    #[test]
    fn activity_orb_is_a_bounded_connecting_web() {
        let first = activity_orb_frame(0.37, 20.0);
        let later = activity_orb_frame(0.62, 20.0);
        assert!((12..=13).contains(&first.dots.len()));
        assert!(!first.lines.is_empty());
        assert!(first.lines.len() <= 66);

        for dot in &first.dots {
            assert!((0.0..=1.0).contains(&dot.x), "x={}", dot.x);
            assert!((0.0..=1.0).contains(&dot.y), "y={}", dot.y);
            assert!((-1.0..=1.0).contains(&dot.depth));
            assert!(dot.radius > 0.0);
            assert!((0.0..=1.0).contains(&dot.ink));
            assert!((0.0..=1.0).contains(&dot.opacity));
        }
        for line in &first.lines {
            assert!((0.0..=1.0).contains(&line.x1));
            assert!((0.0..=1.0).contains(&line.y1));
            assert!((0.0..=1.0).contains(&line.x2));
            assert!((0.0..=1.0).contains(&line.y2));
            assert!(line.width >= 0.9);
            assert!((0.0..=1.0).contains(&line.ink));
            assert!((0.0..=1.0).contains(&line.opacity));
        }
        assert_ne!(first.dots, later.dots, "the constellation should drift");
        assert!(
            first
                .dots
                .windows(2)
                .all(|pair| pair[0].depth <= pair[1].depth)
        );
    }

    #[test]
    fn the_mini_ring_visits_every_cell_once() {
        let mut seen: Vec<usize> = MINI_RING.iter().flatten().copied().collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..MINI_RING_LEN as usize).collect::<Vec<_>>());
    }
}
