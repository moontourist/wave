use std::ffi::c_void;
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use tracing::info;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;

    // A CFString the framework sets before main runs.
    safe static kAXTrustedCheckOptionPrompt: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    // CFBoolean singletons the framework sets before main runs.
    safe static kCFBooleanTrue: *const c_void;
    safe static kCFBooleanFalse: *const c_void;
}

const AX_POLL_INTERVAL: Duration = Duration::from_millis(250);
const AX_POLL_TIMEOUT: Duration = Duration::from_secs(30);

/// Asks whether rift has Accessibility access. With `prompt`, macOS also shows its grant dialog.
fn ax_trusted(prompt: bool) -> bool {
    autoreleasepool(|_| {
        let value = if prompt {
            kCFBooleanTrue
        } else {
            kCFBooleanFalse
        };
        let keys: [*mut AnyObject; 1] = [kAXTrustedCheckOptionPrompt as *mut AnyObject];
        let vals: [*mut AnyObject; 1] = [value as *mut AnyObject];
        // SAFETY: Both arrays hold one valid CF object, an immutable framework constant.
        let dict: *mut AnyObject = unsafe {
            msg_send![
                class!(NSDictionary),
                dictionaryWithObjects: vals.as_ptr(),
                forKeys:              keys.as_ptr(),
                count:                1usize
            ]
        };
        // SAFETY: `dict` is an autoreleased NSDictionary that lives until the pool drains.
        unsafe { AXIsProcessTrustedWithOptions(dict.cast()) }
    })
}

pub fn ensure_accessibility_permission() {
    if ax_trusted(false) {
        return;
    }

    info!("Accessibility permission is not granted; prompting user for permission now.");

    let _ = ax_trusted(true);

    let start = Instant::now();
    loop {
        if ax_trusted(false) {
            info!("Accessibility permission granted");
            return;
        }

        if start.elapsed() >= AX_POLL_TIMEOUT {
            break;
        }

        thread::sleep(AX_POLL_INTERVAL);
    }

    println!(
        "Rift still does not have accessibility permission. Enable it in System Settings > Privacy & Security > Accessibility, then restart Rift."
    );

    std::process::exit(1);
}
