//! Opening and closing animations for tiled windows. A new window flies from
//! where macOS opened it into its tile, in step with its neighbors' slide; a
//! closing window shrinks and fades away while its neighbors close the gap.
//!
//! Closing (`Closer`): once a window is gone there is nothing left to capture,
//! so a background thread keeps one low-resolution picture of the focused tiled
//! window and shrinks that picture away when the window closes.
//!
//! Opening:
//! macOS gives rift no way to move, fade or scale another app's window
//! smoothly, so the animation runs on a picture of it. The window is captured
//! where it opened, and a rift-owned overlay covers it with that picture, so
//! nothing visibly changes. The real window then moves off screen unresized
//! (its picture stays true), the picture flies into the tile on the GPU (Core
//! Animation) while the neighbors slide, the real window lands in the tile
//! underneath and redraws, and the picture cross-fades away.
//!
//! Every failure (no Screen Recording permission, a window that never takes
//! its tile size, a capture or overlay error) falls back to placing the window
//! directly, so a pop-in can make a window late but never lost.

use std::num::NonZeroU32;
use std::ptr::NonNull;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use cgs::compositor::{
    CgsWindow, CgsWindowError, G_CONNECTION, WindowSurface, with_disabled_actions,
};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFArray, CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGImage, CGPreflightScreenCaptureAccess};
use objc2_quartz_core::{CALayer, CAMediaTimingFunction, CATransaction};
use parking_lot::Mutex;
use tracing::{debug, warn};

use super::animation::Animation;
use super::{Event, Reactor, Sender, TransactionId};
use crate::actor::app::{AppThreadHandle, Request, WindowId};
use crate::common::collections::HashMap;
use crate::common::config::AnimationEasing;
use crate::model::hidden_window_placement::{HiddenWindowPlacement, HideCorner};
use crate::sys::geometry::SameAs;
use crate::sys::screen::SpaceId;
use crate::sys::skylight::SLSHWCaptureWindowList;
use crate::sys::window_server::{self, WindowServerId};

/// A window laid out this long after it was created no longer counts as "opening".
const CANDIDATE_TTL: Duration = Duration::from_secs(2);
/// How long to wait for the app to apply a frame before giving up on the pop-in.
const FRAME_TIMEOUT: Duration = Duration::from_millis(250);
const FRAME_POLL: Duration = Duration::from_millis(8);
/// Time for a just-opened window to draw its first frame before it is captured.
const FIRST_DRAW: Duration = Duration::from_millis(16);
/// Time for the app to redraw at its tile size under the picture. Finder needs about 30 ms.
const TILE_REDRAW: Duration = Duration::from_millis(40);
/// The picture's cross-fade into the real window.
const FADE_OUT: Duration = Duration::from_millis(80);
/// Capture at full backing resolution, ignoring the window's clip shape: the
/// opening picture stands in for the real window, so it must be sharp.
const CAPTURE_OPTIONS: u32 = (1 << 11) | (1 << 8);
/// The closing picture is kept around, so capture at nominal (1x) resolution:
/// about 10 MB for a big tile instead of 38 MB, and a 0.2 s fade hides the softness.
const CLOSE_CAPTURE_OPTIONS: u32 = (1 << 11) | (1 << 9);
/// Refresh the closing picture once focus has rested this long, so sweeping the
/// mouse across windows (focus follows mouse) costs one capture, not one per window.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(150);
/// A closing window shrinks to this fraction of its tile as it fades.
const CLOSE_SCALE: f64 = 0.85;

/// Reactor-side bookkeeping: which windows just opened, and which are popping in.
#[derive(Default, Debug)]
pub(super) struct PopIns {
    /// Keyed by WindowServer id, which is also `WindowId::idx`, so both the AX
    /// creation event and WindowServer's "appeared" event can mark a window.
    candidates: HashMap<NonZeroU32, Instant>,
    active: HashMap<WindowId, Popping>,
    /// The neighbors' slide, waiting for this window's picture.
    held: Option<(WindowId, Animation)>,
}

#[derive(Debug)]
struct Popping {
    target: CGRect,
    cancelled: Arc<Mutex<bool>>,
}

impl PopIns {
    pub fn note_created(&mut self, wid: WindowId) {
        self.candidates.insert(wid.idx, Instant::now());
    }

    pub fn note_appeared(&mut self, wsid: WindowServerId) {
        if let Some(idx) = wsid.as_nonzero() {
            self.candidates.insert(idx, Instant::now());
        }
    }

    /// True once for a window whose first layout follows its creation.
    pub fn take_candidate(&mut self, wid: WindowId) -> bool {
        let now = Instant::now();
        self.candidates
            .retain(|_, created| now.duration_since(*created) < CANDIDATE_TTL);
        self.candidates.remove(&wid.idx).is_some()
    }

    /// True while a pop-in owns `wid`'s frame; layout passes leave it alone.
    pub fn is_active(&self, wid: WindowId) -> bool {
        self.active.get(&wid).is_some_and(|p| !*p.cancelled.lock())
    }

    /// True if `wid` is popping into exactly `target`.
    pub fn is_heading_to(&self, wid: WindowId, target: CGRect) -> bool {
        self.active.get(&wid).is_some_and(|p| p.target.same_as(target))
    }

    pub fn begin(&mut self, wid: WindowId, target: CGRect) -> Arc<Mutex<bool>> {
        let cancelled = Arc::new(Mutex::new(false));
        self.active.insert(wid, Popping {
            target,
            cancelled: cancelled.clone(),
        });
        cancelled
    }

    /// Hands `wid` back to the caller, which must place it: the picture goes
    /// away within one poll and the pop-in no longer moves the window.
    pub fn cancel(&mut self, wid: WindowId) {
        if let Some(popping) = self.active.get(&wid) {
            *popping.cancelled.lock() = true;
        }
    }

    pub fn hold(&mut self, wid: WindowId, anim: Animation) { self.held = Some((wid, anim)); }

    pub fn take_held(&mut self) -> Option<Animation> { self.held.take().map(|(_, anim)| anim) }

    pub fn take_held_for(&mut self, wid: WindowId) -> Option<Animation> {
        if self.held.as_ref().is_some_and(|(w, _)| *w == wid) {
            self.take_held()
        } else {
            None
        }
    }

    /// True if `wid` was popping in, so the layout should be applied again.
    pub fn finish(&mut self, wid: WindowId) -> bool { self.active.remove(&wid).is_some() }
}

/// Starts a pop-in for `wid` if it just opened and can pop in; otherwise the
/// caller animates it as usual. `park_txid` is the transaction the caller
/// already registered for `target`.
pub(super) fn try_start(
    reactor: &mut Reactor,
    space: SpaceId,
    wid: WindowId,
    park_txid: TransactionId,
    target: CGRect,
) -> bool {
    if reactor.animation_tx.is_none()
        || !reactor.pop_ins.take_candidate(wid)
        || reactor.layout_manager.layout_engine.is_window_floating(wid)
    {
        return false;
    }
    let Some(wsid) = reactor.state.windows.window(wid).and_then(|w| w.info.sys_id) else {
        return false;
    };
    let Some(handle) = reactor.app_manager.apps.get(&wid.pid).map(|app| app.handle.clone()) else {
        return false;
    };
    let Some(screen) = reactor.space_state.screen_by_space(space) else {
        return false;
    };
    let screens: Vec<_> = reactor.space_state.screens.iter().map(|s| s.frame).collect();
    // A newer transaction makes the reactor ignore the parking write's acknowledgement.
    let final_txid = reactor.transaction_manager.generate_next_txid(wsid);
    reactor.transaction_manager.update_txid_entries([(wsid, final_txid, target)]);
    let cancelled = reactor.pop_ins.begin(wid, target);
    debug!(?wid, ?target, "pop-in started");
    Job {
        handle,
        wid,
        wsid,
        screen: screen.frame,
        screens,
        target,
        park_txid,
        final_txid,
        duration: Duration::from_secs_f64(reactor.config.settings.animation_duration),
        easing: reactor.config.settings.animation_easing,
        events: reactor.communication_manager.events_tx.clone(),
        cancelled,
    }
    .spawn();
    true
}

/// One window's pop-in, run on its own short-lived thread.
#[derive(Clone)]
pub(super) struct Job {
    pub handle: AppThreadHandle,
    pub wid: WindowId,
    pub wsid: WindowServerId,
    /// The target's screen, and every screen, for finding an off-screen parking spot.
    pub screen: CGRect,
    pub screens: Vec<CGRect>,
    pub target: CGRect,
    /// The parking write is superseded by `final_txid`, so the reactor ignores its acknowledgement.
    pub park_txid: TransactionId,
    pub final_txid: TransactionId,
    pub duration: Duration,
    pub easing: AnimationEasing,
    pub events: Option<Sender>,
    /// Set by the reactor when a layout takes the window back (workspace switch, display change).
    pub cancelled: Arc<Mutex<bool>>,
}

impl Job {
    // ponytail: one thread per opening window; fine at human window-opening rates.
    pub fn spawn(self) {
        let fallback = self.clone();
        let spawned = thread::Builder::new().name("rift-pop-in".into()).spawn(move || self.run());
        if let Err(error) = spawned {
            warn!(%error, "pop-in thread failed to start; placing window directly");
            fallback.place();
            fallback.finish();
        }
    }

    fn run(self) {
        let started = Instant::now();
        let presented = self.present(started);
        if !presented {
            self.ready();
            self.place_unless_cancelled();
        }
        debug!(wid = ?self.wid, presented, elapsed = ?started.elapsed(), "pop-in finished");
        self.finish();
    }

    /// Runs the animation. Returns false if the window still needs placing.
    fn present(&self, started: Instant) -> bool {
        // Step timings, for lining the animation up against a screen recording.
        let step = |name: &str| {
            debug!(wid = ?self.wid, step = name, t_ms = started.elapsed().as_secs_f64() * 1e3, "pop-in step");
        };
        if !CGPreflightScreenCaptureAccess() {
            debug!("pop-in skipped: Screen Recording permission not granted");
            return false;
        }
        // A window can be laid out before macOS shows it; wait for where it opened.
        let Some(spot) = wait_for_frame(self.wsid, |f| f.size.width > 0.0 && f.size.height > 0.0)
        else {
            debug!(wid = ?self.wid, "pop-in skipped: window never appeared");
            return false;
        };
        thread::sleep(FIRST_DRAW);
        let Some(image) = capture(self.wsid, CAPTURE_OPTIONS) else {
            debug!(wid = ?self.wid, "pop-in skipped: capture failed");
            return false;
        };
        step("captured");
        if self.is_cancelled() {
            return true;
        }
        // The picture covers the window exactly, so swapping one for the other is invisible.
        let overlay = match Overlay::new(spot, self.target, &image, Some(self.wsid)) {
            Ok(overlay) => overlay,
            Err(error) => {
                debug!(wid = ?self.wid, ?error, "pop-in skipped: overlay failed");
                return false;
            }
        };
        step("covered");
        // Parking keeps the opening size, so the picture stays true to the window.
        let parked = HiddenWindowPlacement::calculate(
            self.screen,
            spot,
            HideCorner::BottomRight,
            &self.screens,
        );
        self.write(parked, self.park_txid);
        // The picture may only leave once the real window is gone from under it.
        let gone = |frame: CGRect| !frame.origin.same_as(spot.origin);
        if wait_for_frame(self.wsid, gone).is_none() {
            debug!(wid = ?self.wid, "pop-in skipped: window did not move off screen");
            return false;
        }
        step("parked");
        self.ready();
        overlay.animate(self.target, 1.0, self.duration, self.easing);
        if !self.sleep_unless_interrupted(self.duration) || !self.place_unless_cancelled() {
            // Closed or taken back mid-flight: drop the picture now, leave the window be.
            debug!(wid = ?self.wid, "pop-in cut short");
            return true;
        }
        step("placed");
        // Keep covering until the real window has landed and redrawn underneath.
        wait_for_frame(self.wsid, |frame| frame.origin.same_as(self.target.origin));
        step("landed");
        if self.sleep_unless_interrupted(TILE_REDRAW) {
            overlay.fade_out(FADE_OUT);
            self.sleep_unless_interrupted(FADE_OUT);
        }
        drop(overlay);
        step("overlay removed");
        true
    }

    fn place(&self) { self.write(self.target, self.final_txid); }

    fn is_cancelled(&self) -> bool { *self.cancelled.lock() }

    /// Holding the lock while sending means a cancelling layout's own write always
    /// queues after ours on the app thread, so the reactor's frame wins.
    fn place_unless_cancelled(&self) -> bool {
        let cancelled = self.cancelled.lock();
        if !*cancelled {
            self.place();
        }
        !*cancelled
    }

    /// Sleeps for `duration`. Returns false early if the window closes or the pop-in is cancelled.
    fn sleep_unless_interrupted(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        loop {
            if self.is_cancelled() || window_server::get_window(self.wsid).is_none() {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            thread::sleep(remaining.min(FRAME_POLL));
        }
    }

    fn write(&self, frame: CGRect, txid: TransactionId) {
        if let Err(error) = self.handle.send(Request::set_window_frame(self.wid, frame, txid, true))
        {
            debug!(wid = ?self.wid, ?error, "pop-in frame write failed; app may have quit");
        }
    }

    fn ready(&self) { self.send(Event::PopInReady(self.wid)); }

    fn finish(&self) { self.send(Event::PopInFinished(self.wid)); }

    fn send(&self, event: Event) {
        if let Some(events) = &self.events {
            events.send(event);
        }
    }
}

/// Polls WindowServer (cheap, no app involvement) until `done` holds and the size
/// matched the previous poll, so a resize done in several steps has finished.
/// Returns the settled frame, or `None` at the timeout.
fn wait_for_frame(wsid: WindowServerId, done: impl Fn(CGRect) -> bool) -> Option<CGRect> {
    let deadline = Instant::now() + FRAME_TIMEOUT;
    let mut previous: Option<CGRect> = None;
    loop {
        let frame = window_server::get_window(wsid).map(|info| info.frame);
        if let Some(frame) = frame
            && done(frame)
            && previous.is_some_and(|p| p.size.same_as(frame.size))
        {
            return Some(frame);
        }
        if Instant::now() >= deadline {
            return None;
        }
        previous = frame;
        thread::sleep(FRAME_POLL);
    }
}

/// Core Animation only takes cubic Béziers, so each curve uses its standard
/// Bézier fit (easings.net). `bezier_fits_match_the_layout_curves` checks them
/// against the curve the neighbors use.
fn bezier(easing: AnimationEasing) -> [f32; 4] {
    use AnimationEasing::*;
    match easing {
        Linear => [0.0, 0.0, 1.0, 1.0],
        EaseInSine => [0.12, 0.0, 0.39, 0.0],
        EaseOutSine => [0.61, 1.0, 0.88, 1.0],
        EaseInOutSine => [0.37, 0.0, 0.63, 1.0],
        EaseInQuad => [0.11, 0.0, 0.5, 0.0],
        EaseOutQuad => [0.5, 1.0, 0.89, 1.0],
        EaseInOutQuad => [0.45, 0.0, 0.55, 1.0],
        EaseInCubic => [0.32, 0.0, 0.67, 0.0],
        EaseOutCubic => [0.33, 1.0, 0.68, 1.0],
        EaseInOutCubic => [0.65, 0.0, 0.35, 1.0],
        EaseInQuart => [0.5, 0.0, 0.75, 0.0],
        EaseOutQuart => [0.25, 1.0, 0.5, 1.0],
        EaseInOutQuart => [0.76, 0.0, 0.24, 1.0],
        EaseInQuint => [0.64, 0.0, 0.78, 0.0],
        EaseOutQuint => [0.22, 1.0, 0.36, 1.0],
        EaseInOutQuint => [0.83, 0.0, 0.17, 1.0],
        EaseInExpo => [0.7, 0.0, 0.84, 0.0],
        EaseOutExpo => [0.16, 1.0, 0.3, 1.0],
        EaseInOutExpo => [0.87, 0.0, 0.13, 1.0],
        EaseInCirc => [0.55, 0.0, 1.0, 0.45],
        EaseOutCirc => [0.0, 0.55, 0.45, 1.0],
        EaseInOut | EaseInOutCirc => [0.85, 0.0, 0.15, 1.0],
    }
}

fn capture(wsid: WindowServerId, options: u32) -> Option<CFRetained<CGImage>> {
    let id = wsid.as_u32();
    let images = unsafe { SLSHWCaptureWindowList(*G_CONNECTION, &id, 1, options) };
    let images: CFRetained<CFArray<CGImage>> =
        unsafe { CFRetained::from_raw(NonNull::new(images)?) };
    images.iter().next()
}

/// A rift-owned window over the tile that shows the captured picture.
struct Overlay {
    // The surface must be detached before its owning WindowServer window drops.
    surface: WindowSurface,
    window: CgsWindow,
    /// The overlay window's screen frame.
    frame: CGRect,
    picture: Retained<CALayer>,
}

impl Overlay {
    /// One overlay window spans the whole flight; the picture starts exactly over
    /// `spot`. With no window to sit above, it goes on top of the normal windows.
    fn new(
        spot: CGRect,
        target: CGRect,
        image: &CGImage,
        above: Option<WindowServerId>,
    ) -> Result<Self, CgsWindowError> {
        let frame = union(spot, target);
        let window = CgsWindow::new_compositor(frame, 0.0)?;
        window.disable_shadow();
        window.set_alpha(1.0)?;
        window.set_tags(1 << 3)?;

        let bounds = CGRect::new(CGPoint::ZERO, frame.size);
        let root = CALayer::layer();
        let picture = CALayer::layer();
        with_disabled_actions(|| {
            root.setBounds(bounds);
            root.setPosition(CGPoint::ZERO);
            root.setAnchorPoint(CGPoint::ZERO);
            root.setOpaque(false);
            // Top-left origin, like the screen frames rift works in.
            root.setGeometryFlipped(true);
            picture.setAnchorPoint(CGPoint::ZERO);
            picture.setFrame(local(spot, frame));
            picture.setContentsScale(CGImage::width(Some(image)) as f64 / spot.size.width);
            unsafe { picture.setContents(Some(&*(image as *const CGImage as *const AnyObject))) };
            root.addSublayer(&picture);
        });
        let surface = WindowSurface::new(window.id(), bounds, &root)?;
        surface.flush();
        window.order_above(above.map(|id| id.as_u32()))?;
        Ok(Self {
            surface,
            window,
            frame,
            picture,
        })
    }

    /// Hands the motion to Core Animation; the render server runs it on the GPU.
    fn animate(&self, to: CGRect, opacity: f32, duration: Duration, easing: AnimationEasing) {
        let [x1, y1, x2, y2] = bezier(easing);
        CATransaction::begin();
        CATransaction::setAnimationDuration(duration.as_secs_f64());
        CATransaction::setAnimationTimingFunction(Some(
            &CAMediaTimingFunction::functionWithControlPoints(x1, y1, x2, y2),
        ));
        self.picture.setFrame(local(to, self.frame));
        self.picture.setOpacity(opacity);
        CATransaction::commit();
        self.surface.flush();
    }

    fn fade_out(&self, duration: Duration) {
        CATransaction::begin();
        CATransaction::setAnimationDuration(duration.as_secs_f64());
        self.picture.setOpacity(0.0);
        CATransaction::commit();
        self.surface.flush();
    }
}

enum CloseMsg {
    Watch(WindowId),
    Closed {
        wid: WindowId,
        frame: CGRect,
        duration: Duration,
        easing: AnimationEasing,
    },
}

/// Keeps one picture of the focused tiled window, so it can shrink away when it closes.
#[derive(Debug)]
pub(super) struct Closer {
    tx: crossbeam_channel::Sender<CloseMsg>,
    /// The window and frame the current picture is for, to skip redundant refreshes.
    watched: Option<(WindowId, CGRect)>,
}

impl Closer {
    pub fn spawn() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let spawned =
            thread::Builder::new().name("rift-close".into()).spawn(move || run_closer(rx));
        if let Err(error) = spawned {
            warn!(%error, "close-animation thread failed to start; closes will not animate");
        }
        Self { tx, watched: None }
    }

    /// Refreshes the picture when the focused window or its frame changes.
    pub fn watch(&mut self, wid: WindowId, frame: CGRect) {
        if self.watched.is_some_and(|(w, f)| w == wid && f.same_as(frame)) {
            return;
        }
        self.watched = Some((wid, frame));
        _ = self.tx.send(CloseMsg::Watch(wid));
    }

    /// Shrinks `wid`'s picture away from `frame`, if the picture is of `wid`.
    pub fn closed(
        &mut self,
        wid: WindowId,
        frame: CGRect,
        duration: Duration,
        easing: AnimationEasing,
    ) {
        if self.watched.is_some_and(|(w, _)| w == wid) {
            self.watched = None;
        }
        _ = self.tx.send(CloseMsg::Closed { wid, frame, duration, easing });
    }
}

fn run_closer(rx: crossbeam_channel::Receiver<CloseMsg>) {
    use crossbeam_channel::RecvTimeoutError;
    let mut picture: Option<(WindowId, CFRetained<CGImage>)> = None;
    let mut pending: Option<WindowId> = None;
    loop {
        let msg = match pending {
            Some(wid) => match rx.recv_timeout(WATCH_DEBOUNCE) {
                Ok(msg) => msg,
                Err(RecvTimeoutError::Timeout) => {
                    pending = None;
                    let wsid = WindowServerId::new(wid.idx.get());
                    picture = CGPreflightScreenCaptureAccess()
                        .then(|| capture(wsid, CLOSE_CAPTURE_OPTIONS))
                        .flatten()
                        .map(|image| (wid, image));
                    debug!(?wid, captured = picture.is_some(), "close picture refreshed");
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(msg) => msg,
                Err(_) => return,
            },
        };
        match msg {
            CloseMsg::Watch(wid) => pending = Some(wid),
            CloseMsg::Closed { wid, frame, duration, easing } => {
                if pending == Some(wid) {
                    pending = None;
                }
                match picture.take_if(|(w, _)| *w == wid) {
                    Some((_, image)) => shrink_away(frame, &image, duration, easing),
                    None => debug!(?wid, "close not animated: no picture of this window"),
                }
            }
        }
    }
}

fn shrink_away(frame: CGRect, image: &CGImage, duration: Duration, easing: AnimationEasing) {
    let overlay = match Overlay::new(frame, frame, image, None) {
        Ok(overlay) => overlay,
        Err(error) => {
            debug!(?error, "close animation skipped: overlay failed");
            return;
        }
    };
    overlay.animate(scaled_about_center(frame, CLOSE_SCALE), 0.0, duration, easing);
    thread::sleep(duration);
}

fn scaled_about_center(rect: CGRect, scale: f64) -> CGRect {
    let size = CGSize::new(rect.size.width * scale, rect.size.height * scale);
    let origin = CGPoint::new(
        rect.origin.x + (rect.size.width - size.width) / 2.0,
        rect.origin.y + (rect.size.height - size.height) / 2.0,
    );
    CGRect::new(origin, size)
}

fn union(a: CGRect, b: CGRect) -> CGRect {
    let (x0, y0) = (a.origin.x.min(b.origin.x), a.origin.y.min(b.origin.y));
    let x1 = (a.origin.x + a.size.width).max(b.origin.x + b.size.width);
    let y1 = (a.origin.y + a.size.height).max(b.origin.y + b.size.height);
    CGRect::new(CGPoint::new(x0, y0), CGSize::new(x1 - x0, y1 - y0))
}

/// `rect` relative to the overlay window at `frame`.
fn local(rect: CGRect, frame: CGRect) -> CGRect {
    CGRect::new(
        CGPoint::new(rect.origin.x - frame.origin.x, rect.origin.y - frame.origin.y),
        rect.size,
    )
}

impl Drop for Overlay {
    fn drop(&mut self) { let _ = self.window.order_out(); }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use objc2_core_foundation::CGSize;

    use super::*;

    fn wid(idx: u32) -> WindowId {
        WindowId {
            pid: 1,
            idx: NonZeroU32::new(idx).unwrap(),
        }
    }

    fn rect(x: f64) -> CGRect { CGRect::new(CGPoint::new(x, 0.0), CGSize::new(100.0, 100.0)) }

    #[test]
    fn a_created_window_is_a_candidate_exactly_once() {
        let mut pop_ins = PopIns::default();
        pop_ins.note_created(wid(1));
        assert!(!pop_ins.take_candidate(wid(2)));
        assert!(pop_ins.take_candidate(wid(1)));
        assert!(!pop_ins.take_candidate(wid(1)));
    }

    #[test]
    fn a_window_server_appearance_marks_the_same_window() {
        let mut pop_ins = PopIns::default();
        pop_ins.note_appeared(WindowServerId::new(7));
        assert!(pop_ins.take_candidate(WindowId {
            pid: 42,
            idx: NonZeroU32::new(7).unwrap()
        }));
        assert!(!pop_ins.take_candidate(wid(7)));
    }

    #[test]
    fn stale_candidates_expire() {
        let mut pop_ins = PopIns::default();
        pop_ins.candidates.insert(wid(1).idx, Instant::now() - CANDIDATE_TTL);
        assert!(!pop_ins.take_candidate(wid(1)));
    }

    /// y at x for a CSS-style cubic Bézier, by bisection on x.
    fn bezier_at([x1, y1, x2, y2]: [f32; 4], x: f64) -> f64 {
        let at = |a: f64, b: f64, s: f64| {
            3.0 * a * s * (1.0 - s).powi(2) + 3.0 * b * s * s * (1.0 - s) + s.powi(3)
        };
        let (mut lo, mut hi) = (0.0, 1.0);
        for _ in 0..60 {
            let mid = (lo + hi) / 2.0;
            if at(x1 as f64, x2 as f64, mid) < x {
                lo = mid
            } else {
                hi = mid
            }
        }
        at(y1 as f64, y2 as f64, lo)
    }

    #[test]
    fn bezier_fits_match_the_layout_curves() {
        use AnimationEasing::*;
        for easing in [
            EaseInOut,
            Linear,
            EaseInSine,
            EaseOutSine,
            EaseInOutSine,
            EaseInQuad,
            EaseOutQuad,
            EaseInOutQuad,
            EaseInCubic,
            EaseOutCubic,
            EaseInOutCubic,
            EaseInQuart,
            EaseOutQuart,
            EaseInOutQuart,
            EaseInQuint,
            EaseOutQuint,
            EaseInOutQuint,
            EaseInExpo,
            EaseOutExpo,
            EaseInOutExpo,
            EaseInCirc,
            EaseOutCirc,
            EaseInOutCirc,
        ] {
            for i in 1..20 {
                let t = i as f64 / 20.0;
                let (fit, curve) = (
                    bezier_at(bezier(easing), t),
                    super::super::animation::ease(easing, t),
                );
                assert!(
                    (fit - curve).abs() < 0.08,
                    "{easing:?} at {t}: fit {fit:.3}, curve {curve:.3}"
                );
            }
        }
    }

    #[test]
    fn the_overlay_spans_the_flight_and_starts_over_the_window() {
        let spot = CGRect::new(CGPoint::new(400.0, 300.0), CGSize::new(900.0, 500.0));
        let tile = CGRect::new(CGPoint::new(2300.0, 800.0), CGSize::new(1500.0, 250.0));
        let frame = union(spot, tile);
        assert_eq!(
            frame,
            CGRect::new(CGPoint::new(400.0, 300.0), CGSize::new(3400.0, 750.0))
        );
        assert_eq!(local(spot, frame).origin, CGPoint::ZERO);
        assert_eq!(
            local(tile, frame),
            CGRect::new(CGPoint::new(1900.0, 500.0), tile.size)
        );
    }

    #[test]
    fn a_closing_window_shrinks_about_its_center() {
        let tile = CGRect::new(CGPoint::new(100.0, 200.0), CGSize::new(1000.0, 400.0));
        let small = scaled_about_center(tile, 0.85);
        assert_eq!(small.size, CGSize::new(850.0, 340.0));
        assert_eq!(small.origin, CGPoint::new(175.0, 230.0));
    }

    #[test]
    fn a_held_slide_is_released_only_for_its_own_window() {
        let mut pop_ins = PopIns::default();
        pop_ins.hold(wid(1), Animation::new(100.0, 0.2, AnimationEasing::default()));
        assert!(pop_ins.take_held_for(wid(2)).is_none());
        assert!(pop_ins.take_held_for(wid(1)).is_some());
        assert!(pop_ins.take_held().is_none());
    }

    #[test]
    fn a_cancelled_pop_in_hands_the_window_back_until_it_finishes() {
        let mut pop_ins = PopIns::default();
        let cancelled = pop_ins.begin(wid(1), rect(0.0));
        assert!(pop_ins.is_active(wid(1)));
        assert!(pop_ins.is_heading_to(wid(1), rect(0.0)));
        assert!(!pop_ins.is_heading_to(wid(1), rect(50.0)));
        pop_ins.cancel(wid(1));
        pop_ins.cancel(wid(2));
        assert!(*cancelled.lock(), "the job must see the cancel");
        assert!(
            !pop_ins.is_active(wid(1)),
            "layouts place a cancelled window themselves"
        );
        assert!(pop_ins.finish(wid(1)), "finishing still re-applies the layout");
        assert!(!pop_ins.finish(wid(1)));
        assert!(!pop_ins.is_active(wid(2)));
    }
}
