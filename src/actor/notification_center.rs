//! This actor manages the global notification queue, which tells us when an
//! application is launched or focused or the screen state changes.

use std::cell::Cell;
use std::ffi::c_void;
use std::future;

use dispatchr::queue;
use dispatchr::time::Time;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{AnyThread, DeclaredClass, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{
    self, NSApplicationDidChangeScreenParametersNotification, NSRunningApplication, NSWorkspace,
    NSWorkspaceActiveSpaceDidChangeNotification, NSWorkspaceApplicationKey,
    NSWorkspaceDidActivateApplicationNotification, NSWorkspaceDidDeactivateApplicationNotification,
    NSWorkspaceDidWakeNotification, NSWorkspaceSessionDidBecomeActiveNotification,
    NSWorkspaceSessionDidResignActiveNotification, NSWorkspaceWillSleepNotification,
};
use objc2_foundation::{
    NSDistributedNotificationCenter, NSNotification, NSNotificationCenter,
    NSNotificationSuspensionBehavior, NSObject, NSProcessInfo, NSString,
};
use tracing::{debug, info_span, trace, warn};

use super::spaces;
use super::wm_controller::{self, WmEvent};
use crate::sys::app::NSRunningApplicationExt;
use crate::sys::dispatch::DispatchExt;
use crate::sys::power::{init_power_state, set_low_power_mode_state};
use crate::sys::skylight::{CGDisplayRegisterReconfigurationCallback, DisplayReconfigFlags};

#[repr(C)]
struct Instance {
    events_tx: wm_controller::Sender,
    spaces_tx: spaces::Sender,
    session_inactive_hint: Cell<bool>,
}

define_class! {
    // SAFETY:
    // - The superclass NSObject does not have any subclassing requirements.
    // - `NotificationHandler` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[ivars = Box<Instance>]
    struct NotificationCenterInner;

    // SAFETY: Each of these method signatures must match their invocations.
    impl NotificationCenterInner {
        #[unsafe(method(recvScreenChangedEvent:))]
        fn recv_screen_changed_event(&self, notif: &NSNotification) {
            trace!("{notif:#?}");
            self.handle_screen_changed_event(notif);
        }

        #[unsafe(method(recvAppEvent:))]
        fn recv_app_event(&self, notif: &NSNotification) {
            trace!("{notif:#?}");
            self.handle_app_event(notif);
        }

        #[unsafe(method(recvWakeEvent:))]
        fn recv_wake_event(&self, notif: &NSNotification) {
            trace!("{notif:#?}");
            self.send_space_event(spaces::Event::SystemDidWake);
        }

        #[unsafe(method(recvSleepEvent:))]
        fn recv_sleep_event(&self, notif: &NSNotification) {
            trace!("{notif:#?}");
            self.send_space_event(spaces::Event::SystemWillSleep);
        }

        #[unsafe(method(recvSessionEvent:))]
        fn recv_session_event(&self, notif: &NSNotification) {
            trace!("{notif:#?}");
            self.handle_session_event(notif);
        }

        #[unsafe(method(recvPowerEvent:))]
        fn recv_power_event(&self, notif: &NSNotification) {
            trace!("{notif:#?}");
            self.handle_power_event(notif);
        }

        #[unsafe(method(recvMenuBarPrefChanged:))]
        fn recv_menu_bar_pref_changed(&self, notif: &NSNotification) {
            trace!("{notif:#?}");
            self.handle_menu_bar_pref_changed();
        }

        #[unsafe(method(recvDockPrefChanged:))]
        fn recv_dock_pref_changed(&self, notif: &NSNotification) {
            trace!("{notif:#?}");
            self.handle_dock_pref_changed();
        }

        #[unsafe(method(recvKeyboardLayoutChanged:))]
        fn recv_keyboard_layout_changed(&self, notif: &NSNotification) {
            trace!("{notif:#?}");
            // Workspace notifications are delivered on the main thread.
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            crate::sys::hotkey::refresh_virtual_keymap(mtm);
            self.send_event(WmEvent::KeyboardLayoutChanged);
        }
    }
}

impl NotificationCenterInner {
    fn new(events_tx: wm_controller::Sender, spaces_tx: spaces::Sender) -> Retained<Self> {
        let instance = Instance {
            events_tx,
            spaces_tx,
            session_inactive_hint: Cell::new(false),
        };
        let this = Self::alloc().set_ivars(Box::new(instance));
        // SAFETY: NSObject's init takes no arguments, and the ivars are set above.
        let handler: Retained<Self> = unsafe { msg_send![super(this), init] };
        // SAFETY: The callback matches the reconfiguration ABI. The handler stays alive while
        // NotificationCenter holds it, which is the life of the process.
        unsafe {
            CGDisplayRegisterReconfigurationCallback(
                Some(Self::display_reconfig_callback),
                Retained::<NotificationCenterInner>::as_ptr(&handler) as *mut c_void,
            );
        }
        handler
    }

    fn enter_session_inactive(&self) {
        if !self.ivars().session_inactive_hint.replace(true) {
            self.send_space_event(spaces::Event::SessionDidResignActive);
        }
    }

    fn leave_session_inactive(&self) {
        if self.ivars().session_inactive_hint.replace(false) {
            self.send_space_event(spaces::Event::SessionDidBecomeActive);
        }
    }

    fn handle_screen_changed_event(&self, notif: &NSNotification) {
        use objc2_app_kit::*;
        let name = &*notif.name();
        let span = info_span!("notification_center::handle_screen_changed_event", ?name);
        let _s = span.enter();
        // SAFETY: AppKit's notification names are immutable framework constants.
        let (space_changed, screen_parameters_changed) = unsafe {
            (
                NSWorkspaceActiveSpaceDidChangeNotification,
                NSApplicationDidChangeScreenParametersNotification,
            )
        };
        if name.to_string() == "NSWorkspaceActiveDisplayDidChangeNotification" {
            self.send_space_event(spaces::Event::ActiveDisplayChanged);
        } else if space_changed == name {
            self.send_space_event(spaces::Event::ActiveSpaceChanged);
        } else if screen_parameters_changed == name {
            self.send_space_event(spaces::Event::ScreenRefreshRequested);
        } else {
            warn!("Unexpected screen changed event: {notif:?}");
        }
    }

    fn handle_session_event(&self, notif: &NSNotification) {
        use objc2_app_kit::*;
        let name = &*notif.name();
        let span = info_span!("notification_center::handle_session_event", ?name);
        let _guard = span.enter();

        // SAFETY: AppKit's notification names are immutable framework constants.
        let (resigned, became_active) = unsafe {
            (
                NSWorkspaceSessionDidResignActiveNotification,
                NSWorkspaceSessionDidBecomeActiveNotification,
            )
        };
        if resigned == name || name.to_string() == "com.apple.screenIsLocked" {
            self.enter_session_inactive();
        } else if became_active == name || name.to_string() == "com.apple.screenIsUnlocked" {
            self.leave_session_inactive();
        } else {
            warn!("Unexpected session event: {notif:?}");
        }
    }

    fn handle_power_event(&self, _notif: &NSNotification) {
        let span = info_span!("notification_center::handle_power_event");
        let _s = span.enter();

        let process_info = NSProcessInfo::processInfo();
        let current_state = process_info.isLowPowerModeEnabled();
        let old_state = set_low_power_mode_state(current_state);

        if old_state != current_state {
            debug!("Low power mode changed: {} -> {}", old_state, current_state);
            self.send_event(WmEvent::PowerStateChanged(current_state));
        }
    }

    fn handle_app_event(&self, notif: &NSNotification) {
        use objc2_app_kit::*;
        let Some(app) = self.running_application(notif) else {
            return;
        };
        let pid = app.pid();
        let name = &*notif.name();
        let span = info_span!("notification_center::handle_app_event", ?name);
        let _guard = span.enter();
        // SAFETY: AppKit's notification names are immutable framework constants.
        let (deactivated, activated) = unsafe {
            (
                NSWorkspaceDidDeactivateApplicationNotification,
                NSWorkspaceDidActivateApplicationNotification,
            )
        };
        if deactivated == name {
            self.send_event(WmEvent::AppGloballyDeactivated(pid));
        } else if activated == name {
            // Do not forward AppGloballyActivated from NSWorkspace here.
            //
            // Wave intentionally treats workspace app-activation notifications as a
            // lock/login hint channel only. The authoritative global activation
            // stream comes from the Carbon process actor (`K_EVENT_APP_FRONT_SWITCHED`),
            // which still drives the reactor's activation-time visible-window refresh
            // and workspace-switch behavior. Re-emitting activation here would create
            // duplicate front-app events; the only notification-center-specific job is
            // to notice loginwindow transitions that Carbon does not model as a
            // session-lock boundary.
            let bundle_id = app.bundle_id().as_deref().map(ToString::to_string);
            if bundle_id.as_deref() == Some("com.apple.loginwindow") {
                // loginwindow activation to be a more reliable lock
                // boundary than the distributed lock notification alone. Route it
                // through the same session event stream so the spaces actor
                // buffers topology while macOS swaps in the lock-screen spaces.
                self.enter_session_inactive();
            } else if self.should_leave_session_inactive_for_activation(pid, bundle_id.as_deref()) {
                self.leave_session_inactive();
            }
        }
    }

    fn should_leave_session_inactive_for_activation(
        &self,
        activated_pid: i32,
        activated_bundle_id: Option<&str>,
    ) -> bool {
        let frontmost = NSWorkspace::sharedWorkspace().frontmostApplication();
        let frontmost_pid = frontmost.as_ref().map(|app| app.pid());
        let frontmost_bundle_id = frontmost
            .as_ref()
            .and_then(|app| app.bundle_id().as_deref().map(ToString::to_string));

        should_leave_session_inactive_after_non_login_activation(
            self.ivars().session_inactive_hint.get(),
            activated_pid,
            activated_bundle_id,
            frontmost_pid,
            frontmost_bundle_id.as_deref(),
        )
    }

    fn send_event(&self, event: WmEvent) { _ = self.ivars().events_tx.send(event); }

    fn send_space_event(&self, event: spaces::Event) { self.ivars().spaces_tx.send(event); }

    fn running_application(
        &self,
        notif: &NSNotification,
    ) -> Option<Retained<NSRunningApplication>> {
        let info = notif.userInfo();
        let Some(info) = info else {
            warn!("Got app notification without user info: {notif:?}");
            return None;
        };
        // SAFETY: NSWorkspaceApplicationKey is an immutable framework constant, and the
        // result is only used after a checked downcast.
        let app = unsafe { info.valueForKey(NSWorkspaceApplicationKey) };
        let Some(app) = app else {
            warn!("Got app notification without app object: {notif:?}");
            return None;
        };
        app.downcast::<NSRunningApplication>()
            .inspect_err(|app| {
                warn!("App notification object is not an NSRunningApplication: {app:?}")
            })
            .ok()
    }

    fn handle_dock_pref_changed(&self) {
        trace!("Dock preferences changed; scheduling refresh");
        self.send_space_event(spaces::Event::ScreenRefreshRequested);
    }

    fn handle_menu_bar_pref_changed(&self) {
        trace!("Menu bar autohide changed; scheduling refresh");
        self.send_space_event(spaces::Event::ScreenRefreshRequested);
    }

    unsafe extern "C" fn display_reconfig_callback(
        display_id: u32,
        flags: u32,
        user_info: *mut c_void,
    ) {
        if user_info.is_null() {
            return;
        }
        // SAFETY: The registered handler is alive during this callback. Only clone its
        // immutable thread-safe sender; no native object or Cell crosses queues.
        let tx = unsafe { &*user_info.cast::<NotificationCenterInner>() }
            .ivars()
            .spaces_tx
            .clone();
        let parsed = DisplayReconfigFlags::from_bits_truncate(flags);
        queue::main().after_f_s(Time::NOW, (tx, display_id, parsed), |(tx, display_id, flags)| {
            tx.send(spaces::Event::DisplayReconfigured { display_id, flags });
        });
    }
}

fn should_leave_session_inactive_after_non_login_activation(
    session_inactive_hint: bool,
    activated_pid: i32,
    activated_bundle_id: Option<&str>,
    frontmost_pid: Option<i32>,
    frontmost_bundle_id: Option<&str>,
) -> bool {
    session_inactive_hint
        && activated_bundle_id != Some("com.apple.loginwindow")
        && frontmost_bundle_id != Some("com.apple.loginwindow")
        && frontmost_pid == Some(activated_pid)
}

pub struct NotificationCenter {
    inner: Retained<NotificationCenterInner>,
}

impl NotificationCenter {
    pub fn new(events_tx: wm_controller::Sender, spaces_tx: spaces::Sender) -> Self {
        let handler = NotificationCenterInner::new(events_tx.clone(), spaces_tx);

        let workspace = &NSWorkspace::sharedWorkspace();
        let workspace_object: &AnyObject = workspace;
        let workspace_center = &workspace.notificationCenter();
        let default_center = &NSNotificationCenter::defaultCenter();
        let distributed_center = &NSDistributedNotificationCenter::defaultCenter();
        let observe = |center: &NSNotificationCenter,
                       selector: Sel,
                       name: &NSString,
                       object: Option<&AnyObject>| {
            // SAFETY: Every selector passed here names a method above that takes
            // `&NSNotification`.
            unsafe {
                center.addObserver_selector_name_object(&handler, selector, Some(name), object)
            };
        };

        let active_display = NSString::from_str("NSWorkspaceActiveDisplayDidChangeNotification");
        let workspace_events: [(Sel, &NSString); 8] = [
            (sel!(recvScreenChangedEvent:), &active_display),
            // SAFETY: An immutable AppKit constant.
            (sel!(recvScreenChangedEvent:), unsafe {
                NSWorkspaceActiveSpaceDidChangeNotification
            }),
            // SAFETY: An immutable AppKit constant.
            (sel!(recvWakeEvent:), unsafe { NSWorkspaceDidWakeNotification }),
            // SAFETY: An immutable AppKit constant.
            (sel!(recvSleepEvent:), unsafe {
                NSWorkspaceWillSleepNotification
            }),
            // SAFETY: An immutable AppKit constant.
            (sel!(recvAppEvent:), unsafe {
                NSWorkspaceDidActivateApplicationNotification
            }),
            // SAFETY: An immutable AppKit constant.
            (sel!(recvAppEvent:), unsafe {
                NSWorkspaceDidDeactivateApplicationNotification
            }),
            // SAFETY: An immutable AppKit constant.
            (sel!(recvSessionEvent:), unsafe {
                NSWorkspaceSessionDidResignActiveNotification
            }),
            // SAFETY: An immutable AppKit constant.
            (sel!(recvSessionEvent:), unsafe {
                NSWorkspaceSessionDidBecomeActiveNotification
            }),
        ];
        for (selector, name) in workspace_events {
            observe(workspace_center, selector, name, Some(workspace_object));
        }

        let dock_pref = NSString::from_str("com.apple.dock.prefchanged");
        let menu_bar_pref = NSString::from_str("AppleInterfaceMenuBarHidingChangedNotification");
        let power_state = NSString::from_str("NSProcessInfoPowerStateDidChangeNotification");
        let default_events: [(Sel, &NSString); 4] = [
            // SAFETY: An immutable AppKit constant.
            (sel!(recvScreenChangedEvent:), unsafe {
                NSApplicationDidChangeScreenParametersNotification
            }),
            (sel!(recvDockPrefChanged:), &dock_pref),
            (sel!(recvMenuBarPrefChanged:), &menu_bar_pref),
            (sel!(recvPowerEvent:), &power_state),
        ];
        for (selector, name) in default_events {
            observe(default_center, selector, name, None);
        }

        let distributed_events = [
            (
                sel!(recvKeyboardLayoutChanged:),
                "com.apple.Carbon.TISNotifySelectedKeyboardInputSourceChanged",
            ),
            (sel!(recvSessionEvent:), "com.apple.screenIsLocked"),
            (sel!(recvSessionEvent:), "com.apple.screenIsUnlocked"),
        ];
        for (selector, name) in distributed_events {
            // SAFETY: Each selector names a method above that takes `&NSNotification`.
            unsafe {
                distributed_center.addObserver_selector_name_object_suspensionBehavior(
                    &handler,
                    selector,
                    Some(&NSString::from_str(name)),
                    None,
                    NSNotificationSuspensionBehavior::DeliverImmediately,
                )
            };
        }

        init_power_state();

        NotificationCenter { inner: handler }
    }

    pub async fn watch_for_notifications(self) {
        let workspace = &NSWorkspace::sharedWorkspace();

        self.inner.send_space_event(spaces::Event::ScreenRefreshRequested);
        self.inner.send_event(WmEvent::AppEventsRegistered);
        if let Some(app) = workspace.frontmostApplication() {
            if app.bundle_id().as_deref().map(ToString::to_string).as_deref()
                == Some("com.apple.loginwindow")
            {
                self.inner.enter_session_inactive();
            }
            self.inner.send_event(WmEvent::AppGloballyActivated(app.pid()));
        }

        future::pending().await
    }
}

#[cfg(test)]
mod tests {
    use super::should_leave_session_inactive_after_non_login_activation;

    #[test]
    fn unlock_fallback_requires_session_to_be_marked_inactive() {
        assert!(!should_leave_session_inactive_after_non_login_activation(
            false,
            42,
            Some("com.example.app"),
            Some(42),
            Some("com.example.app"),
        ));
    }

    #[test]
    fn unlock_fallback_rejects_loginwindow_or_non_frontmost_activations() {
        assert!(!should_leave_session_inactive_after_non_login_activation(
            true,
            42,
            Some("com.example.app"),
            Some(7),
            Some("com.example.app"),
        ));
        assert!(!should_leave_session_inactive_after_non_login_activation(
            true,
            42,
            Some("com.example.app"),
            Some(42),
            Some("com.apple.loginwindow"),
        ));
        assert!(!should_leave_session_inactive_after_non_login_activation(
            true,
            42,
            Some("com.apple.loginwindow"),
            Some(42),
            Some("com.apple.loginwindow"),
        ));
    }

    #[test]
    fn unlock_fallback_accepts_matching_frontmost_non_login_activation() {
        assert!(should_leave_session_inactive_after_non_login_activation(
            true,
            42,
            Some("com.example.app"),
            Some(42),
            Some("com.example.app"),
        ));
    }
}
