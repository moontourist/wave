use std::ffi::c_void;

use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFRunLoopMode, CFRunLoopSource, kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGEvent, CGEventMask, CGEventTapLocation as CGTapLoc, CGEventTapOptions as CGTapOpt,
    CGEventTapPlacement as CGTapPlace, CGEventTapProxy, CGEventType,
};
use tracing::{error, warn};

pub type TapCallback = Option<
    unsafe extern "C-unwind" fn(
        CGEventTapProxy,
        CGEventType,
        core::ptr::NonNull<CGEvent>,
        *mut c_void,
    ) -> *mut CGEvent,
>;

pub type TapReenabledCallback = Option<unsafe extern "C-unwind" fn(*mut c_void)>;
pub type TapInvalidatedCallback = Option<unsafe extern "C-unwind" fn(*mut c_void)>;

struct TrampolineCtx {
    callback: TapCallback,
    original_user_info: *mut c_void,
    original_drop: Option<unsafe fn(*mut c_void)>,
    reenabled_callback: TapReenabledCallback,
    invalidated_callback: TapInvalidatedCallback,
    port_ptr: Option<core::ptr::NonNull<CFMachPort>>,
}

extern "C-unwind" fn port_invalidated(_port: *mut CFMachPort, user_info: *mut c_void) {
    if user_info.is_null() {
        return;
    }

    // SAFETY: A non-null `user_info` is the TrampolineCtx box EventTap::new installed, alive until
    // EventTap drops after invalidating the port.
    let ctx = unsafe { &*(user_info as *const TrampolineCtx) };
    warn!("Event tap Mach port was invalidated; scheduling tap recreation");
    if let Some(callback) = ctx.invalidated_callback {
        // SAFETY: EventTap::new's caller guarantees its callbacks accept `original_user_info` for
        // the tap's lifetime.
        unsafe { callback(ctx.original_user_info) };
    }
}

extern "C-unwind" fn trampoline_callback(
    proxy: CGEventTapProxy,
    etype: CGEventType,
    event_ref: core::ptr::NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    if user_info.is_null() {
        return event_ref.as_ptr();
    }

    // SAFETY: A non-null `user_info` is the TrampolineCtx box EventTap::new installed, alive until
    // EventTap drops after invalidating the port.
    let ctx = unsafe { &*(user_info as *const TrampolineCtx) };

    // kCGEventTapDisabledByTimeout (-2) & kCGEventTapDisabledByUserInput (-1)
    let ety = etype.0 as i32;
    if ety == -1 || ety == -2 {
        if let Some(port_ptr) = ctx.port_ptr {
            // SAFETY: `port_ptr` points at the EventTap's retained Mach port, which outlives every
            // callback the port delivers.
            let port = unsafe { port_ptr.as_ref() };
            let reason = if ety == -2 { "timeout" } else { "user input" };
            warn!(reason, "Event tap was disabled; re-enabling it");
            CGEvent::tap_enable(port, true);
            if CGEvent::tap_is_enabled(port) {
                if let Some(callback) = ctx.reenabled_callback {
                    // SAFETY: EventTap::new's caller guarantees its callbacks accept
                    // `original_user_info` for the tap's lifetime.
                    unsafe { callback(ctx.original_user_info) };
                }
            } else {
                error!(reason, "Event tap did not re-enable; scheduling tap recreation");
                if let Some(callback) = ctx.invalidated_callback {
                    // SAFETY: EventTap::new's caller guarantees its callbacks accept
                    // `original_user_info` for the tap's lifetime.
                    unsafe { callback(ctx.original_user_info) };
                }
            }
        }

        return event_ref.as_ptr();
    }

    if let Some(orig_cb) = ctx.callback {
        // SAFETY: EventTap::new's caller guarantees `orig_cb` accepts these tap arguments and
        // `original_user_info`.
        return unsafe { orig_cb(proxy, etype, event_ref, ctx.original_user_info) };
    }

    event_ref.as_ptr()
}

unsafe fn trampoline_drop(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }

    // SAFETY: `ptr` is the TrampolineCtx box from EventTap::new, and EventTap::drop calls this
    // once.
    let ctx: Box<TrampolineCtx> = unsafe { Box::from_raw(ptr as *mut TrampolineCtx) };
    if let Some(dropper) = ctx.original_drop {
        if !ctx.original_user_info.is_null() {
            // SAFETY: EventTap::new's caller passed `original_drop` to free `original_user_info`
            // exactly once.
            unsafe { dropper(ctx.original_user_info) };
        }
    }
}

pub struct EventTap {
    port: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
    user_info: *mut c_void,
    drop_ctx: Option<unsafe fn(*mut c_void)>,
}

impl EventTap {
    /// Creates a tap on the current run loop (Rift input uses active HID).
    /// On failure the caller retains ownership of `user_info`.
    ///
    /// # Safety
    ///
    /// `callback`, `reenabled_callback` and `invalidated_callback` must be sound to call with
    /// `user_info` on this thread's run loop for as long as the tap lives. If `drop_ctx` is
    /// given, it must free `user_info` exactly once.
    pub unsafe fn new(
        location: CGTapLoc,
        options: CGTapOpt,
        mask: CGEventMask,
        callback: TapCallback,
        user_info: *mut c_void,
        drop_ctx: Option<unsafe fn(*mut c_void)>,
        reenabled_callback: TapReenabledCallback,
        invalidated_callback: TapInvalidatedCallback,
    ) -> Option<Self> {
        let tramp = Box::new(TrampolineCtx {
            callback,
            original_user_info: user_info,
            original_drop: drop_ctx,
            reenabled_callback,
            invalidated_callback,
            port_ptr: None,
        });
        let tramp_ptr = Box::into_raw(tramp) as *mut c_void;

        // SAFETY: `trampoline_callback` matches the tap callback ABI, and `tramp_ptr` is a live
        // TrampolineCtx box it reads.
        let port = unsafe {
            CGEvent::tap_create(
                location,
                // Append so active taps installed earlier at the HID level, such as
                // Raycast's Hyper Key remap, see events before rift does.
                CGTapPlace::TailAppendEventTap,
                options,
                mask,
                Some(trampoline_callback),
                tramp_ptr,
            )
        };
        let Some(port) = port else {
            // SAFETY: The tap was never created, so `tramp_ptr` is still this function's unshared
            // box.
            unsafe {
                drop(Box::from_raw(tramp_ptr as *mut TrampolineCtx));
            }
            return None;
        };
        let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
            port.invalidate();
            // SAFETY: The source failed and the port is invalidated, so `tramp_ptr` is still this
            // function's box with no other reader.
            unsafe {
                drop(Box::from_raw(tramp_ptr as *mut TrampolineCtx));
            }
            return None;
        };
        if let Some(rl) = CFRunLoop::current()
            // SAFETY: kCFRunLoopCommonModes is an immutable CoreFoundation constant.
            && let Some(mode) = unsafe { kCFRunLoopCommonModes }
        {
            let mode: &CFRunLoopMode = mode;
            rl.add_source(Some(&source), Some(mode));
        }
        CGEvent::tap_enable(&port, true);

        let event_tap = Self {
            port,
            source,
            user_info: tramp_ptr,
            drop_ctx: Some(trampoline_drop),
        };

        // SAFETY: `tramp_ptr` is the live TrampolineCtx box; no callback runs until the run
        // loop does, so this write doesn't race. `port_invalidated` reads that same box.
        unsafe {
            let tramp_ctx = &mut *(tramp_ptr as *mut TrampolineCtx);
            tramp_ctx.port_ptr = Some(core::ptr::NonNull::from(&*event_tap.port));
            event_tap.port.set_invalidation_call_back(Some(port_invalidated));
        }

        Some(event_tap)
    }
}

impl Drop for EventTap {
    fn drop(&mut self) {
        if self.port.is_valid() {
            // Intentional teardown/replacement must not be mistaken for an
            // unexpected Mach-port failure by the event-driven recovery path.
            // SAFETY: Clearing the invalidation callback only stops future calls into the
            // trampoline box.
            unsafe { self.port.set_invalidation_call_back(None) };
            CGEvent::tap_enable(&self.port, false);
        }
        if let Some(rl) = CFRunLoop::current() {
            // SAFETY: kCFRunLoopCommonModes is an immutable CoreFoundation constant.
            rl.remove_source(Some(&self.source), unsafe { kCFRunLoopCommonModes });
        }
        self.port.invalidate();
        if let Some(dropper) = self.drop_ctx {
            // SAFETY: `drop_ctx` is trampoline_drop for the `user_info` box, and the invalidated
            // port no longer calls into it.
            unsafe { dropper(self.user_info) };
        }
    }
}
