//! Orbit camera with damping and a fly-to-centroid on selection.
//!
//! Constants are the codebase-memory-mcp galaxy's (ported, MIT; see
//! third_party/codebase-memory-mcp/UPSTREAM), as recorded in
//! the analysis of that UI, section 4
//! ("Scene and camera", "Selection"): camera at z=800, field of view 50, far plane
//! 100000, damping 0.08, rotate speed 0.5, zoom speed 1.5, distance 10 to 50000, fly-to
//! lerp 0.08 per frame with the pivot moved in step. The damping model follows three.js
//! `OrbitControls` (MIT), which that UI uses: each frame applies a fraction of the
//! pending delta and keeps the rest.
//!
//! The reference steps per display frame. Here each step is scaled to elapsed time
//! (`1 - (1 - k)^(dt * 60)`), so a 120 Hz display and a 30 Hz software path settle in
//! the same wall-clock time instead of the software path feeling sluggish.
//!
//! State machine (why `update` returns a flag):
//! ```text
//!   Resting --rotate/pan input--> Damping --deltas below epsilon--> Resting
//!   Resting --fly_to-----------> Flying  --within epsilon of goal--> Resting
//!   Flying  --zoom/pan input---> Damping (the fly is cancelled; rotate keeps it)
//! ```
//! `update` returns `true` while not Resting; the caller repaints only then.

use glam::{Mat4, Vec3};

pub const FOV_Y_DEGREES: f32 = 50.0;
pub const NEAR: f32 = 1.0;
pub const FAR: f32 = 100_000.0;
pub const MIN_DISTANCE: f32 = 10.0;
pub const MAX_DISTANCE: f32 = 50_000.0;
pub const INITIAL_DISTANCE: f32 = 800.0;
pub const DAMPING: f32 = 0.08;
pub const ROTATE_SPEED: f32 = 0.5;
pub const ZOOM_SPEED: f32 = 1.5;
/// Fly-to lerp per 60 Hz frame.
pub const FLY_LERP: f32 = 0.08;
/// Fly-to distance is `max(FLY_SPREAD_FACTOR * spread, FLY_MIN_DISTANCE)`.
pub const FLY_SPREAD_FACTOR: f32 = 3.0;
/// The evidence records "a floor" without its value. 120 world units frames a single
/// node (core radius 4 to 20) with its immediate cluster; chosen, not measured.
pub const FLY_MIN_DISTANCE: f32 = 120.0;
/// Wheel zoom: distance factor per point of scroll, before `ZOOM_SPEED`. Chosen so one
/// typical wheel notch (about 50 points) changes distance by about 14%.
const ZOOM_PER_POINT: f32 = 0.002;
/// Pitch stays short of the poles so the up vector never degenerates.
const MAX_PITCH: f32 = std::f32::consts::FRAC_PI_2 - 0.01;
/// Rotation below this (radians) counts as settled.
const ROTATE_EPSILON: f32 = 1e-5;
/// Reference frame rate the per-frame constants were tuned at.
const REFERENCE_HZ: f32 = 60.0;

/// Fraction of the remaining distance a per-frame factor `k` covers in `dt` seconds.
fn step_fraction(k: f32, dt: f32) -> f32 {
    1.0 - (1.0 - k).powf(dt.max(0.0) * REFERENCE_HZ)
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct FlyTo {
    target: Vec3,
    distance: f32,
}

/// What the renderer and the picker need from the camera for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraFrame {
    pub view_proj: Mat4,
    pub eye: Vec3,
    pub forward: Vec3,
    pub right: Vec3,
    pub up: Vec3,
    /// Target size in physical pixels.
    pub viewport: [f32; 2],
    /// World size of one pixel at depth 1 along `forward` (multiply by depth).
    pub pixel_world_size_per_depth: f32,
}

impl CameraFrame {
    /// Pixel position (origin top left) and view depth of a world point, or `None`
    /// when it is behind the near plane.
    pub fn project(&self, p: Vec3) -> Option<([f32; 2], f32)> {
        let clip = self.view_proj * p.extend(1.0);
        if clip.w <= NEAR * 0.5 {
            return None;
        }
        let ndc = clip.truncate() / clip.w;
        let x = (ndc.x * 0.5 + 0.5) * self.viewport[0];
        let y = (0.5 - ndc.y * 0.5) * self.viewport[1];
        Some(([x, y], clip.w))
    }
}

/// Orbit camera around a pivot (`target`).
#[derive(Debug, Clone, PartialEq)]
pub struct OrbitCamera {
    target: Vec3,
    distance: f32,
    yaw: f32,
    pitch: f32,
    yaw_delta: f32,
    pitch_delta: f32,
    pan_delta: Vec3,
    fly: Option<FlyTo>,
}

impl Default for OrbitCamera {
    /// Looking down -Z at the origin from z = 800, the reference's start.
    fn default() -> Self {
        Self {
            target: Vec3::ZERO,
            distance: INITIAL_DISTANCE,
            yaw: 0.0,
            pitch: 0.0,
            yaw_delta: 0.0,
            pitch_delta: 0.0,
            pan_delta: Vec3::ZERO,
            fly: None,
        }
    }
}

impl OrbitCamera {
    pub fn target(&self) -> Vec3 {
        self.target
    }

    pub fn distance(&self) -> f32 {
        self.distance
    }

    pub fn is_flying(&self) -> bool {
        self.fly.is_some()
    }

    /// Unit vector from the pivot to the eye.
    fn offset_dir(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        Vec3::new(cp * sy, sp, cp * cy)
    }

    pub fn eye(&self) -> Vec3 {
        self.target + self.offset_dir() * self.distance
    }

    /// Queues a rotation from a pointer drag of `dx, dy` pixels over a viewport
    /// `viewport_h` pixels tall (three.js: `2 * PI * d / height * rotateSpeed`).
    pub fn rotate(&mut self, dx: f32, dy: f32, viewport_h: f32) {
        let k = std::f32::consts::TAU * ROTATE_SPEED / viewport_h.max(1.0);
        self.yaw_delta -= dx * k;
        self.pitch_delta += dy * k;
    }

    /// Queues a pan by a drag of `dx, dy` pixels; the pivot moves in the view plane.
    /// Cancels a fly-to.
    pub fn pan(&mut self, dx: f32, dy: f32, viewport_h: f32) {
        self.fly = None;
        let world_per_px = self.pixel_world_size_per_depth(viewport_h) * self.distance;
        let (right, up) = self.right_up();
        self.pan_delta += (-dx * right + dy * up) * world_per_px;
    }

    /// Wheel zoom by `scroll_points` (positive zooms in). Immediate, clamped.
    /// Cancels a fly-to.
    pub fn zoom_scroll(&mut self, scroll_points: f32) {
        // Clamped exponent: a huge scroll must hit the distance clamp, not underflow
        // to a zero factor that `zoom_factor` refuses.
        let exponent = (-scroll_points * ZOOM_PER_POINT * ZOOM_SPEED).clamp(-20.0, 20.0);
        self.zoom_factor(exponent.exp());
    }

    /// Multiplies the distance by `factor` (below 1 zooms in), clamped.
    pub fn zoom_factor(&mut self, factor: f32) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        self.fly = None;
        self.distance = (self.distance * factor).clamp(MIN_DISTANCE, MAX_DISTANCE);
    }

    /// A camera looking down -Z at `center`, far enough to frame a sphere of `radius`,
    /// never closer than the reference's start distance of 800.
    pub fn framing(center: Vec3, radius: f32) -> Self {
        let fit = radius.max(0.0) / (FOV_Y_DEGREES.to_radians() * 0.5).sin();
        Self {
            target: center,
            distance: fit.max(INITIAL_DISTANCE).clamp(MIN_DISTANCE, MAX_DISTANCE),
            ..Self::default()
        }
    }

    /// Starts a flight to `centroid`, framing a selection of radius `spread`. The pivot
    /// and the distance move together, so the orbit center follows the flight.
    pub fn fly_to(&mut self, centroid: Vec3, spread: f32) {
        let distance = (FLY_SPREAD_FACTOR * spread.max(0.0))
            .max(FLY_MIN_DISTANCE)
            .clamp(MIN_DISTANCE, MAX_DISTANCE);
        self.pan_delta = Vec3::ZERO;
        self.fly = Some(FlyTo {
            target: centroid,
            distance,
        });
    }

    /// Advances damping and flight by `dt` seconds. Returns `true` while the camera is
    /// still moving, which is the only time the view needs another frame.
    pub fn update(&mut self, dt: f32) -> bool {
        let damp = step_fraction(DAMPING, dt);
        self.yaw += self.yaw_delta * damp;
        self.pitch = (self.pitch + self.pitch_delta * damp).clamp(-MAX_PITCH, MAX_PITCH);
        self.yaw_delta *= 1.0 - damp;
        self.pitch_delta *= 1.0 - damp;
        self.target += self.pan_delta * damp;
        self.pan_delta *= 1.0 - damp;

        // Settled thresholds scale with distance: a sub-pixel move is not motion.
        let still = 1e-4 * self.distance;
        if self.yaw_delta.abs() < ROTATE_EPSILON {
            self.yaw_delta = 0.0;
        }
        if self.pitch_delta.abs() < ROTATE_EPSILON {
            self.pitch_delta = 0.0;
        }
        if self.pan_delta.length() < still {
            self.pan_delta = Vec3::ZERO;
        }

        if let Some(goal) = self.fly {
            let f = step_fraction(FLY_LERP, dt);
            self.target += (goal.target - self.target) * f;
            self.distance += (goal.distance - self.distance) * f;
            let eps = 1e-3 * goal.distance;
            if self.target.distance(goal.target) < eps
                && (self.distance - goal.distance).abs() < eps
            {
                self.target = goal.target;
                self.distance = goal.distance;
                self.fly = None;
            }
        }

        self.yaw_delta != 0.0
            || self.pitch_delta != 0.0
            || self.pan_delta != Vec3::ZERO
            || self.fly.is_some()
    }

    fn right_up(&self) -> (Vec3, Vec3) {
        let forward = -self.offset_dir();
        let right = forward.cross(Vec3::Y).normalize_or(Vec3::X);
        let up = right.cross(forward);
        (right, up)
    }

    pub fn pixel_world_size_per_depth(&self, viewport_h: f32) -> f32 {
        2.0 * (FOV_Y_DEGREES.to_radians() * 0.5).tan() / viewport_h.max(1.0)
    }

    /// The camera for a `width` by `height` pixel target.
    pub fn frame(&self, width: f32, height: f32) -> CameraFrame {
        let eye = self.eye();
        let forward = -self.offset_dir();
        let (right, up) = self.right_up();
        let view = glam::camera::rh::view::look_at_mat4(eye, self.target, Vec3::Y);
        let aspect = width.max(1.0) / height.max(1.0);
        let proj = glam::camera::rh::proj::directx::perspective(
            FOV_Y_DEGREES.to_radians(),
            aspect,
            NEAR,
            FAR,
        );
        CameraFrame {
            view_proj: proj * view,
            eye,
            forward,
            right,
            up,
            viewport: [width, height],
            pixel_world_size_per_depth: self.pixel_world_size_per_depth(height),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settle(cam: &mut OrbitCamera, dt: f32) -> usize {
        for i in 0..10_000 {
            if !cam.update(dt) {
                return i;
            }
        }
        panic!("camera never settled: {cam:?}");
    }

    #[test]
    fn starts_at_z_800_looking_at_origin() {
        let cam = OrbitCamera::default();
        assert!((cam.eye() - Vec3::new(0.0, 0.0, 800.0)).length() < 1e-3);
        let f = cam.frame(800.0, 600.0);
        let (px, depth) = f.project(Vec3::ZERO).expect("in front");
        assert!((px[0] - 400.0).abs() < 1e-2 && (px[1] - 300.0).abs() < 1e-2);
        assert!((depth - 800.0).abs() < 1e-2);
    }

    #[test]
    fn damped_rotation_applies_the_whole_drag_then_rests() {
        let mut cam = OrbitCamera::default();
        cam.rotate(100.0, 0.0, 600.0);
        let expected = -100.0 * std::f32::consts::TAU * ROTATE_SPEED / 600.0;
        assert!(cam.update(1.0 / 60.0), "moving after input");
        // The first step applies only the damping fraction, not the whole drag.
        assert!((cam.yaw - expected * DAMPING).abs() < 1e-5, "{}", cam.yaw);
        settle(&mut cam, 1.0 / 60.0);
        assert!(
            (cam.yaw - expected).abs() < 1e-3,
            "{} vs {expected}",
            cam.yaw
        );
        assert!(!cam.update(1.0 / 60.0), "at rest no further frames");
    }

    #[test]
    fn damping_is_frame_rate_independent() {
        let run = |hz: f32, secs: f32| {
            let mut cam = OrbitCamera::default();
            cam.rotate(200.0, 0.0, 600.0);
            for _ in 0..(hz * secs) as usize {
                cam.update(1.0 / hz);
            }
            cam.yaw
        };
        // 0.5 s is a whole number of frames at both rates (60 and 15).
        let a = run(120.0, 0.5);
        let b = run(30.0, 0.5);
        assert!((a - b).abs() < 1e-4, "120 Hz {a} vs 30 Hz {b}");
    }

    #[test]
    fn fly_to_frames_three_times_spread_with_a_floor() {
        let mut cam = OrbitCamera::default();
        let goal = Vec3::new(300.0, -50.0, 100.0);
        cam.fly_to(goal, 200.0);
        assert!(cam.update(1.0 / 60.0));
        // One 60 Hz step covers the lerp fraction of the way, pivot and distance alike.
        let expected = goal * FLY_LERP;
        assert!(
            (cam.target() - expected).length() < 1e-3,
            "{:?}",
            cam.target()
        );
        settle(&mut cam, 1.0 / 60.0);
        assert_eq!(cam.target(), goal);
        assert_eq!(cam.distance(), 600.0);

        cam.fly_to(goal, 1.0);
        settle(&mut cam, 1.0 / 60.0);
        assert_eq!(cam.distance(), FLY_MIN_DISTANCE);
    }

    #[test]
    fn framing_fits_the_scene_but_never_starts_closer_than_800() {
        assert_eq!(
            OrbitCamera::framing(Vec3::ZERO, 10.0),
            OrbitCamera::default()
        );
        let cam = OrbitCamera::framing(Vec3::new(5.0, 0.0, 0.0), 2000.0);
        assert_eq!(cam.target(), Vec3::new(5.0, 0.0, 0.0));
        // The whole sphere fits the vertical field of view.
        let half_fov = (cam.distance() / 2000.0).recip().asin();
        assert!(half_fov <= FOV_Y_DEGREES.to_radians() * 0.5 + 1e-4);
    }

    #[test]
    fn zoom_is_clamped_and_cancels_flight() {
        let mut cam = OrbitCamera::default();
        cam.fly_to(Vec3::X * 1000.0, 10.0);
        cam.zoom_scroll(1e6);
        assert_eq!(cam.distance(), MIN_DISTANCE);
        assert!(!cam.is_flying());
        cam.zoom_scroll(-1e6);
        assert_eq!(cam.distance(), MAX_DISTANCE);
    }

    #[test]
    fn pan_moves_the_pivot_in_the_view_plane() {
        let mut cam = OrbitCamera::default();
        cam.pan(-100.0, 0.0, 600.0);
        settle(&mut cam, 1.0 / 60.0);
        // Dragging left by 100 px moves the pivot right (+X) by 100 px of world at 800.
        let world = 100.0 * cam.pixel_world_size_per_depth(600.0) * 800.0;
        assert!(
            (cam.target().x - world).abs() < 0.05 * world,
            "{:?}",
            cam.target()
        );
        assert!(cam.target().y.abs() < 1e-3 && cam.target().z.abs() < 1e-3);
    }
}
