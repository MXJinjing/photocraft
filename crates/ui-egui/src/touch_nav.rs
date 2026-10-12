//! Two-or-more-finger canvas navigation.
//!
//! A second finger on the canvas cancels any in-progress tool and keeps only pan and pinch-zoom
//! until every finger is lifted. One finger still drives the current tool once a short grace
//! window has passed (so a staggered pinch never deposits ink at the first press). Pinch zoom
//! itself is applied by [`crate::wheel_nav`] (egui's multi-touch zoom factor).

use egui::{Context, Event, Id, Pos2, TouchPhase, Vec2};

use crate::PhotocraftApp;

/// Frames to hold off a lone-finger tool after Touch Start so a second finger can arrive.
/// ~4 frames ≈ 64 ms at 60 Hz — enough for a staggered pinch, short enough for paint/pan.
const PINCH_GRACE_FRAMES: u8 = 4;

/// Navigation is active this frame if it already was, or a second finger just landed. The lift
/// frame must still count as navigation so a pointer-up cannot commit the cancelled tool.
pub fn active_this_frame(was_nav: bool, touches: usize) -> bool {
    was_nav || touches >= 2
}

/// Stay in navigation while any finger is still down.
///
/// A leftover Primary after the last finger lifts must *not* keep the latch: that previously
/// delayed or swallowed a rapid second pinch/pan on touch screens.
pub fn next_latch(nav_this_frame: bool, touches: usize, pointer_down: bool) -> bool {
    let _ = pointer_down;
    touches >= 2 || (nav_this_frame && touches > 0)
}

/// How many fingers egui currently tracks.
pub fn touch_count(ctx: &Context) -> usize {
    ctx.input(|i| i.multi_touch().map_or(usize::from(i.any_touches()), |m| m.num_touches))
}

fn grace_id() -> Id {
    Id::new("pc-touch-pinch-grace")
}

fn arm_grace(ctx: &Context) {
    ctx.data_mut(|d| d.insert_temp(grace_id(), PINCH_GRACE_FRAMES));
}

fn clear_grace(ctx: &Context) {
    ctx.data_mut(|d| {
        d.remove::<u8>(grace_id());
    });
}

/// Count down the lone-finger grace window; `true` while tools must still stay suppressed.
fn tick_grace(ctx: &Context) -> bool {
    let left = ctx.data(|d| d.get_temp::<u8>(grace_id())).unwrap_or(0);
    if left == 0 {
        return false;
    }
    let next = left.saturating_sub(1);
    if next == 0 {
        clear_grace(ctx);
    } else {
        ctx.data_mut(|d| d.insert_temp(grace_id(), next));
    }
    true
}

/// Origin eligibility belongs to a canvas, and remains fixed until all contacts are lifted.
#[derive(Clone, Default)]
struct Origins {
    contacts: Vec<(egui::TouchDeviceId, egui::TouchId)>,
    blocked: bool,
}

impl Origins {
    fn frame(&mut self, events: &[Event], free: impl Fn(Pos2) -> bool) -> bool {
        let mut eligible = !self.contacts.is_empty() && !self.blocked;
        for event in events {
            if let Event::Touch { device_id, id, phase, pos, .. } = event {
                let key = (*device_id, *id);
                match phase {
                    TouchPhase::Start => {
                        if self.contacts.is_empty() {
                            self.blocked = false;
                        }
                        if !self.contacts.contains(&key) {
                            self.contacts.push(key);
                            self.blocked |= !free(*pos);
                        }
                        eligible = !self.blocked;
                    }
                    TouchPhase::Move => {}
                    TouchPhase::End | TouchPhase::Cancel => self.contacts.retain(|k| *k != key),
                }
            }
        }
        eligible && !self.blocked
    }
}

/// Both fingers must start on this canvas's unobstructed surface. Moving in from a panel or
/// dialog cannot acquire it; a gesture that started here can continue outside the canvas.
pub fn eligible(ctx: &Context, response: &egui::Response, under_dialog: bool) -> bool {
    let id = response.id.with("touch-origins");
    let mut origins = ctx.data(|d| d.get_temp::<Origins>(id)).unwrap_or_default();
    let events = ctx.input(|i| i.events.clone());
    let eligible = origins.frame(&events, |p| {
        response.rect.contains(p)
            && if under_dialog {
                crate::dialogs::free_position(ctx, response.rect, p)
            } else {
                ctx.layer_id_at(p).is_none_or(|layer| layer == response.layer_id)
            }
    });
    ctx.data_mut(|d| d.insert_temp(id, origins));
    eligible
}

/// Update the latch, drop any in-progress tool, and return whether this frame is navigation-only.
///
/// Acceptance:
/// 1. Rapid consecutive two-finger zoom/pan must work (latch does not stick on leftover Primary).
/// 2. Before and after touch-nav starts, finger presses must not ink — lone-finger Touch Start
///    arms a short grace that aborts tools until a second finger arrives or the window expires.
pub fn update(app: &mut PhotocraftApp, ctx: &Context, eligible: bool) -> bool {
    if !eligible {
        app.touch_nav = false;
        clear_grace(ctx);
        return false;
    }
    let touches = touch_count(ctx);
    let pointer_down = ctx.input(|i| i.pointer.any_down());
    let lone_start = touches == 1
        && ctx.input(|i| i.events.iter().any(|e| matches!(e, Event::Touch { phase: TouchPhase::Start, .. })));
    if touches >= 2 || touches == 0 {
        clear_grace(ctx);
    } else if lone_start && !app.touch_nav {
        arm_grace(ctx);
    }
    let grace = tick_grace(ctx);
    let multi_nav = active_this_frame(app.touch_nav, touches);
    let nav = multi_nav || grace;
    if nav {
        abort_tools(app, ctx);
    }
    app.touch_nav = next_latch(multi_nav, touches, pointer_down);
    nav
}

/// Centroid pan of a two-finger gesture (points). `None` when only one finger remains.
pub fn pan_delta(ctx: &Context) -> Option<Vec2> {
    ctx.input(|i| i.multi_touch().map(|m| m.translation_delta)).filter(|d| d.x.is_finite() && d.y.is_finite() && *d != Vec2::ZERO)
}

/// Drop an in-progress tool without committing it.
pub fn abort_tools(app: &mut PhotocraftApp, ctx: &Context) {
    app.drag = None;
    app.live_stroke = None;
    app.trail = None;
    app.move_preview = None;
    app.alt_sampling = false;
    app.brush_resize = None;
    app.brush_resize_armed = false;
    app.secondary_erase = false;
    app.guide_drag = None;
    crate::move_mods::finish(app);
    crate::crop_ui::abort_drag(app);
    crate::gradient_ui::abort_drag(app);
    crate::transform_tool::abort_pointer(app);
    crate::zoom_tool::abort(ctx);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(id: u64, phase: TouchPhase, x: f32) -> Event {
        Event::Touch { device_id: egui::TouchDeviceId(0), id: egui::TouchId(id), phase, pos: egui::pos2(x, 50.0), force: None }
    }

    #[test]
    fn outside_origins_cannot_acquire_canvas_after_moving_inside() {
        let mut origins = Origins::default();
        let free = |p: Pos2| p.x >= 100.0 && p.x <= 500.0;
        assert!(!origins.frame(&[touch(1, TouchPhase::Start, 50.0)], free));
        assert!(!origins.frame(&[touch(1, TouchPhase::Move, 200.0), touch(2, TouchPhase::Start, 300.0)], free));
        assert!(!origins.frame(&[touch(1, TouchPhase::End, 200.0)], free));
        assert!(!origins.frame(&[touch(3, TouchPhase::Start, 400.0)], free));
        assert!(!origins.frame(&[touch(2, TouchPhase::Cancel, 300.0), touch(3, TouchPhase::End, 400.0)], free));
        assert!(origins.frame(&[touch(1, TouchPhase::Start, 200.0), touch(2, TouchPhase::Start, 300.0)], free));
        assert!(origins.frame(&[touch(1, TouchPhase::Move, 10.0), touch(2, TouchPhase::Move, 600.0)], free));
        assert!(origins.frame(&[touch(1, TouchPhase::End, 10.0), touch(2, TouchPhase::End, 600.0)], free));
        assert!(!origins.frame(&[], free));
    }

    #[test]
    fn second_finger_on_overlay_rejects_even_when_centroid_is_on_canvas() {
        let mut origins = Origins::default();
        let free = |p: Pos2| p.x < 300.0;
        assert!(origins.frame(&[touch(1, TouchPhase::Start, 100.0)], free));
        assert!(!origins.frame(&[touch(2, TouchPhase::Start, 350.0)], free));
        assert!(!origins.frame(&[touch(2, TouchPhase::Move, 150.0)], free));
    }

    #[test]
    fn a_second_finger_enters_navigation() {
        assert!(!active_this_frame(false, 0));
        assert!(!active_this_frame(false, 1));
        assert!(active_this_frame(false, 2));
        assert!(active_this_frame(false, 3));
        assert!(active_this_frame(true, 1));
        assert!(active_this_frame(true, 0), "the lift frame must still block the tool");
    }

    #[test]
    fn latch_holds_while_fingers_remain_not_on_leftover_primary() {
        assert!(next_latch(true, 2, true));
        assert!(next_latch(true, 1, true), "the remaining finger still navigates");
        assert!(
            !next_latch(true, 0, true),
            "leftover Primary alone must not keep the latch (blocks consecutive pinch)"
        );
        assert!(!next_latch(true, 0, false));
        assert!(!next_latch(false, 1, true));
        assert!(next_latch(false, 2, true));
    }

    #[test]
    fn hostile_counts_never_enter_navigation_from_one_finger() {
        for n in [0, 1] {
            assert!(!active_this_frame(false, n));
            assert!(!next_latch(false, n, false));
        }
    }
}
