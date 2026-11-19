//! Outlines around windows, in the style of Hyprland's borders.
//!
//! Every managed window on a visible workspace gets its own outline. The focused one is drawn in
//! the active color and the rest in the inactive color. When focus moves, nothing travels across
//! the screen: the old outline fades to the inactive color and the new one to the active color.
//!
//! Each outline lives in a wave-owned WindowServer window, a little larger than its window and
//! ordered just below it, so the window covers everything but the outline. Core Animation draws
//! the outline from the layer's border properties, with no bitmap, so it costs the same whatever
//! the window's size. Border windows are kept in a small pool and reused.
//!
//! The animation thread owns the borders through a [`BorderFollower`]. On every display tick it
//! moves each border to the frame it just sent that window, so the two move together.

use std::collections::hash_map::Entry;
use std::time::Duration;

use cgs::compositor::{
    CgsWindow, CgsWindowError, WindowSurface, WindowTransaction, with_disabled_actions,
};
use objc2::rc::{Retained, autoreleasepool};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGColor;
use objc2_quartz_core::{CALayer, CAMediaTimingFunction, CATransaction};

use crate::actor::app::WindowId;
use crate::common::collections::HashMap;
use crate::common::config::{BorderSettings, parse_hex_color};
use crate::sys::geometry::SameAs;
use crate::sys::window_server::WindowServerId;

/// How far the outline reaches in under its window, in points. macOS draws a window's visible edge
/// a little inside its frame, so an outline that stopped at the frame would leave a thin line of
/// background between them. The window covers this part.
const OVERLAP: f64 = 1.0;

/// Hidden border windows kept for reuse. More than this are freed.
const SPARE_BORDERS: usize = 4;

/// One window that gets a border.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BorderedWindow {
    pub wid: WindowId,
    pub wsid: WindowServerId,
    /// Where the window rests. While it animates, its border follows the animated frame instead.
    pub frame: CGRect,
}

/// Everything the borders show, sent from the reactor to the animation thread.
#[derive(Debug, Clone, PartialEq)]
pub struct BorderScene {
    pub windows: Vec<BorderedWindow>,
    pub focused: Option<WindowId>,
    pub settings: BorderSettings,
    /// How long a color change takes when focus moves.
    pub fade: Duration,
    /// The fade's timing curve, as cubic bezier control points.
    pub curve: [f32; 4],
}

/// The parts of [`BorderSettings`] every outline shares.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Style {
    width: f64,
    radius: f64,
    active: [f64; 4],
    inactive: [f64; 4],
}

impl Style {
    /// `None` for settings that failed validation; the config loader reports those.
    fn new(settings: &BorderSettings) -> Option<Self> {
        let active = parse_hex_color(&settings.active_color)?;
        let inactive = if settings.inactive {
            parse_hex_color(&settings.inactive_color)?
        } else {
            // Fade out in the active color's own hue, so the fade doesn't pass through gray.
            let [r, g, b, _] = active;
            [r, g, b, 0.0]
        };
        Some(Self {
            width: settings.width,
            radius: settings.radius,
            active,
            inactive,
        })
    }

    /// The outline's outside corner radius: the window's, plus the width around it.
    fn outer_radius(self) -> f64 { self.radius + self.width }

    fn color(self, focused: bool) -> [f64; 4] { if focused { self.active } else { self.inactive } }
}

/// Owns the borders on the animation thread and keeps each on its window's current frame.
#[derive(Default)]
pub struct BorderFollower {
    borders: HashMap<WindowId, Border>,
    spare: Vec<Border>,
    scene: Option<BorderScene>,
}

impl BorderFollower {
    /// Shows `scene`, or with `None` releases every border window.
    pub fn set_scene(&mut self, scene: Option<BorderScene>) {
        let Some(scene) = scene else {
            self.borders.clear();
            self.spare.clear();
            self.scene = None;
            return;
        };
        let Some(style) = Style::new(&scene.settings) else {
            self.set_scene(None);
            return;
        };
        let focus_moved = self.scene.as_ref().is_none_or(|old| old.focused != scene.focused);
        let result = autoreleasepool(|_| -> Result<(), CgsWindowError> {
            // Windows that left the scene give their borders back to the pool.
            let gone: Vec<WindowId> = self
                .borders
                .keys()
                .filter(|wid| !scene.windows.iter().any(|w| w.wid == **wid))
                .copied()
                .collect();
            for wid in gone {
                if let Some(mut border) = self.borders.remove(&wid) {
                    border.hide();
                    if self.spare.len() < SPARE_BORDERS {
                        self.spare.push(border);
                    }
                }
            }
            for window in &scene.windows {
                let focused = scene.focused == Some(window.wid);
                match self.borders.entry(window.wid) {
                    Entry::Occupied(mut entry) => {
                        let border = entry.get_mut();
                        border.restyle(style);
                        border.set_color(style.color(focused), Some((scene.fade, scene.curve)));
                        // A newly focused window is raised; keep its border just below it.
                        if focused && focus_moved {
                            border.order_below(window.wsid)?;
                        }
                    }
                    Entry::Vacant(entry) => {
                        let mut border = match self.spare.pop() {
                            Some(border) => border,
                            None => Border::new()?,
                        };
                        border.restyle(style);
                        border.set_color(style.color(focused), None);
                        border.place(window.frame, style)?;
                        border.order_below(window.wsid)?;
                        entry.insert(border);
                    }
                }
            }
            Ok(())
        });
        if let Err(error) = result {
            // Wait for the reactor's next scene rather than retrying on every tick.
            tracing::warn!(?error, "failed to update borders");
            self.set_scene(None);
            return;
        }
        self.scene = Some(scene);
    }

    /// Moves each border to its window. `animated` gives the frame a moving window was just sent.
    pub fn follow(&mut self, animated: impl Fn(WindowId) -> Option<CGRect>) {
        let Some(scene) = &self.scene else { return };
        let Some(style) = Style::new(&scene.settings) else {
            return;
        };
        let result = autoreleasepool(|_| -> Result<(), CgsWindowError> {
            for window in &scene.windows {
                if let Some(border) = self.borders.get_mut(&window.wid) {
                    border.place(animated(window.wid).unwrap_or(window.frame), style)?;
                }
            }
            Ok(())
        });
        if let Err(error) = result {
            tracing::warn!(?error, "failed to move borders");
            self.set_scene(None);
        }
    }
}

/// One border window, hidden until it's ordered below a window.
struct Border {
    // The surface must be detached before its owning WindowServer window drops.
    surface: WindowSurface,
    window: CgsWindow,
    root: Retained<CALayer>,
    /// The border window's own frame: the target window's frame grown by the width on each side.
    frame: CGRect,
    style: Option<Style>,
    color: Option<[f64; 4]>,
}

impl Border {
    fn new() -> Result<Self, CgsWindowError> {
        let frame = CGRect::new(CGPoint::ZERO, CGSize::new(1., 1.));
        let window = CgsWindow::new_compositor(frame, 0.)?;
        window.disable_shadow();
        window.set_tags(1 << 3)?;
        let root = CALayer::layer();
        with_disabled_actions(|| {
            root.setBounds(bounds(frame.size));
            root.setPosition(CGPoint::ZERO);
            root.setAnchorPoint(CGPoint::ZERO);
            root.setContentsScale(2.0);
            root.setOpaque(false);
        });
        let surface = WindowSurface::new(window.id(), bounds(frame.size), &root)?;
        surface.flush();
        Ok(Self {
            surface,
            window,
            root,
            frame,
            style: None,
            color: None,
        })
    }

    fn restyle(&mut self, style: Style) {
        if self.style == Some(style) {
            return;
        }
        with_disabled_actions(|| {
            self.root.setCornerRadius(style.outer_radius());
            self.root.setBorderWidth(style.width + OVERLAP);
        });
        // A new width or radius changes the window's shape, so `place` must redo it.
        self.frame = CGRect::ZERO;
        self.style = Some(style);
    }

    /// Changes the outline's color, fading over `fade` on its curve, or at once with `None`.
    fn set_color(&mut self, color: [f64; 4], fade: Option<(Duration, [f32; 4])>) {
        if self.color == Some(color) {
            return;
        }
        let [r, g, b, a] = color;
        let cg: Retained<CGColor> = CGColor::new_generic_rgb(r, g, b, a).into();
        match fade {
            Some((duration, [x1, y1, x2, y2])) if !duration.is_zero() => {
                CATransaction::begin();
                CATransaction::setAnimationDuration(duration.as_secs_f64());
                CATransaction::setAnimationTimingFunction(Some(
                    &CAMediaTimingFunction::functionWithControlPoints(x1, y1, x2, y2),
                ));
                self.root.setBorderColor(Some(&cg));
                CATransaction::commit();
            }
            _ => with_disabled_actions(|| self.root.setBorderColor(Some(&cg))),
        }
        self.surface.flush();
        self.color = Some(color);
    }

    /// Puts the outline around a window whose frame is `target`.
    fn place(&mut self, target: CGRect, style: Style) -> Result<(), CgsWindowError> {
        let w = style.width;
        let frame = CGRect::new(
            CGPoint::new(target.origin.x - w, target.origin.y - w),
            CGSize::new(target.size.width + 2. * w, target.size.height + 2. * w),
        );
        let resized = !self.frame.size.same_as(frame.size);
        if !resized && self.frame.origin.same_as(frame.origin) {
            return Ok(());
        }
        if resized {
            with_disabled_actions(|| self.root.setBounds(bounds(frame.size)));
        }
        let transaction = WindowTransaction::new()?;
        if resized {
            transaction.set_rounded_frame(&self.window, frame, style.outer_radius())?;
            self.surface.set_bounds_in(&transaction, bounds(frame.size));
        } else {
            transaction.move_window(&self.window, frame.origin);
        }
        transaction.commit();
        if resized {
            self.surface.flush();
        }
        self.frame = frame;
        Ok(())
    }

    fn order_below(&self, wsid: WindowServerId) -> Result<(), CgsWindowError> {
        self.window.set_alpha(1.0)?;
        self.window.order_below(Some(wsid.as_u32()))
    }

    fn hide(&mut self) {
        if let Err(error) = self.window.order_out() {
            tracing::warn!(?error, "failed to hide a border");
        }
        // A reused border starts over: its old color must not fade into the new window's.
        self.color = None;
    }
}

fn bounds(size: CGSize) -> CGRect { CGRect::new(CGPoint::ZERO, size) }
