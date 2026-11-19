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
//! The borders run on their own thread, a [`BorderThread`]. On every display tick the animation
//! thread sends it the frames it just gave each moving window, and the borders follow. Creating a
//! border window is a WindowServer round trip, slow while apps are launching, so it must never
//! hold up the animation thread's next frame.
//!
//! A window that moves without wave, dragged by the user or moved by its app, is reported by
//! WindowServer through [`window_moved`], and its border follows from there. The reactor learns of
//! the move through Accessibility, which reads the frame back from the app and lags a frame or
//! more behind.

use std::collections::hash_map::Entry;
use std::time::Duration;
use std::{iter, thread};

use cgs::compositor::{
    CgsWindow, CgsWindowError, WindowSurface, WindowTransaction, with_disabled_actions,
};
use objc2::rc::{Retained, autoreleasepool};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGColor;
use objc2_quartz_core::{CALayer, CAMediaTimingFunction, CATransaction};
use parking_lot::Mutex;

use crate::actor::app::WindowId;
use crate::common::collections::HashMap;
use crate::common::config::{BorderSettings, parse_hex_color};
use crate::sys::geometry::SameAs;
use crate::sys::window_server::{self, WindowServerId};

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

/// Draws the focused window's outline from `settings` on `layer`, for a picture that stands in for
/// a window while it opens or closes. The window's picture covers the part that reaches in under
/// it. Returns how far the outline reaches outside the window and the window's corner radius, or
/// `None` with borders off.
pub(crate) fn draw_focused_outline(
    layer: &CALayer,
    settings: &BorderSettings,
) -> Option<(f64, f64)> {
    if !settings.enabled {
        return None;
    }
    let style = Style::new(settings)?;
    let [r, g, b, a] = style.active;
    let color: Retained<CGColor> = CGColor::new_generic_rgb(r, g, b, a).into();
    with_disabled_actions(|| {
        layer.setContentsScale(2.0);
        layer.setCornerRadius(style.outer_radius());
        layer.setBorderWidth(style.width + OVERLAP);
        layer.setBorderColor(Some(&color));
    });
    Some((style.width, style.radius))
}

/// What the animation thread sends the border thread.
enum Update {
    Scene(Option<BorderScene>),
    /// The frames just sent to every moving window. Other windows are at rest.
    Frames(Vec<(WindowId, CGRect)>),
    /// WindowServer moved or resized this window.
    Moved(WindowServerId),
}

/// The running border thread's inbox, for WindowServer's move reports.
static MOVES_INBOX: Mutex<Option<crossbeam_channel::Sender<Update>>> = Mutex::new(None);

/// WindowServer moved or resized `wsid`: a drag, an app moving its own window, or one of wave's
/// own moves. The border thread reads where the window is now and puts its border there, unless
/// wave is animating that window, in which case its border follows the animation.
pub fn window_moved(wsid: WindowServerId) {
    if let Some(tx) = MOVES_INBOX.lock().as_ref() {
        _ = tx.send(Update::Moved(wsid));
    }
}

/// The animation thread's handle to the border thread. Dropping it ends the thread and releases
/// every border window.
#[derive(Debug)]
pub struct BorderThread {
    tx: Option<crossbeam_channel::Sender<Update>>,
    sent_frames: Vec<(WindowId, CGRect)>,
}

impl BorderThread {
    pub fn spawn() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let spawned = thread::Builder::new().name("wave-borders".into()).spawn(move || run(&rx));
        let tx = match spawned {
            Ok(_) => {
                *MOVES_INBOX.lock() = Some(tx.clone());
                Some(tx)
            }
            Err(error) => {
                tracing::warn!(%error, "border thread failed to start; no borders");
                None
            }
        };
        Self { tx, sent_frames: Vec::new() }
    }

    pub fn set_scene(&self, scene: Option<BorderScene>) { self.send(Update::Scene(scene)); }

    /// Sends this tick's frames for moving windows, unless they're the same as last tick's.
    pub fn follow(&mut self, frames: Vec<(WindowId, CGRect)>) {
        if frames != self.sent_frames {
            self.sent_frames.clone_from(&frames);
            self.send(Update::Frames(frames));
        }
    }

    fn send(&self, update: Update) {
        if let Some(tx) = &self.tx {
            // The thread only stops when this handle drops.
            _ = tx.send(update);
        }
    }
}

impl Drop for BorderThread {
    fn drop(&mut self) {
        // The inbox's copy of the sender would keep the thread alive.
        let mut inbox = MOVES_INBOX.lock();
        if let (Some(ours), Some(registered)) = (&self.tx, inbox.as_ref())
            && ours.same_channel(registered)
        {
            *inbox = None;
        }
    }
}

/// Applies updates as fast as WindowServer allows. Updates that queued meanwhile are taken
/// together: every scene in order, then only the newest frames.
fn run(rx: &crossbeam_channel::Receiver<Update>) {
    let mut follower = BorderFollower::default();
    let mut latest = Latest::default();
    while let Ok(first) = rx.recv() {
        for update in iter::once(first).chain(rx.try_iter()) {
            match update {
                Update::Scene(scene) => {
                    latest.keep_only(|wid| {
                        scene
                            .as_ref()
                            .is_some_and(|scene| scene.windows.iter().any(|w| w.wid == wid))
                    });
                    follower.set_scene(scene);
                }
                Update::Frames(frames) => latest.animate(frames),
                Update::Moved(wsid) => {
                    if let Some(wid) =
                        follower.window_for(wsid).filter(|wid| latest.takes_report(*wid))
                        && let Some(info) = window_server::get_window(wsid)
                    {
                        latest.report(wid, info.frame);
                    }
                }
            }
        }
        follower.follow(|wid| latest.frame(wid));
    }
}

/// Frames newer than the scene's, which waits for Accessibility.
#[derive(Default)]
struct Latest {
    /// The frames wave just sent to the windows it's animating.
    moving: HashMap<WindowId, CGRect>,
    /// Where WindowServer last reported each window at rest. It stands until wave moves the window.
    reported: HashMap<WindowId, CGRect>,
}

impl Latest {
    fn animate(&mut self, frames: Vec<(WindowId, CGRect)>) {
        self.moving = frames.into_iter().collect();
        self.reported.retain(|wid, _| !self.moving.contains_key(wid));
    }

    /// False while wave animates `wid`: WindowServer's reports trail the frames wave sends and
    /// would pull the border back.
    fn takes_report(&self, wid: WindowId) -> bool { !self.moving.contains_key(&wid) }

    fn report(&mut self, wid: WindowId, frame: CGRect) {
        if self.takes_report(wid) {
            self.reported.insert(wid, frame);
        }
    }

    /// Forgets the reports for windows that left the scene.
    fn keep_only(&mut self, in_scene: impl Fn(WindowId) -> bool) {
        self.reported.retain(|wid, _| in_scene(*wid));
    }

    fn frame(&self, wid: WindowId) -> Option<CGRect> {
        self.moving.get(&wid).or_else(|| self.reported.get(&wid)).copied()
    }
}

/// Owns the borders and keeps each on its window's current frame.
#[derive(Default)]
struct BorderFollower {
    borders: HashMap<WindowId, Border>,
    spare: Vec<Border>,
    scene: Option<BorderScene>,
}

impl BorderFollower {
    /// Shows `scene`, or with `None` releases every border window.
    fn set_scene(&mut self, scene: Option<BorderScene>) {
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
                        // Placed before it's ordered in, so it never shows at its old spot.
                        let mut moves = Moves::default();
                        border.place(window.frame, style, &mut moves)?;
                        moves.commit();
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

    /// The bordered window WindowServer knows as `wsid`.
    fn window_for(&self, wsid: WindowServerId) -> Option<WindowId> {
        self.scene.as_ref()?.windows.iter().find(|w| w.wsid == wsid).map(|w| w.wid)
    }

    /// Moves each border to its window. `current` gives a window's frame when it's newer than the
    /// scene's: the frame a moving window was just sent, or where WindowServer saw it last.
    fn follow(&mut self, current: impl Fn(WindowId) -> Option<CGRect>) {
        let Some(scene) = &self.scene else { return };
        let Some(style) = Style::new(&scene.settings) else {
            return;
        };
        // One Core Animation commit and one WindowServer transaction for every border: per border,
        // the commits contend for Core Animation's lock and cost a frame with many windows.
        let result = autoreleasepool(|_| -> Result<(), CgsWindowError> {
            let mut moves = Moves::default();
            with_disabled_actions(|| -> Result<(), CgsWindowError> {
                for window in &scene.windows {
                    if let Some(border) = self.borders.get_mut(&window.wid) {
                        let target = current(window.wid).unwrap_or(window.frame);
                        border.place(target, style, &mut moves)?;
                    }
                }
                Ok(())
            })?;
            moves.commit();
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

    /// Puts the outline around a window whose frame is `target`, once `moves` commits.
    fn place(
        &mut self,
        target: CGRect,
        style: Style,
        moves: &mut Moves,
    ) -> Result<(), CgsWindowError> {
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
        let transaction = moves.transaction()?;
        if resized {
            transaction.set_rounded_frame(&self.window, frame, style.outer_radius())?;
            self.surface.set_bounds_in(transaction, bounds(frame.size));
            moves.resized = true;
        } else {
            transaction.move_window(&self.window, frame.origin);
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

/// Border moves gathered into one WindowServer transaction, opened on the first move.
#[derive(Default)]
struct Moves {
    transaction: Option<WindowTransaction>,
    /// A border changed size, so its layer's new bounds must be flushed too.
    resized: bool,
}

impl Moves {
    fn transaction(&mut self) -> Result<&WindowTransaction, CgsWindowError> {
        match &mut self.transaction {
            Some(transaction) => Ok(transaction),
            slot @ None => Ok(slot.insert(WindowTransaction::new()?)),
        }
    }

    fn commit(self) {
        if let Some(transaction) = self.transaction {
            transaction.commit();
            if self.resized {
                CATransaction::flush();
            }
        }
    }
}

fn bounds(size: CGSize) -> CGRect { CGRect::new(CGPoint::ZERO, size) }

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64) -> CGRect { CGRect::new(CGPoint::new(x, 0.0), CGSize::new(100.0, 100.0)) }

    #[test]
    fn a_window_moved_without_wave_takes_its_border_until_wave_moves_it() {
        let wid = WindowId::new(1, 1);
        let mut latest = Latest::default();
        assert_eq!(latest.frame(wid), None);

        latest.report(wid, rect(10.0));
        assert_eq!(latest.frame(wid), Some(rect(10.0)));

        // Mid-animation reports trail the frames wave sends, so they're ignored.
        latest.animate(vec![(wid, rect(50.0))]);
        latest.report(wid, rect(40.0));
        assert_eq!(latest.frame(wid), Some(rect(50.0)));

        // At rest again, the scene holds until WindowServer reports a move.
        latest.animate(Vec::new());
        assert_eq!(latest.frame(wid), None);
        latest.report(wid, rect(60.0));
        assert_eq!(latest.frame(wid), Some(rect(60.0)));

        latest.keep_only(|_| false);
        assert_eq!(latest.frame(wid), None);
    }
}
