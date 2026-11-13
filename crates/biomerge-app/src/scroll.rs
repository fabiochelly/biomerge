//! Wheel and touchpad scrolling from raw events: egui's smoothing gives each notch a long tail
//! that feels slow. Touchpad pixels apply at once and keep going after the fingers lift (Wayland
//! leaves kinetic scrolling to applications). A wheel notch moves a few rows, eased over about
//! 55 ms, faster and faster when notches come in quick succession.

use egui::{Event, MouseWheelUnit, TouchPhase, Ui, Vec2, vec2};

const NOTCH_ROWS: f32 = 4.0;
const NOTCH_EASE_SECONDS: f32 = 0.055;
const MAX_NOTCH_SPEED: f32 = 8.0;
const FLING_SLOWDOWN_SECONDS: f32 = 0.33;
const MIN_FLING_SPEED: f32 = 100.0;
const MAX_FLING_SPEED: f32 = 9000.0;
const STOP_FLING_SPEED: f32 = 20.0;

#[derive(Default)]
pub struct Scroller {
    remaining: (f32, f64),
    last_notch: f64,
    speed: f32,
    last_frame: f64,
    recent: Vec<(f64, Vec2)>,
    velocity: Vec2,
}

/// The scrollable position and its bounds.
pub struct Position<'a> {
    pub y: &'a mut f64,
    pub x: &'a mut f32,
    pub max_y: f64,
    pub max_x: f32,
    pub row_height: f32,
}

#[derive(Default)]
struct Frame {
    pixels: Vec2,
    notches: Vec2,
    moved: Vec2,
    lifted: bool,
    stop_fling: bool,
}

impl Scroller {
    pub fn update(&mut self, ui: &Ui, hovered: bool, target: Position<'_>) {
        let (now, dt, frame) = ui.input(|input| {
            let mut frame = Frame {
                stop_fling: input.pointer.any_pressed()
                    || input.events.iter().any(|e| matches!(e, Event::Key { pressed: true, .. })),
                ..Frame::default()
            };
            for event in &input.events {
                let Event::MouseWheel { unit, delta, phase, modifiers, .. } = event else {
                    continue;
                };
                // Ctrl+wheel is left to zooming.
                if modifiers.command {
                    continue;
                }
                let delta = if modifiers.shift && delta.x == 0.0 {
                    vec2(delta.y, 0.0)
                } else {
                    *delta
                };
                match (unit, phase) {
                    (MouseWheelUnit::Point, TouchPhase::End) => frame.lifted = true,
                    (MouseWheelUnit::Point, _) => {
                        frame.pixels += delta;
                        frame.moved += delta;
                        frame.stop_fling = true;
                    }
                    (MouseWheelUnit::Line, _) => {
                        frame.notches += delta;
                        frame.stop_fling = true;
                    }
                    (MouseWheelUnit::Page, _) => {
                        frame.pixels += delta * input.content_rect().height();
                        frame.stop_fling = true;
                    }
                }
            }
            (input.time, input.stable_dt.clamp(1.0 / 500.0, 0.05), frame)
        });

        let Position { y, x, max_y, max_x, row_height } = target;
        let mut apply = |dx: f32, dy: f64| {
            *y = (*y - dy).clamp(0.0, max_y);
            *x = (*x - dx).clamp(0.0, max_x);
        };

        if hovered {
            apply(frame.pixels.x, frame.pixels.y as f64);
            if frame.notches != Vec2::ZERO {
                let gap = now - self.last_notch;
                self.speed = if gap < 0.06 {
                    (self.speed * 1.6).clamp(1.0, MAX_NOTCH_SPEED)
                } else if gap > 0.2 {
                    1.0
                } else {
                    self.speed.max(1.0)
                };
                self.last_notch = now;
                let step = NOTCH_ROWS * row_height * self.speed;
                let distance = frame.notches * step;
                self.remaining.0 += distance.x;
                // Reversing the wheel drops what was left the other way.
                let reversed = self.remaining.1 != 0.0
                    && self.remaining.1.signum() != distance.y.signum() as f64;
                self.remaining.1 = if reversed {
                    distance.y as f64
                } else {
                    self.remaining.1 + distance.y as f64
                };
            }
        }

        let elapsed = (now - self.last_frame) as f32;
        if self.remaining != (0.0, 0.0) {
            let step_dt = if elapsed > 0.1 { dt } else { elapsed.max(1.0 / 500.0) };
            let share = 1.0 - (-step_dt / NOTCH_EASE_SECONDS).exp();
            let mut step = (self.remaining.0 * share, self.remaining.1 * share as f64);
            if self.remaining.0.abs() < 0.5 {
                step.0 = self.remaining.0;
            }
            if self.remaining.1.abs() < 0.5 {
                step.1 = self.remaining.1;
            }
            self.remaining.0 -= step.0;
            self.remaining.1 -= step.1;
            apply(step.0, step.1);
            ui.ctx().request_repaint();
        }

        if frame.stop_fling {
            self.velocity = Vec2::ZERO;
        }
        if frame.moved != Vec2::ZERO && hovered {
            self.recent.push((now, frame.moved));
        }
        self.recent.retain(|motion| motion.0 >= now - 0.1);
        if frame.lifted && hovered {
            if let (Some(first), Some(last)) = (self.recent.first(), self.recent.last()) {
                // Only if the fingers were still moving when they lifted.
                if now - last.0 < 0.05 {
                    let span = (last.0 - first.0) as f32 + dt;
                    let total = self.recent.iter().fold(Vec2::ZERO, |sum, m| sum + m.1);
                    let velocity = total / span;
                    if velocity.length() > MIN_FLING_SPEED {
                        self.velocity = velocity
                            .clamp(Vec2::splat(-MAX_FLING_SPEED), Vec2::splat(MAX_FLING_SPEED));
                    }
                }
            }
            self.recent.clear();
        }
        if self.velocity != Vec2::ZERO {
            let step_dt = elapsed.clamp(0.0, 0.05);
            let step = self.velocity * step_dt;
            apply(step.x, step.y as f64);
            self.velocity *= (-step_dt / FLING_SLOWDOWN_SECONDS).exp();
            if self.velocity.length() < STOP_FLING_SPEED {
                self.velocity = Vec2::ZERO;
            } else {
                ui.ctx().request_repaint();
            }
        }
        self.last_frame = now;
    }
}
