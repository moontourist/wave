//! Opening and closing animations for tiled windows, like Hyprland's "popin": a
//! new window grows into its tile from a little smaller as it fades in, while its
//! neighbors slide aside; a closing window shrinks and fades away while its
//! neighbors close the gap. The picture carries the window's border, so the
//! border grows and shrinks with it.
//!
//! Closing (`Closer`): once a window is gone there is nothing left to capture,
//! so a background thread keeps one low-resolution picture of the focused tiled
//! window and shrinks that picture away when the window closes.
//!
//! Opening:
//! macOS gives wave no way to fade or scale another app's window, so the
//! animation runs on a picture of it, taken where macOS opened the window (a
//! window off screen captures only the part still on a display). The window
//! then moves off screen and takes its tile's size there, out of sight. Its
//! picture grows into the tile on the GPU (Core Animation), pinned to the
//! tile's top-left corner with the window's background color filling the rest,
//! timed to settle together with the neighbors' slide. The real window lands in
//! the tile under the picture, its real border appears, and the picture fades
//! away.
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
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFArray, CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGDataProvider, CGImage, CGPreflightScreenCaptureAccess};
use objc2_quartz_core::{
    CALayer, CAMediaTimingFunction, CATransaction, CATransform3D, kCAGravityTopLeft,
};
use parking_lot::Mutex;
use tracing::{debug, warn};

use super::{Event, Reactor, Sender, TransactionId};
use crate::actor::app::{AppThreadHandle, Request, WindowId};
use crate::common::collections::HashMap;
use crate::common::config::{AnimationEasing, BorderSettings};
use crate::model::hidden_window_placement::{HiddenWindowPlacement, HideCorner};
use crate::sys::geometry::SameAs;
use crate::sys::screen::SpaceId;
use crate::sys::skylight::SLSHWCaptureWindowList;
use crate::sys::window_server::{self, WindowServerId};
use crate::ui::border;

/// A window laid out this long after it was created no longer counts as "opening".
const CANDIDATE_TTL: Duration = Duration::from_secs(2);
/// How long to wait for the app to apply a frame before giving up on the pop-in.
const FRAME_TIMEOUT: Duration = Duration::from_millis(250);
const FRAME_POLL: Duration = Duration::from_millis(8);
/// How long a new window gets to draw where it opened before the pop-in gives up on it.
const FIRST_DRAW: Duration = Duration::from_millis(150);
/// A new window's picture starts at this fraction of its tile, and a closing one shrinks to it.
const POP_SCALE: f64 = 0.85;
/// The shortest a pop-in runs, as a share of the animation duration. It lasts until the
/// neighbors' slide ends, but getting the picture ready can eat into that.
const MIN_POP_SHARE: f64 = 0.6;
/// The share of a pop-in its fade-in takes; the growing takes all of it.
const FADE_IN_SHARE: f64 = 0.4;
/// How long the picture's outline stays over the window's real border while that is drawn.
const BORDER_HANDOFF: Duration = Duration::from_millis(100);
/// How long the picture waits for the window to land in its tile before leaving anyway.
const LAND_TIMEOUT: Duration = Duration::from_secs(1);
/// The picture's cross-fade into the real window.
const FADE_OUT: Duration = Duration::from_millis(80);
/// Capture at full backing resolution, ignoring the window's clip shape: the
/// opening picture stands in for the real window, so it must be sharp.
const CAPTURE_OPTIONS: u32 = (1 << 11) | (1 << 8);
/// The closing picture is kept around, so capture at nominal (1x) resolution:
/// about 10 MB for a big tile instead of 38 MB, and a 0.2 s fade hides the softness.
const CLOSE_CAPTURE_OPTIONS: u32 = (1 << 11) | (1 << 9);
/// `kCGBitmapByteOrder32Little | kCGImageAlphaPremultipliedFirst`: premultiplied BGRA bytes, the
/// layout window captures come in.
const BGRA: u32 = 0x2002;
/// Refresh the closing picture once focus has rested this long, so sweeping the
/// mouse across windows (focus follows mouse) costs one capture, not one per window.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(150);
/// A closed window's picture holds its spot at most this long, waiting for the neighbors' slide.
const COVER_TIMEOUT: Duration = Duration::from_millis(300);

/// Reactor-side bookkeeping: which windows just opened, and which are popping in.
#[derive(Default, Debug)]
pub(super) struct PopIns {
    /// Keyed by WindowServer id, which is also `WindowId::idx`, so both the AX
    /// creation event and WindowServer's "appeared" event can mark a window.
    candidates: HashMap<NonZeroU32, Instant>,
    active: HashMap<WindowId, Popping>,
}

#[derive(Debug)]
struct Popping {
    target: CGRect,
    cancelled: Arc<Mutex<bool>>,
    /// The window sits in its tile under the picture, so its real border can be drawn.
    landed: bool,
}

impl PopIns {
    pub(super) fn note_created(&mut self, wid: WindowId) {
        self.candidates.insert(wid.idx, Instant::now());
    }

    pub(super) fn note_appeared(&mut self, wsid: WindowServerId) {
        if let Some(idx) = wsid.as_nonzero() {
            self.candidates.insert(idx, Instant::now());
        }
    }

    /// True once for a window whose first layout follows its creation.
    pub(super) fn take_candidate(&mut self, wid: WindowId) -> bool {
        let now = Instant::now();
        self.candidates
            .retain(|_, created| now.duration_since(*created) < CANDIDATE_TTL);
        self.candidates.remove(&wid.idx).is_some()
    }

    /// True while a pop-in owns `wid`'s frame; layout passes leave it alone.
    pub(super) fn is_active(&self, wid: WindowId) -> bool {
        self.active.get(&wid).is_some_and(|p| !*p.cancelled.lock())
    }

    /// True if `wid` is popping into exactly `target`.
    pub(super) fn is_heading_to(&self, wid: WindowId, target: CGRect) -> bool {
        self.active.get(&wid).is_some_and(|p| p.target.same_as(target))
    }

    pub(super) fn begin(&mut self, wid: WindowId, target: CGRect) -> Arc<Mutex<bool>> {
        let cancelled = Arc::new(Mutex::new(false));
        self.active.insert(wid, Popping {
            target,
            cancelled: cancelled.clone(),
            landed: false,
        });
        cancelled
    }

    /// True while `wid`'s picture draws its border, so the border thread leaves it out.
    pub(super) fn draws_border(&self, wid: WindowId) -> bool {
        self.active.get(&wid).is_some_and(|p| !p.landed && !*p.cancelled.lock())
    }

    /// `wid` sits in its tile under its picture: its real border takes over.
    pub(super) fn land(&mut self, wid: WindowId) {
        if let Some(popping) = self.active.get_mut(&wid) {
            popping.landed = true;
        }
    }

    /// Hands `wid` back to the caller, which must place it: the picture goes
    /// away within one poll and the pop-in no longer moves the window.
    pub(super) fn cancel(&mut self, wid: WindowId) {
        if let Some(popping) = self.active.get(&wid) {
            *popping.cancelled.lock() = true;
        }
    }

    /// True if `wid` was popping in, so the layout should be applied again.
    pub(super) fn finish(&mut self, wid: WindowId) -> bool { self.active.remove(&wid).is_some() }
}

/// Starts a pop-in for `wid`, which just opened, if it can pop in; otherwise the
/// caller places it. `park_txid` is the transaction the caller already registered
/// for `target`. The neighbors' slide starts now, and the pop-in settles with it.
pub(super) fn try_start(
    reactor: &mut Reactor,
    space: SpaceId,
    wid: WindowId,
    park_txid: TransactionId,
    target: CGRect,
) -> bool {
    // During a burst of new windows, only the first pops in: each pop-in captures its window
    // and composites an overlay, and that WindowServer work would slow every window after it.
    if reactor.animation_tx.is_none()
        || !reactor.config.settings.window_open_close_animation
        || !reactor.pop_ins.active.is_empty()
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
    let settings = &reactor.config.settings;
    let duration = Duration::from_secs_f64(settings.animation_duration);
    Job {
        handle,
        wid,
        wsid,
        screen: screen.frame,
        screens,
        target,
        park_txid,
        final_txid,
        duration,
        slide_ends: Instant::now() + duration,
        easing: settings.animation_easing,
        outline: settings.ui.borders.enabled.then(|| settings.ui.borders.clone()),
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
    /// When the neighbors' slide ends; the picture grows in until then.
    pub slide_ends: Instant,
    pub easing: AnimationEasing,
    /// The border settings, when borders are on: the picture draws the window's outline.
    pub outline: Option<BorderSettings>,
    pub events: Option<Sender>,
    /// Set by the reactor when a layout takes the window back (workspace switch, display change).
    pub cancelled: Arc<Mutex<bool>>,
}

impl Job {
    // ponytail: one thread per opening window; fine at human window-opening rates.
    pub(super) fn spawn(self) {
        let fallback = self.clone();
        let spawned = thread::Builder::new().name("wave-pop-in".into()).spawn(move || self.run());
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
        let Some(opened) = window_server::get_window(self.wsid).map(|info| info.frame) else {
            return false;
        };
        let deadline = Instant::now() + FIRST_DRAW;
        let picture = loop {
            if let Some(picture) = Picture::take(self.wsid, CAPTURE_OPTIONS, opened.size) {
                break picture;
            }
            if Instant::now() >= deadline {
                debug!(wid = ?self.wid, "pop-in skipped: window did not draw where it opened");
                return false;
            }
            thread::sleep(FRAME_POLL);
        };
        step("captured");
        // Out of sight, the window takes its tile's size and redraws at it.
        let parked = HiddenWindowPlacement::calculate(
            self.screen,
            self.target,
            HideCorner::BottomRight,
            &self.screens,
        );
        self.write(parked, self.park_txid);
        // The overlay starts out shrunk and see-through, so making it now shows nothing.
        let overlay = match Overlay::new(
            self.target,
            Some(&picture),
            self.outline.as_ref(),
            Some(self.wsid),
            Start::Hidden,
        ) {
            Ok(overlay) => overlay,
            Err(error) => {
                debug!(wid = ?self.wid, ?error, "pop-in skipped: overlay failed");
                return false;
            }
        };
        let resized = |frame: CGRect| {
            frame.size.same_as(self.target.size) && !frame.origin.same_as(opened.origin)
        };
        if wait_for_frame(self.wsid, resized).is_none() {
            debug!(wid = ?self.wid, "pop-in skipped: window did not take its tile size off screen");
            return false;
        }
        step("parked");
        if self.is_cancelled() {
            return true;
        }
        let duration = self
            .slide_ends
            .saturating_duration_since(Instant::now())
            .max(self.duration.mul_f64(MIN_POP_SHARE));
        overlay.animate(1.0, 1.0, duration, self.easing);
        if !self.sleep_unless_interrupted(duration) || !self.place_unless_cancelled() {
            // Closed or taken back mid-flight: drop the picture now, leave the window be.
            debug!(wid = ?self.wid, "pop-in cut short");
            return true;
        }
        step("placed");
        // Keep covering until the real window is in its tile and has drawn there: a launching
        // app can take a while to apply the move, and an app that stops drawing off screen
        // shows nothing until it draws again.
        let deadline = Instant::now() + LAND_TIMEOUT;
        while !(window_server::get_window(self.wsid)
            .is_some_and(|info| info.frame.origin.same_as(self.target.origin))
            && Picture::take(self.wsid, CLOSE_CAPTURE_OPTIONS, self.target.size).is_some())
        {
            if Instant::now() >= deadline || !self.sleep_unless_interrupted(FRAME_POLL) {
                break;
            }
        }
        step("landed");
        self.send(Event::PopInLanded(self.wid));
        if self.sleep_unless_interrupted(BORDER_HANDOFF) {
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
pub(super) fn bezier(easing: AnimationEasing) -> [f32; 4] {
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
    // SAFETY: `id` is one valid u32 for the duration of the call, matching the count of 1.
    let images = unsafe { SLSHWCaptureWindowList(*G_CONNECTION, &id, 1, options) };
    // SAFETY: the call returns a +1 CFArray of CGImages (a Copy-rule function), or null.
    let images: CFRetained<CFArray<CGImage>> =
        unsafe { CFRetained::from_raw(NonNull::new(images)?) };
    images.iter().next()
}

/// How an overlay starts out.
#[derive(Clone, Copy, Debug)]
enum Start {
    /// At full size and opaque, standing in for a window that just closed.
    Shown,
    /// Shrunk and see-through, ready to grow in.
    Hidden,
}

/// A window's picture, with its own copy of the pixels: a hardware capture can share the window's
/// live backing store, which empties out when the window moves or redraws.
struct Picture {
    image: CFRetained<CGImage>,
    /// The window's size when it was taken, in points.
    size: CGSize,
    /// The window's background, for the part of a bigger tile the picture doesn't reach.
    background: Option<CFRetained<CGColor>>,
}

impl Picture {
    /// Captures the window, `size` points big. `None` while it hasn't drawn, which captures as all
    /// zero bytes whatever the pixel format, or if part of it is off screen and so missing.
    fn take(wsid: WindowServerId, options: u32, size: CGSize) -> Option<Self> {
        // The capture leaves autoreleased buffers behind, and a pop-in captures every few
        // milliseconds while it waits for the window.
        autoreleasepool(|_| Self::take_in_pool(wsid, options, size))
    }

    fn take_in_pool(wsid: WindowServerId, options: u32, size: CGSize) -> Option<Self> {
        let image = capture(wsid, options)?;
        let width = CGImage::width(Some(&image));
        let height = CGImage::height(Some(&image));
        let [pixels_wide, pixels_high] =
            [width, height].map(|n| u32::try_from(n).map_or(0.0, f64::from));
        if size.width < 1.0
            || size.height < 1.0
            || (pixels_wide / size.width - pixels_high / size.height).abs() > 0.02
        {
            return None;
        }
        let data = CGImage::data_provider(Some(&image))
            .and_then(|provider| CGDataProvider::data(Some(&provider)))?;
        // SAFETY: the CFData is immutable and this function holds the only reference to it.
        let bytes = unsafe { data.as_bytes_unchecked() };
        let row = CGImage::bytes_per_row(Some(&image));
        let pixel = (CGImage::bits_per_pixel(Some(&image)) / 8).max(1);
        // A few rows across the middle, every 16th pixel: corners can be transparent by design.
        let drawn = [1, 2, 3].into_iter().any(|quarter| {
            let start = row * (height * quarter / 4);
            bytes.get(start..start + row).is_some_and(|line| {
                line.chunks(pixel).step_by(16).any(|px| px.iter().any(|&byte| byte != 0))
            })
        });
        if !drawn {
            return None;
        }
        // Sampled low and to the right, clear of most content.
        let sample = row * (height * 3 / 4) + pixel * (width * 7 / 8);
        let background = match bytes.get(sample..sample + 4) {
            Some(&[b, g, r, a]) if CGImage::bitmap_info(Some(&image)).0 == BGRA && a > 0 => {
                let alpha = f64::from(a);
                Some(CGColor::new_generic_rgb(
                    f64::from(r) / alpha,
                    f64::from(g) / alpha,
                    f64::from(b) / alpha,
                    alpha / 255.0,
                ))
            }
            _ => None,
        };
        let provider = CGDataProvider::with_cf_data(Some(&data))?;
        // SAFETY: every layout argument is read from `image`, whose pixels `provider` holds, and
        // a null decode array means no remapping.
        let image = unsafe {
            CGImage::new(
                width,
                height,
                CGImage::bits_per_component(Some(&image)),
                CGImage::bits_per_pixel(Some(&image)),
                row,
                CGImage::color_space(Some(&image)).as_deref(),
                CGImage::bitmap_info(Some(&image)),
                Some(&provider),
                std::ptr::null(),
                false,
                CGImage::rendering_intent(Some(&image)),
            )
        }?;
        Some(Self { image, size, background })
    }
}

/// A wave-owned window over a tile that shows a window's captured picture, with the window's
/// outline around it when borders are on.
struct Overlay {
    // The surface must be detached before its owning WindowServer window drops.
    surface: WindowSurface,
    window: CgsWindow,
    /// The picture and its outline, scaled and faded together about the tile's center.
    tile: Retained<CALayer>,
}

impl Overlay {
    /// Covers `target`, plus the outline's width around it. With no window to sit above, it goes
    /// on top of the normal windows.
    fn new(
        target: CGRect,
        picture: Option<&Picture>,
        outline: Option<&BorderSettings>,
        above: Option<WindowServerId>,
        start: Start,
    ) -> Result<Self, CgsWindowError> {
        let outline = outline.and_then(|settings| {
            let layer = CALayer::layer();
            border::draw_focused_outline(&layer, settings)
                .map(|(width, radius)| (layer, width, radius))
        });
        let reach = outline.as_ref().map_or(0.0, |(_, width, _)| *width);
        let frame = outset(target, reach);
        let window = CgsWindow::new_compositor(frame, 0.0)?;
        window.disable_shadow();
        window.set_alpha(1.0)?;
        window.set_tags(1 << 3)?;

        let bounds = CGRect::new(CGPoint::ZERO, frame.size);
        let root = CALayer::layer();
        let tile = CALayer::layer();
        let picture_layer = CALayer::layer();
        with_disabled_actions(|| {
            root.setBounds(bounds);
            root.setPosition(CGPoint::ZERO);
            root.setAnchorPoint(CGPoint::ZERO);
            root.setOpaque(false);
            // Top-left origin, like the screen frames wave works in.
            root.setGeometryFlipped(true);
            // The tile keeps its center anchor, so it scales about the tile's center.
            tile.setFrame(bounds);
            if let Some((layer, _, radius)) = &outline {
                layer.setFrame(bounds);
                tile.addSublayer(layer);
                picture_layer.setCornerRadius(*radius);
            }
            picture_layer.setFrame(CGRect::new(CGPoint::new(reach, reach), target.size));
            // A picture from a different size than the tile is cropped or padded, never stretched.
            picture_layer.setMasksToBounds(true);
            // SAFETY: a constant string Core Animation exports.
            picture_layer.setContentsGravity(unsafe { kCAGravityTopLeft });
            tile.addSublayer(&picture_layer);
            if let Start::Hidden = start {
                tile.setTransform(CATransform3D::new_scale(POP_SCALE, POP_SCALE, 1.0));
                tile.setOpacity(0.0);
            }
            root.addSublayer(&tile);
        });
        if let Some(picture) = picture {
            show_picture(&picture_layer, picture);
        }
        let surface = WindowSurface::new(window.id(), bounds, &root)?;
        surface.flush();
        window.order_above(above.map(|id| id.as_u32()))?;
        Ok(Self { surface, window, tile })
    }

    /// Scales and fades the picture and its outline on the GPU, which runs the motion.
    fn animate(&self, scale: f64, opacity: f32, duration: Duration, easing: AnimationEasing) {
        let [x1, y1, x2, y2] = bezier(easing);
        CATransaction::begin();
        CATransaction::setAnimationDuration(duration.as_secs_f64());
        CATransaction::setAnimationTimingFunction(Some(
            &CAMediaTimingFunction::functionWithControlPoints(x1, y1, x2, y2),
        ));
        self.tile.setTransform(CATransform3D::new_scale(scale, scale, 1.0));
        // Fading in finishes early, so whatever the neighbors uncover never shows through.
        let fade_share = if opacity > 0.0 { FADE_IN_SHARE } else { 1.0 };
        CATransaction::begin();
        CATransaction::setAnimationDuration(duration.as_secs_f64() * fade_share);
        self.tile.setOpacity(opacity);
        CATransaction::commit();
        CATransaction::commit();
        self.surface.flush();
    }

    fn fade_out(&self, duration: Duration) {
        CATransaction::begin();
        CATransaction::setAnimationDuration(duration.as_secs_f64());
        self.tile.setOpacity(0.0);
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
        outline: Option<BorderSettings>,
    },
    SlideStarted,
}

/// Keeps one picture of the focused tiled window, so it can shrink away when it closes.
#[derive(Debug)]
pub(super) struct Closer {
    tx: crossbeam_channel::Sender<CloseMsg>,
    /// The window and frame the current picture is for, to skip redundant refreshes.
    watched: Option<(WindowId, CGRect)>,
}

impl Closer {
    pub(super) fn spawn() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let spawned =
            thread::Builder::new().name("wave-close".into()).spawn(move || run_closer(rx));
        if let Err(error) = spawned {
            warn!(%error, "close-animation thread failed to start; closes will not animate");
        }
        Self { tx, watched: None }
    }

    /// Refreshes the picture when the focused window or its frame changes.
    pub(super) fn watch(&mut self, wid: WindowId, frame: CGRect) {
        if self.watched.is_some_and(|(w, f)| w == wid && f.same_as(frame)) {
            return;
        }
        self.watched = Some((wid, frame));
        _ = self.tx.send(CloseMsg::Watch(wid));
    }

    /// Shrinks `wid`'s picture away from `frame`, if the picture is of `wid`. The picture
    /// carries the focused window's outline when `outline` is set.
    pub(super) fn closed(
        &mut self,
        wid: WindowId,
        frame: CGRect,
        duration: Duration,
        easing: AnimationEasing,
        outline: Option<BorderSettings>,
    ) {
        if self.watched.is_some_and(|(w, _)| w == wid) {
            self.watched = None;
        }
        _ = self.tx.send(CloseMsg::Closed {
            wid,
            frame,
            duration,
            easing,
            outline,
        });
    }

    /// The neighbors started sliding: a closed window's picture now shrinks with them.
    pub(super) fn slide_started(&self) { _ = self.tx.send(CloseMsg::SlideStarted); }
}

fn run_closer(rx: crossbeam_channel::Receiver<CloseMsg>) {
    use crossbeam_channel::RecvTimeoutError;
    let mut picture: Option<(WindowId, CFRetained<CGImage>)> = None;
    // The focused window to capture once focus has rested until the deadline.
    let mut pending: Option<(WindowId, Instant)> = None;
    let mut covering: Option<Cover> = None;
    loop {
        let deadline = [
            pending.map(|(_, at)| at),
            covering.as_ref().map(|c| c.deadline),
        ]
        .into_iter()
        .flatten()
        .min();
        let msg = match deadline {
            Some(at) => rx.recv_deadline(at),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        if let Err(RecvTimeoutError::Disconnected) = msg {
            return;
        }
        // This thread lives as long as wave, so nothing drains its autoreleased Core
        // Animation objects unless each message gets its own pool. Without it every
        // close overlay, and the window picture it holds, stays in memory.
        autoreleasepool(|_| match msg {
            Err(RecvTimeoutError::Disconnected) => {}
            Err(RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                if covering.as_ref().is_some_and(|c| c.deadline <= now) {
                    debug!("close slide never came; shrinking anyway");
                    if let Some(cover) = covering.take() {
                        cover.shrink();
                    }
                }
                if let Some((wid, _)) = pending.take_if(|(_, at)| *at <= now) {
                    let wsid = WindowServerId::new(wid.idx.get());
                    picture = CGPreflightScreenCaptureAccess()
                        .then(|| capture(wsid, CLOSE_CAPTURE_OPTIONS))
                        .flatten()
                        .map(|image| (wid, image));
                    debug!(?wid, captured = picture.is_some(), "close picture refreshed");
                }
            }
            Ok(CloseMsg::Watch(wid)) => pending = Some((wid, Instant::now() + WATCH_DEBOUNCE)),
            Ok(CloseMsg::Closed {
                wid,
                frame,
                duration,
                easing,
                outline,
            }) => {
                pending.take_if(|(w, _)| *w == wid);
                if let Some(cover) = covering.take() {
                    cover.shrink();
                }
                match picture.take_if(|(w, _)| *w == wid) {
                    Some((_, image)) => {
                        covering = Cover::new(frame, &image, outline.as_ref(), duration, easing);
                    }
                    None => debug!(?wid, "close not animated: no picture of this window"),
                }
            }
            Ok(CloseMsg::SlideStarted) => {
                if let Some(cover) = covering.take() {
                    debug!("close picture shrinking with the neighbors' slide");
                    cover.shrink();
                }
            }
        });
    }
}

/// A closed window's picture, holding its spot at full size until the neighbors move.
struct Cover {
    overlay: Overlay,
    duration: Duration,
    easing: AnimationEasing,
    /// Shrink by now even if no slide starts (the last window on a workspace closed).
    deadline: Instant,
}

impl Cover {
    fn new(
        frame: CGRect,
        image: &CFRetained<CGImage>,
        outline: Option<&BorderSettings>,
        duration: Duration,
        easing: AnimationEasing,
    ) -> Option<Self> {
        let picture = Picture {
            image: image.clone(),
            size: frame.size,
            background: None,
        };
        match Overlay::new(frame, Some(&picture), outline, None, Start::Shown) {
            Ok(overlay) => Some(Self {
                overlay,
                duration,
                easing,
                deadline: Instant::now() + COVER_TIMEOUT,
            }),
            Err(error) => {
                debug!(?error, "close animation skipped: overlay failed");
                None
            }
        }
    }

    fn shrink(self) {
        self.overlay.animate(POP_SCALE, 0.0, self.duration, self.easing);
        thread::sleep(self.duration);
    }
}

/// Shows `picture` on `layer`.
fn show_picture(layer: &CALayer, picture: &Picture) {
    // Pixels per point, so a Retina capture stays sharp.
    let scale = u32::try_from(CGImage::width(Some(&picture.image)))
        .map_or(2.0, |pixels| f64::from(pixels) / picture.size.width);
    with_disabled_actions(|| {
        layer.setContentsScale(scale);
        layer.setBackgroundColor(picture.background.as_deref());
        // SAFETY: A CGImage is a CF type toll-free bridged to an Objective-C object.
        let contents =
            unsafe { &*std::ptr::from_ref::<CGImage>(&picture.image).cast::<AnyObject>() };
        // SAFETY: CALayer.contents takes a CGImage, and the layer retains it.
        unsafe { layer.setContents(Some(contents)) };
    });
}

/// `rect` grown by `by` on every side.
fn outset(rect: CGRect, by: f64) -> CGRect {
    CGRect::new(
        CGPoint::new(rect.origin.x - by, rect.origin.y - by),
        CGSize::new(rect.size.width + 2.0 * by, rect.size.height + 2.0 * by),
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
    fn the_overlay_reaches_out_to_cover_the_outline() {
        let tile = CGRect::new(CGPoint::new(100.0, 200.0), CGSize::new(1000.0, 400.0));
        assert_eq!(
            outset(tile, 2.0),
            CGRect::new(CGPoint::new(98.0, 198.0), CGSize::new(1004.0, 404.0))
        );
        assert_eq!(outset(tile, 0.0), tile);
    }

    #[test]
    fn the_real_border_waits_until_the_window_lands_or_is_taken_back() {
        let mut pop_ins = PopIns::default();
        pop_ins.begin(wid(1), rect(0.0));
        pop_ins.begin(wid(2), rect(0.0));
        assert!(pop_ins.draws_border(wid(1)));
        pop_ins.land(wid(1));
        assert!(!pop_ins.draws_border(wid(1)));
        pop_ins.cancel(wid(2));
        assert!(!pop_ins.draws_border(wid(2)));
        assert!(!pop_ins.draws_border(wid(3)));
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
