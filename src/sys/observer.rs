use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::ptr::{self, NonNull};
use std::sync::Arc;

use dispatchr::queue;
use dispatchr::time::Time;
use objc2_application_services::{AXError, AXObserver, AXUIElement as RawAXUIElement};
use objc2_core_foundation::{
    CFRetained, CFRunLoop, CFRunLoopMode, CFString, kCFRunLoopCommonModes,
};

use crate::sys::app::pid_t;
use crate::sys::axuielement::{AXUIElement, Error as AxError};
use crate::sys::dispatch::DispatchExt;

/// An observer for accessibility events.
pub struct Observer {
    callback: Callback,
    observer: CFRetained<AXObserver>,
    subscription_ctx: RefCell<HashMap<NotificationKey, Arc<SubscriptionContext>>>,
}

static_assertions::assert_not_impl_any!(Observer: Send);

/// Helper type for building an [`Observer`].
pub struct ObserverBuilder<F>(CFRetained<AXObserver>, PhantomData<F>);

type NotificationKey = (NonNull<RawAXUIElement>, &'static str);

struct SubscriptionContext {
    callback: Callback,
    data: Cell<usize>,
    notification: &'static str,
}

enum Callback {
    Data(Arc<dyn Fn(AXUIElement, usize)>),
    Notification(Arc<dyn Fn(AXUIElement, &'static str)>),
}

impl Observer {
    /// Creates a new observer for an app, given its `pid`.
    ///
    /// Note that you must call [`ObserverBuilder::install`] on the result of
    /// this function and supply a callback for the observer to have any effect.
    pub fn new<F: Fn(AXUIElement, usize) + 'static>(
        pid: pid_t,
    ) -> Result<ObserverBuilder<F>, AxError> {
        let mut observer_ptr: *mut AXObserver = ptr::null_mut();
        // SAFETY: `internal_callback` matches the AXObserver callback ABI, and `observer_ptr` is a
        // live local the call writes.
        let status = unsafe {
            AXObserver::create(pid, Some(internal_callback), NonNull::from(&mut observer_ptr))
        };
        make_result(status)?;
        let observer = NonNull::new(observer_ptr)
            .ok_or(AxError::Ax(objc2_application_services::AXError::Failure))?;
        // SAFETY: AXObserverCreate follows the Create rule; the caller owns the observer.
        let observer = unsafe { CFRetained::from_raw(observer) };
        Ok(ObserverBuilder(observer, PhantomData))
    }

    /// Creates an observer whose callback receives the notification name.
    ///
    /// This is useful for APIs such as Dock's expose notifications where the
    /// notification itself is the event payload rather than an opaque callback
    /// value supplied at registration time.
    pub fn new_with_notification<F: Fn(AXUIElement, &'static str) + 'static>(
        pid: pid_t,
    ) -> Result<ObserverBuilder<F>, AxError> {
        let mut observer_ptr: *mut AXObserver = ptr::null_mut();
        // SAFETY: `internal_callback_with_notification` matches the AXObserver callback ABI, and
        // `observer_ptr` is a live local the call writes.
        let status = unsafe {
            AXObserver::create(
                pid,
                Some(internal_callback_with_notification),
                NonNull::from(&mut observer_ptr),
            )
        };
        make_result(status)?;
        let observer = NonNull::new(observer_ptr)
            .ok_or(AxError::Ax(objc2_application_services::AXError::Failure))?;
        // SAFETY: AXObserverCreate follows the Create rule; the caller owns the observer.
        let observer = unsafe { CFRetained::from_raw(observer) };
        Ok(ObserverBuilder(observer, PhantomData))
    }
}

impl<F: Fn(AXUIElement, usize) + 'static> ObserverBuilder<F> {
    /// Installs the observer with the supplied callback into the current
    /// thread's run loop.
    pub fn install(self, callback: F) -> Observer {
        self.install_inner(Callback::Data(Arc::new(callback)))
    }
}

impl<F: Fn(AXUIElement, &'static str) + 'static> ObserverBuilder<F> {
    /// Installs a callback that receives the registered notification name.
    pub fn install_with_notification(self, callback: F) -> Observer {
        self.install_inner(Callback::Notification(Arc::new(callback)))
    }
}

impl<F> ObserverBuilder<F> {
    fn install_inner(self, callback: Callback) -> Observer {
        // SAFETY: The observer is a valid, retained AXObserver; the source it returns is retained
        // by the observer.
        let run_loop_source = unsafe { self.0.run_loop_source() };
        if let Some(run_loop) = CFRunLoop::current()
            // SAFETY: kCFRunLoopCommonModes is an immutable CoreFoundation constant.
            && let Some(mode) = unsafe { kCFRunLoopCommonModes }
        {
            let mode: &CFRunLoopMode = mode;
            run_loop.add_source(Some(run_loop_source.as_ref()), Some(mode));
        }
        Observer {
            callback,
            observer: self.0,
            subscription_ctx: RefCell::new(HashMap::new()),
        }
    }
}

impl Clone for Callback {
    fn clone(&self) -> Self {
        match self {
            Self::Data(callback) => Self::Data(Arc::clone(callback)),
            Self::Notification(callback) => Self::Notification(Arc::clone(callback)),
        }
    }
}

struct AddNotifRetryCtx {
    observer: CFRetained<AXObserver>,
    elem: AXUIElement,
    notification: &'static str,
    callback_ctx: Arc<SubscriptionContext>,
}

extern "C" fn add_notif_retry(ctx: *mut c_void) {
    if ctx.is_null() {
        return;
    }
    // SAFETY: `ctx` is the box add_notification_inner leaked into after_f, and dispatch calls this
    // once.
    let ctx = unsafe { Box::from_raw(ctx as *mut AddNotifRetryCtx) };
    let notification_cf = CFString::from_static_str(ctx.notification);
    // SAFETY: The box keeps the observer, element and subscription context alive for the call, and
    // the Observer's map keeps that context alive for later callbacks.
    let _ = unsafe {
        ctx.observer.add_notification(
            ctx.elem.as_concrete_TypeRef(),
            notification_cf.as_ref(),
            Arc::as_ptr(&ctx.callback_ctx).cast_mut().cast(),
        )
    };
}

impl Observer {
    pub fn add_notification(
        &self,
        elem: &AXUIElement,
        notification: &'static str,
    ) -> Result<(), AxError> {
        self.add_notification_with_data(elem, notification, 0)
    }

    pub fn add_notification_with_data(
        &self,
        elem: &AXUIElement,
        notification: &'static str,
        data: usize,
    ) -> Result<(), AxError> {
        let callback_ctx = self.subscription_context(elem, notification, data);
        self.add_notification_inner(elem, notification, callback_ctx)
    }

    fn add_notification_inner(
        &self,
        elem: &AXUIElement,
        notification: &'static str,
        callback_ctx: Arc<SubscriptionContext>,
    ) -> Result<(), AxError> {
        let notification_cf = CFString::from_static_str(notification);
        let observer: &AXObserver = &self.observer;
        let callback_data = Arc::as_ptr(&callback_ctx).cast_mut().cast();
        // SAFETY: The element and name live through the call. The Observer's map keeps
        // `callback_ctx` alive while AX may hand `callback_data` back.
        let first = unsafe {
            observer.add_notification(
                elem.as_concrete_TypeRef(),
                notification_cf.as_ref(),
                callback_data,
            )
        };
        if make_result(first).is_ok() {
            return Ok(());
        }
        if first == AXError::CannotComplete {
            let retained_observer = self.observer.clone();
            let ctx = Box::new(AddNotifRetryCtx {
                observer: retained_observer,
                elem: elem.clone(),
                notification,
                callback_ctx,
            });
            // The retained AX objects and subscription context survive the retry;
            // the retry passes the callback pointer without accessing its thread-local data.
            // SAFETY: `add_notif_retry` matches dispatch's function ABI and takes ownership of the
            // leaked box exactly once.
            unsafe {
                queue::main().after_f(
                    Time::NOW.new_after(10_000_000),
                    Box::into_raw(ctx) as *mut c_void,
                    add_notif_retry,
                )
            };
            return Ok(());
        }
        make_result(first)
    }

    fn subscription_context(
        &self,
        elem: &AXUIElement,
        notification: &'static str,
        data: usize,
    ) -> Arc<SubscriptionContext> {
        let key = (elem.raw_ptr(), notification);
        let mut subscription_ctx = self.subscription_ctx.borrow_mut();
        let ctx = subscription_ctx.entry(key).or_insert_with(|| {
            Arc::new(SubscriptionContext {
                callback: self.callback.clone(),
                data: Cell::new(data),
                notification,
            })
        });
        ctx.data.set(data);
        Arc::clone(ctx)
    }

    pub fn remove_notification(
        &self,
        elem: &AXUIElement,
        notification: &'static str,
    ) -> Result<(), AxError> {
        let notification_cf = CFString::from_static_str(notification);
        let observer: &AXObserver = &self.observer;
        // SAFETY: The element and notification name live through the call.
        let result = make_result(unsafe {
            observer.remove_notification(elem.as_concrete_TypeRef(), notification_cf.as_ref())
        });
        // AXObserverRemoveNotification can succeed while a notification
        // carrying this refcon is already queued on the run loop. Keep the
        // context alive until the observer is dropped instead of freeing
        // memory that CoreFoundation may still pass back to us.
        result
    }
}

unsafe extern "C-unwind" fn internal_callback(
    _observer: NonNull<AXObserver>,
    elem: NonNull<RawAXUIElement>,
    _notif: NonNull<CFString>,
    data: *mut c_void,
) {
    // SAFETY: A non-null `data` is a SubscriptionContext kept alive by the Observer's map until the
    // observer drops.
    let Some(ctx) = (unsafe { data.cast::<SubscriptionContext>().as_ref() }) else {
        return;
    };
    // SAFETY: AX passes a valid element it owns for the callback; from_get_rule retains our own
    // reference.
    let elem = unsafe { AXUIElement::from_get_rule(elem.as_ptr()) };
    if let Callback::Data(callback) = &ctx.callback {
        callback(elem, ctx.data.get());
    }
}

unsafe extern "C-unwind" fn internal_callback_with_notification(
    _observer: NonNull<AXObserver>,
    elem: NonNull<RawAXUIElement>,
    _notif: NonNull<CFString>,
    data: *mut c_void,
) {
    // SAFETY: A non-null `data` is a SubscriptionContext kept alive by the Observer's map until the
    // observer drops.
    let Some(ctx) = (unsafe { data.cast::<SubscriptionContext>().as_ref() }) else {
        return;
    };
    // SAFETY: AX passes a valid element it owns for the callback; from_get_rule retains our own
    // reference.
    let elem = unsafe { AXUIElement::from_get_rule(elem.as_ptr()) };
    if let Callback::Notification(callback) = &ctx.callback {
        callback(elem, ctx.notification);
    }
}

fn make_result(err: AXError) -> Result<(), AxError> {
    if err == AXError::Success {
        Ok(())
    } else {
        Err(AxError::Ax(err))
    }
}
