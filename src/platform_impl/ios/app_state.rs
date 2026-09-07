#![deny(unused_results)]

use std::cell::{RefCell, RefMut};
use std::collections::{HashSet, VecDeque};
use std::os::raw::c_void;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use std::{fmt, mem, ptr};

use core_foundation::base::CFRelease;
use core_foundation::date::CFAbsoluteTimeGetCurrent;
use core_foundation::runloop::{
    kCFRunLoopCommonModes, CFRunLoopAddTimer, CFRunLoopGetMain, CFRunLoopRef, CFRunLoopTimerCreate,
    CFRunLoopTimerInvalidate, CFRunLoopTimerRef, CFRunLoopTimerSetNextFireDate,
};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, sel};
use objc2_foundation::{
    CGRect, CGSize, MainThreadMarker, NSInteger, NSObjectProtocol, NSOperatingSystemVersion,
    NSProcessInfo,
};
use objc2_ui_kit::{UIApplication, UICoordinateSpace, UIView, UIWindow};

use super::window::WinitUIWindow;
use crate::dpi::PhysicalSize;
use crate::event::{Event, InnerSizeWriter, StartCause, WindowEvent};
use crate::event_loop::{ActiveEventLoop as RootActiveEventLoop, ControlFlow};
use crate::platform_impl::callback_state::{CallbackAction, CallbackState};
use crate::window::WindowId as RootWindowId;

macro_rules! bug {
    ($($msg:tt)*) => {
        panic!("winit iOS bug, file an issue: {}", format!($($msg)*))
    };
}

macro_rules! bug_assert {
    ($test:expr, $($msg:tt)*) => {
        assert!($test, "winit iOS bug, file an issue: {}", format!($($msg)*))
    };
}

#[derive(Debug)]
pub(crate) struct HandlePendingUserEvents;

pub(crate) struct EventLoopHandler {
    #[allow(clippy::type_complexity)]
    pub(crate) handler: Box<dyn FnMut(Event<HandlePendingUserEvents>, &RootActiveEventLoop)>,
    pub(crate) event_loop: RootActiveEventLoop,
}

impl fmt::Debug for EventLoopHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventLoopHandler")
            .field("handler", &"...")
            .field("event_loop", &self.event_loop)
            .finish()
    }
}

impl EventLoopHandler {
    fn handle_event(&mut self, event: Event<HandlePendingUserEvents>) {
        (self.handler)(event, &self.event_loop)
    }
}

#[derive(Debug)]
pub(crate) enum EventWrapper {
    StaticEvent(Event<HandlePendingUserEvents>),
    ScaleFactorChanged(ScaleFactorChanged),
}

#[derive(Debug)]
pub struct ScaleFactorChanged {
    pub(super) window: Retained<WinitUIWindow>,
    pub(super) suggested_size: PhysicalSize<u32>,
    pub(super) scale_factor: f64,
}

enum UserCallbackTransitionResult {
    Success { handler: EventLoopHandler, processing_redraws: bool },
    ReentrancyPrevented,
}

enum TerminationTransition {
    Immediate { handler: EventLoopHandler, events: Vec<EventWrapper> },
    Deferred,
    AlreadyTerminated,
}

impl Event<HandlePendingUserEvents> {
    fn is_redraw(&self) -> bool {
        matches!(self, Event::WindowEvent { event: WindowEvent::RedrawRequested, .. })
    }

    fn is_expected_after_main_events(&self) -> bool {
        self.is_redraw() || matches!(self, Event::AboutToWait)
    }
}

// this is the state machine for the app lifecycle
#[derive(Debug)]
#[must_use = "dropping `AppStateImpl` without inspecting it is probably a bug"]
enum AppStateImpl {
    NotLaunched {
        queued_windows: Vec<Retained<WinitUIWindow>>,
        queued_events: Vec<EventWrapper>,
    },
    Launching {
        queued_windows: Vec<Retained<WinitUIWindow>>,
        queued_events: Vec<EventWrapper>,
        queued_handler: EventLoopHandler,
    },
    ProcessingEvents {
        handler: EventLoopHandler,
    },
    // special state to deal with reentrancy and prevent mutable aliasing.
    InUserCallback {
        callback: CallbackState<EventWrapper>,
    },
    ProcessingRedraws {
        handler: EventLoopHandler,
    },
    Waiting {
        waiting_handler: EventLoopHandler,
        start: Instant,
    },
    PollFinished {
        waiting_handler: EventLoopHandler,
    },
    Terminated,
}

pub(crate) struct AppState {
    // This should never be `None`, except for briefly during a state transition.
    app_state: Option<AppStateImpl>,
    application_active: bool,
    control_flow: ControlFlow,
    queued_gpu_redraws: HashSet<Retained<WinitUIWindow>>,
    waker: EventLoopWaker,
}

impl AppState {
    pub(crate) fn get_mut(_mtm: MainThreadMarker) -> RefMut<'static, AppState> {
        // basically everything in UIKit requires the main thread, so it's pointless to use the
        // std::sync APIs.
        // must be mut because plain `static` requires `Sync`
        static mut APP_STATE: RefCell<Option<AppState>> = RefCell::new(None);

        #[allow(unknown_lints)] // New lint below
        #[allow(static_mut_refs)] // TODO: Use `MainThreadBound` instead.
        let mut guard = unsafe { APP_STATE.borrow_mut() };
        if guard.is_none() {
            #[inline(never)]
            #[cold]
            fn init_guard(guard: &mut RefMut<'static, Option<AppState>>) {
                let waker = EventLoopWaker::new(unsafe { CFRunLoopGetMain() });
                **guard = Some(AppState {
                    app_state: Some(AppStateImpl::NotLaunched {
                        queued_windows: Vec::new(),
                        queued_events: Vec::new(),
                    }),
                    application_active: false,
                    control_flow: ControlFlow::default(),
                    queued_gpu_redraws: HashSet::new(),
                    waker,
                });
            }
            init_guard(&mut guard);
        }
        RefMut::map(guard, |state| state.as_mut().unwrap())
    }

    fn state(&self) -> &AppStateImpl {
        match &self.app_state {
            Some(ref state) => state,
            None => bug!("`AppState` previously failed a state transition"),
        }
    }

    fn state_mut(&mut self) -> &mut AppStateImpl {
        match &mut self.app_state {
            Some(ref mut state) => state,
            None => bug!("`AppState` previously failed a state transition"),
        }
    }

    fn take_state(&mut self) -> AppStateImpl {
        match self.app_state.take() {
            Some(state) => state,
            None => bug!("`AppState` previously failed a state transition"),
        }
    }

    fn set_state(&mut self, new_state: AppStateImpl) {
        bug_assert!(
            self.app_state.is_none(),
            "attempted to set an `AppState` without calling `take_state` first {:?}",
            self.app_state
        );
        self.app_state = Some(new_state)
    }

    fn replace_state(&mut self, new_state: AppStateImpl) -> AppStateImpl {
        match &mut self.app_state {
            Some(ref mut state) => mem::replace(state, new_state),
            None => bug!("`AppState` previously failed a state transition"),
        }
    }

    fn has_launched(&self) -> bool {
        !matches!(self.state(), AppStateImpl::NotLaunched { .. } | AppStateImpl::Launching { .. })
    }

    fn has_terminated(&self) -> bool {
        matches!(self.state(), AppStateImpl::Terminated)
    }

    pub(crate) fn is_terminating(&self) -> bool {
        match self.state() {
            AppStateImpl::InUserCallback { callback } => callback.is_terminating(),
            AppStateImpl::Terminated => true,
            _ => false,
        }
    }

    fn control_flow_observers_suppressed(&self) -> bool {
        // Common-mode observers also run in nested UIKit loops. Do not re-enter Winit while the
        // outer loop is dispatching an application callback.
        matches!(self.state(), AppStateImpl::InUserCallback { .. })
    }

    fn will_launch_transition(&mut self, queued_handler: EventLoopHandler) {
        let (queued_windows, queued_events) = match self.take_state() {
            AppStateImpl::NotLaunched { queued_windows, queued_events } => {
                (queued_windows, queued_events)
            },
            s => bug!("unexpected state {:?}", s),
        };
        self.set_state(AppStateImpl::Launching { queued_windows, queued_events, queued_handler });
    }

    fn did_finish_launching_transition(
        &mut self,
    ) -> (Vec<Retained<WinitUIWindow>>, Vec<EventWrapper>) {
        let (windows, events, handler) = match self.take_state() {
            AppStateImpl::Launching { queued_windows, queued_events, queued_handler } => {
                (queued_windows, queued_events, queued_handler)
            },
            s => bug!("unexpected state {:?}", s),
        };
        self.set_state(AppStateImpl::ProcessingEvents { handler });
        (windows, events)
    }

    fn wakeup_transition(&mut self) -> Option<EventWrapper> {
        // before `AppState::did_finish_launching` is called, pretend there is no running
        // event loop.
        if !self.has_launched() || self.has_terminated() {
            return None;
        }

        let (handler, event) = match (self.control_flow, self.take_state()) {
            (ControlFlow::Poll, AppStateImpl::PollFinished { waiting_handler }) => {
                (waiting_handler, EventWrapper::StaticEvent(Event::NewEvents(StartCause::Poll)))
            },
            (ControlFlow::Wait, AppStateImpl::Waiting { waiting_handler, start }) => (
                waiting_handler,
                EventWrapper::StaticEvent(Event::NewEvents(StartCause::WaitCancelled {
                    start,
                    requested_resume: None,
                })),
            ),
            (
                ControlFlow::WaitUntil(requested_resume),
                AppStateImpl::Waiting { waiting_handler, start },
            ) => {
                let event = if Instant::now() >= requested_resume {
                    EventWrapper::StaticEvent(Event::NewEvents(StartCause::ResumeTimeReached {
                        start,
                        requested_resume,
                    }))
                } else {
                    EventWrapper::StaticEvent(Event::NewEvents(StartCause::WaitCancelled {
                        start,
                        requested_resume: Some(requested_resume),
                    }))
                };
                (waiting_handler, event)
            },
            s => bug!("`EventHandler` unexpectedly woke up {:?}", s),
        };

        self.set_state(AppStateImpl::ProcessingEvents { handler });
        Some(event)
    }

    fn try_user_callback_transition(&mut self) -> UserCallbackTransitionResult {
        // If we're not able to process an event due to recursion or `Init` not having been sent out
        // yet, then queue the events up.
        match self.state() {
            AppStateImpl::Launching { .. }
            | AppStateImpl::NotLaunched { .. }
            | AppStateImpl::InUserCallback { .. } => {
                return UserCallbackTransitionResult::ReentrancyPrevented;
            },

            AppStateImpl::ProcessingEvents { .. } | AppStateImpl::ProcessingRedraws { .. } => {},

            s @ AppStateImpl::PollFinished { .. }
            | s @ AppStateImpl::Waiting { .. }
            | s @ AppStateImpl::Terminated => {
                bug!("unexpected attempted to process an event {:?}", s)
            },
        }

        let (handler, processing_redraws) = match self.take_state() {
            AppStateImpl::Launching { .. }
            | AppStateImpl::NotLaunched { .. }
            | AppStateImpl::InUserCallback { .. } => unreachable!(),
            AppStateImpl::ProcessingEvents { handler } => (handler, false),
            AppStateImpl::ProcessingRedraws { handler } => (handler, true),
            AppStateImpl::PollFinished { .. }
            | AppStateImpl::Waiting { .. }
            | AppStateImpl::Terminated => unreachable!(),
        };
        self.set_state(AppStateImpl::InUserCallback { callback: CallbackState::new() });
        UserCallbackTransitionResult::Success { handler, processing_redraws }
    }

    fn queue_callback_events(&mut self, events: impl IntoIterator<Item = EventWrapper>) {
        match self.state_mut() {
            AppStateImpl::Launching { queued_events, .. }
            | AppStateImpl::NotLaunched { queued_events, .. } => queued_events.extend(events),
            AppStateImpl::InUserCallback { callback } => callback.queue(events),
            s => bug!("unexpected attempted to queue events {:?}", s),
        }
    }

    fn main_events_cleared_transition(&mut self) -> HashSet<Retained<WinitUIWindow>> {
        let handler = match self.take_state() {
            AppStateImpl::ProcessingEvents { handler } => handler,
            s => bug!("unexpected state {:?}", s),
        };
        self.set_state(AppStateImpl::ProcessingRedraws { handler });
        mem::take(&mut self.queued_gpu_redraws)
    }

    fn events_cleared_transition(&mut self) {
        if !self.has_launched() || self.has_terminated() {
            return;
        }
        let waiting_handler = match self.take_state() {
            AppStateImpl::ProcessingRedraws { handler } => handler,
            s => bug!("unexpected state {:?}", s),
        };

        let state = match self.control_flow {
            ControlFlow::Wait | ControlFlow::WaitUntil(_) => {
                AppStateImpl::Waiting { waiting_handler, start: Instant::now() }
            },
            // Unlike on macOS, handle Poll to Poll transition here to call the waker
            ControlFlow::Poll => AppStateImpl::PollFinished { waiting_handler },
        };
        self.set_state(state);
        self.update_waker();
    }

    fn terminated_transition(&mut self, events: Vec<EventWrapper>) -> TerminationTransition {
        match self.state_mut() {
            AppStateImpl::InUserCallback { callback } => {
                if callback.request_termination(events) {
                    self.queued_gpu_redraws.clear();
                    self.waker.stop();
                    return TerminationTransition::Deferred;
                }
                return TerminationTransition::AlreadyTerminated;
            },
            AppStateImpl::Terminated => return TerminationTransition::AlreadyTerminated,
            AppStateImpl::NotLaunched { .. } => {
                bug!("`LoopExiting` happened before the event loop launched")
            },
            _ => {},
        }

        let handler = match self.replace_state(AppStateImpl::Terminated) {
            AppStateImpl::Launching { queued_handler, .. } => queued_handler,
            AppStateImpl::ProcessingEvents { handler }
            | AppStateImpl::ProcessingRedraws { handler } => handler,
            AppStateImpl::Waiting { waiting_handler, .. }
            | AppStateImpl::PollFinished { waiting_handler } => waiting_handler,
            AppStateImpl::NotLaunched { .. }
            | AppStateImpl::InUserCallback { .. }
            | AppStateImpl::Terminated => unreachable!(),
        };
        self.queued_gpu_redraws.clear();
        self.waker.stop();
        TerminationTransition::Immediate { handler, events }
    }

    pub(crate) fn set_control_flow(&mut self, control_flow: ControlFlow) {
        self.control_flow = control_flow;
    }

    fn update_waker(&mut self) {
        if self.is_terminating() {
            self.waker.stop();
            return;
        }

        match (self.application_active, self.queued_gpu_redraws.is_empty(), self.control_flow) {
            (false, ..) | (true, true, ControlFlow::Wait) => self.waker.stop(),
            (true, false, _) | (true, true, ControlFlow::Poll) => self.waker.start(),
            (true, true, ControlFlow::WaitUntil(instant)) => self.waker.start_at(instant),
        }
    }

    fn set_application_active(&mut self, active: bool) {
        self.application_active = active;
        self.update_waker();
    }

    pub(crate) fn control_flow(&self) -> ControlFlow {
        self.control_flow
    }
}

pub(crate) fn set_key_window(mtm: MainThreadMarker, window: &Retained<WinitUIWindow>) {
    let mut this = AppState::get_mut(mtm);
    if this.is_terminating() {
        panic!("Attempt to create a `Window` after the app has terminated");
    }
    match this.state_mut() {
        &mut AppStateImpl::NotLaunched { ref mut queued_windows, .. } => {
            return queued_windows.push(window.clone())
        },
        &mut AppStateImpl::ProcessingEvents { .. }
        | &mut AppStateImpl::InUserCallback { .. }
        | &mut AppStateImpl::ProcessingRedraws { .. } => {},
        s @ &mut AppStateImpl::Launching { .. }
        | s @ &mut AppStateImpl::Waiting { .. }
        | s @ &mut AppStateImpl::PollFinished { .. } => bug!("unexpected state {:?}", s),
        &mut AppStateImpl::Terminated => {
            panic!("Attempt to create a `Window` after the app has terminated")
        },
    }
    drop(this);
    window.makeKeyAndVisible();
}

pub(crate) fn queue_gl_or_metal_redraw(mtm: MainThreadMarker, window: Retained<WinitUIWindow>) {
    let mut this = AppState::get_mut(mtm);
    if this.is_terminating() {
        tracing::trace!("ignoring redraw request while the app is terminating");
        return;
    }
    let _ = this.queued_gpu_redraws.insert(window);
    this.update_waker();
}

pub(crate) fn will_launch(mtm: MainThreadMarker, queued_handler: EventLoopHandler) {
    AppState::get_mut(mtm).will_launch_transition(queued_handler)
}

pub fn did_finish_launching(mtm: MainThreadMarker) {
    let mut this = AppState::get_mut(mtm);
    if this.is_terminating() {
        return;
    }
    let windows = match this.state_mut() {
        AppStateImpl::Launching { queued_windows, .. } => mem::take(queued_windows),
        s => bug!("unexpected state {:?}", s),
    };

    this.update_waker();

    // have to drop RefMut because the window setup code below can trigger new events
    drop(this);

    for window in windows {
        if AppState::get_mut(mtm).is_terminating() {
            return;
        }

        // Do a little screen dance here to account for windows being created before
        // `UIApplicationMain` is called. This fixes visual issues such as being
        // offcenter and sized incorrectly. Additionally, to fix orientation issues, we
        // gotta reset the `rootViewController`.
        //
        // relevant iOS log:
        // ```
        // [ApplicationLifecycle] Windows were created before application initialization
        // completed. This may result in incorrect visual appearance.
        // ```
        let screen = window.screen();
        let _: () = unsafe { msg_send![&window, setScreen: ptr::null::<AnyObject>()] };
        window.setScreen(&screen);

        let controller = window.rootViewController();
        window.setRootViewController(None);
        window.setRootViewController(controller.as_deref());

        window.makeKeyAndVisible();
    }

    let mut this = AppState::get_mut(mtm);
    if this.is_terminating() {
        return;
    }
    let (windows, events) = this.did_finish_launching_transition();
    drop(this);

    let events = std::iter::once(EventWrapper::StaticEvent(Event::NewEvents(StartCause::Init)))
        .chain(events);
    handle_nonuser_events(mtm, events);

    if AppState::get_mut(mtm).is_terminating() {
        return;
    }

    // the above window dance hack, could possibly trigger new windows to be created.
    // we can just set those windows up normally, as they were created after didFinishLaunching
    for window in windows {
        if AppState::get_mut(mtm).is_terminating() {
            return;
        }
        window.makeKeyAndVisible();
    }
}

pub(crate) fn did_become_active(mtm: MainThreadMarker) {
    AppState::get_mut(mtm).set_application_active(true);
    handle_nonuser_event(mtm, EventWrapper::StaticEvent(Event::Resumed));
}

pub(crate) fn will_resign_active(mtm: MainThreadMarker) {
    let mut this = AppState::get_mut(mtm);
    this.set_application_active(false);
    drop(this);

    handle_nonuser_event(mtm, EventWrapper::StaticEvent(Event::Suspended));
}

// AppState::did_finish_launching handles the special transition `Init`
pub fn handle_wakeup_transition(mtm: MainThreadMarker) {
    let mut this = AppState::get_mut(mtm);
    if this.control_flow_observers_suppressed() {
        return;
    }
    let wakeup_event = match this.wakeup_transition() {
        None => return,
        Some(wakeup_event) => wakeup_event,
    };
    drop(this);

    handle_nonuser_event(mtm, wakeup_event)
}

pub(crate) fn handle_nonuser_event(mtm: MainThreadMarker, event: EventWrapper) {
    handle_nonuser_events(mtm, std::iter::once(event))
}

pub(crate) fn handle_nonuser_events<I: IntoIterator<Item = EventWrapper>>(
    mtm: MainThreadMarker,
    events: I,
) {
    let mut this = AppState::get_mut(mtm);
    if this.is_terminating() {
        return;
    }

    let (mut handler, processing_redraws) = match this.try_user_callback_transition() {
        UserCallbackTransitionResult::ReentrancyPrevented => {
            this.queue_callback_events(events);
            return;
        },
        UserCallbackTransitionResult::Success { handler, processing_redraws } => {
            (handler, processing_redraws)
        },
    };
    drop(this);

    let mut queued_events = VecDeque::new();
    for wrapper in events {
        handle_event_wrapper(&mut handler, wrapper, processing_redraws);
        match take_callback_action(mtm, &mut queued_events) {
            CallbackAction::Continue => {},
            CallbackAction::Terminate(events) => {
                return finish_callback_termination(mtm, handler, events);
            },
        }
    }

    while let Some(wrapper) = queued_events.pop_front() {
        handle_event_wrapper(&mut handler, wrapper, processing_redraws);
        match take_callback_action(mtm, &mut queued_events) {
            CallbackAction::Continue => {},
            CallbackAction::Terminate(events) => {
                return finish_callback_termination(mtm, handler, events);
            },
        }
    }

    restore_callback_handler(mtm, handler, processing_redraws);
}

fn handle_user_events(mtm: MainThreadMarker) {
    let mut this = AppState::get_mut(mtm);
    let (mut handler, processing_redraws) = match this.try_user_callback_transition() {
        UserCallbackTransitionResult::ReentrancyPrevented => {
            bug!("unexpected attempted to process an event")
        },
        UserCallbackTransitionResult::Success { handler, processing_redraws } => {
            (handler, processing_redraws)
        },
    };
    if processing_redraws {
        bug!("user events attempted to be sent out while `ProcessingRedraws`");
    }
    drop(this);

    let mut queued_events = VecDeque::new();
    handler.handle_event(Event::UserEvent(HandlePendingUserEvents));
    match take_callback_action(mtm, &mut queued_events) {
        CallbackAction::Continue => {},
        CallbackAction::Terminate(events) => {
            return finish_callback_termination(mtm, handler, events);
        },
    }

    loop {
        if queued_events.is_empty() {
            restore_callback_handler(mtm, handler, false);
            break;
        }

        let batch_len = queued_events.len();
        for _ in 0..batch_len {
            let wrapper = queued_events.pop_front().unwrap();
            handle_event_wrapper(&mut handler, wrapper, false);
            match take_callback_action(mtm, &mut queued_events) {
                CallbackAction::Continue => {},
                CallbackAction::Terminate(events) => {
                    return finish_callback_termination(mtm, handler, events);
                },
            }
        }

        handler.handle_event(Event::UserEvent(HandlePendingUserEvents));
        match take_callback_action(mtm, &mut queued_events) {
            CallbackAction::Continue => {},
            CallbackAction::Terminate(events) => {
                return finish_callback_termination(mtm, handler, events);
            },
        }
    }
}

pub(crate) fn send_occluded_event_for_all_windows(application: &UIApplication, occluded: bool) {
    let mtm = MainThreadMarker::from(application);

    let mut events = Vec::new();
    #[allow(deprecated)]
    for window in application.windows().iter() {
        if window.is_kind_of::<WinitUIWindow>() {
            // SAFETY: We just checked that the window is a `winit` window
            let window = unsafe {
                let ptr: *const UIWindow = window;
                let ptr: *const WinitUIWindow = ptr.cast();
                &*ptr
            };
            events.push(EventWrapper::StaticEvent(Event::WindowEvent {
                window_id: RootWindowId(window.id()),
                event: WindowEvent::Occluded(occluded),
            }));
        }
    }
    handle_nonuser_events(mtm, events);
}

pub fn handle_main_events_cleared(mtm: MainThreadMarker) {
    let mut this = AppState::get_mut(mtm);
    if this.control_flow_observers_suppressed() {
        return;
    }
    if !this.has_launched() || this.has_terminated() {
        return;
    }
    match this.state_mut() {
        AppStateImpl::ProcessingEvents { .. } => {},
        _ => bug!("`ProcessingRedraws` happened unexpectedly"),
    };
    drop(this);

    handle_user_events(mtm);

    let mut this = AppState::get_mut(mtm);
    if this.is_terminating() {
        return;
    }
    let redraw_events: Vec<EventWrapper> = this
        .main_events_cleared_transition()
        .into_iter()
        .map(|window| {
            EventWrapper::StaticEvent(Event::WindowEvent {
                window_id: RootWindowId(window.id()),
                event: WindowEvent::RedrawRequested,
            })
        })
        .collect();
    drop(this);

    handle_nonuser_events(mtm, redraw_events);
    handle_nonuser_event(mtm, EventWrapper::StaticEvent(Event::AboutToWait));
}

pub fn handle_events_cleared(mtm: MainThreadMarker) {
    let mut this = AppState::get_mut(mtm);
    if !this.control_flow_observers_suppressed() {
        this.events_cleared_transition();
    }
}

pub(crate) fn terminated(application: &UIApplication) {
    let mtm = MainThreadMarker::from(application);

    if AppState::get_mut(mtm).is_terminating() {
        return;
    }

    let mut events = Vec::new();
    #[allow(deprecated)]
    for window in application.windows().iter() {
        if window.is_kind_of::<WinitUIWindow>() {
            // SAFETY: We just checked that the window is a `winit` window
            let window = unsafe {
                let ptr: *const UIWindow = window;
                let ptr: *const WinitUIWindow = ptr.cast();
                &*ptr
            };
            events.push(EventWrapper::StaticEvent(Event::WindowEvent {
                window_id: RootWindowId(window.id()),
                event: WindowEvent::Destroyed,
            }));
        }
    }
    let mut this = AppState::get_mut(mtm);
    match this.terminated_transition(events) {
        TerminationTransition::Immediate { handler, events } => {
            drop(this);
            finish_termination(handler, events);
        },
        TerminationTransition::Deferred | TerminationTransition::AlreadyTerminated => {},
    }
}

fn take_callback_action(
    mtm: MainThreadMarker,
    pending_events: &mut VecDeque<EventWrapper>,
) -> CallbackAction<EventWrapper> {
    let mut this = AppState::get_mut(mtm);
    match this.state_mut() {
        AppStateImpl::InUserCallback { callback } => callback.take_action(pending_events),
        s => bug!("unexpected state {:?}", s),
    }
}

fn restore_callback_handler(
    mtm: MainThreadMarker,
    handler: EventLoopHandler,
    processing_redraws: bool,
) {
    let mut this = AppState::get_mut(mtm);
    let AppStateImpl::InUserCallback { .. } = this.take_state() else { unreachable!() };
    this.set_state(if processing_redraws {
        AppStateImpl::ProcessingRedraws { handler }
    } else {
        AppStateImpl::ProcessingEvents { handler }
    });
}

fn finish_callback_termination(
    mtm: MainThreadMarker,
    handler: EventLoopHandler,
    events: Vec<EventWrapper>,
) {
    let mut this = AppState::get_mut(mtm);
    let AppStateImpl::InUserCallback { .. } = this.take_state() else { unreachable!() };
    this.set_state(AppStateImpl::Terminated);
    this.queued_gpu_redraws.clear();
    this.waker.stop();
    drop(this);

    finish_termination(handler, events);
}

fn finish_termination(mut handler: EventLoopHandler, events: Vec<EventWrapper>) {
    for event in events {
        handle_event_wrapper(&mut handler, event, false);
    }
    handler.handle_event(Event::LoopExiting);
}

fn handle_event_wrapper(
    handler: &mut EventLoopHandler,
    wrapper: EventWrapper,
    processing_redraws: bool,
) {
    match wrapper {
        EventWrapper::StaticEvent(event) => {
            if !processing_redraws && event.is_redraw() {
                tracing::info!("processing `RedrawRequested` during the main event loop");
            } else if processing_redraws && !event.is_expected_after_main_events() {
                tracing::warn!(
                    "processing non `RedrawRequested` event after the main event loop: {:#?}",
                    event
                );
            }
            handler.handle_event(event)
        },
        EventWrapper::ScaleFactorChanged(event) => handle_hidpi_proxy(handler, event),
    }
}

fn handle_hidpi_proxy(handler: &mut EventLoopHandler, event: ScaleFactorChanged) {
    let ScaleFactorChanged { suggested_size, scale_factor, window } = event;
    let mtm = MainThreadMarker::from(&*window);
    let new_inner_size = Arc::new(Mutex::new(suggested_size));
    let event = Event::WindowEvent {
        window_id: RootWindowId(window.id()),
        event: WindowEvent::ScaleFactorChanged {
            scale_factor,
            inner_size_writer: InnerSizeWriter::new(Arc::downgrade(&new_inner_size)),
        },
    };
    handler.handle_event(event);
    if AppState::get_mut(mtm).is_terminating() {
        return;
    }
    let (view, screen_frame) = get_view_and_screen_frame(&window);
    let physical_size = *new_inner_size.lock().unwrap();
    drop(new_inner_size);
    let logical_size = physical_size.to_logical(scale_factor);
    let size = CGSize::new(logical_size.width, logical_size.height);
    let new_frame: CGRect = CGRect::new(screen_frame.origin, size);
    view.setFrame(new_frame);
}

fn get_view_and_screen_frame(window: &WinitUIWindow) -> (Retained<UIView>, CGRect) {
    let view_controller = window.rootViewController().unwrap();
    let view = view_controller.view().unwrap();
    let bounds = window.bounds();
    let screen = window.screen();
    let screen_space = screen.coordinateSpace();
    let screen_frame = window.convertRect_toCoordinateSpace(bounds, &screen_space);
    (view, screen_frame)
}

struct EventLoopWaker {
    timer: CFRunLoopTimerRef,
}

impl Drop for EventLoopWaker {
    fn drop(&mut self) {
        unsafe {
            CFRunLoopTimerInvalidate(self.timer);
            CFRelease(self.timer as _);
        }
    }
}

impl EventLoopWaker {
    fn new(rl: CFRunLoopRef) -> EventLoopWaker {
        extern "C" fn wakeup_main_loop(_timer: CFRunLoopTimerRef, _info: *mut c_void) {}
        unsafe {
            // Re-arm the timer after every wake instead of using a tiny interval. Core Foundation
            // catches up repeating timers one interval at a time after a delayed callback.
            let timer = CFRunLoopTimerCreate(
                ptr::null_mut(),
                f64::MAX,
                1_000_000_000.0,
                0,
                0,
                wakeup_main_loop,
                ptr::null_mut(),
            );
            CFRunLoopAddTimer(rl, timer, kCFRunLoopCommonModes);

            EventLoopWaker { timer }
        }
    }

    fn stop(&mut self) {
        unsafe { CFRunLoopTimerSetNextFireDate(self.timer, f64::MAX) }
    }

    fn start(&mut self) {
        unsafe { CFRunLoopTimerSetNextFireDate(self.timer, CFAbsoluteTimeGetCurrent()) }
    }

    fn start_at(&mut self, instant: Instant) {
        let now = Instant::now();
        if now >= instant {
            self.start();
        } else {
            unsafe {
                let current = CFAbsoluteTimeGetCurrent();
                let duration = instant - now;
                let fsecs =
                    duration.subsec_nanos() as f64 / 1_000_000_000.0 + duration.as_secs() as f64;
                CFRunLoopTimerSetNextFireDate(self.timer, current + fsecs)
            }
        }
    }
}

macro_rules! os_capabilities {
    (
        $(
            $(#[$attr:meta])*
            $error_name:ident: $objc_call:literal,
            $name:ident: $major:literal-$minor:literal
        ),*
        $(,)*
    ) => {
        #[derive(Clone, Debug)]
        pub struct OSCapabilities {
            $(
                pub $name: bool,
            )*

            os_version: NSOperatingSystemVersion,
        }

        impl OSCapabilities {
            fn from_os_version(os_version: NSOperatingSystemVersion) -> Self {
                $(let $name = meets_requirements(os_version, $major, $minor);)*
                Self { $($name,)* os_version, }
            }
        }

        impl OSCapabilities {$(
            $(#[$attr])*
            pub fn $error_name(&self, extra_msg: &str) {
                tracing::warn!(
                    concat!("`", $objc_call, "` requires iOS {}.{}+. This device is running iOS {}.{}.{}. {}"),
                    $major, $minor, self.os_version.majorVersion, self.os_version.minorVersion, self.os_version.patchVersion,
                    extra_msg
                )
            }
        )*}
    };
}

os_capabilities! {
    /// <https://developer.apple.com/documentation/uikit/uiview/2891103-safeareainsets?language=objc>
    #[allow(unused)] // error message unused
    safe_area_err_msg: "-[UIView safeAreaInsets]",
    safe_area: 11-0,
    /// <https://developer.apple.com/documentation/uikit/uiviewcontroller/2887509-setneedsupdateofhomeindicatoraut?language=objc>
    home_indicator_hidden_err_msg: "-[UIViewController setNeedsUpdateOfHomeIndicatorAutoHidden]",
    home_indicator_hidden: 11-0,
    /// <https://developer.apple.com/documentation/uikit/uiviewcontroller/2887507-setneedsupdateofscreenedgesdefer?language=objc>
    defer_system_gestures_err_msg: "-[UIViewController setNeedsUpdateOfScreenEdgesDeferringSystem]",
    defer_system_gestures: 11-0,
    /// <https://developer.apple.com/documentation/uikit/uiscreen/2806814-maximumframespersecond?language=objc>
    maximum_frames_per_second_err_msg: "-[UIScreen maximumFramesPerSecond]",
    maximum_frames_per_second: 10-3,
    /// <https://developer.apple.com/documentation/uikit/uitouch/1618110-force?language=objc>
    #[allow(unused)] // error message unused
    force_touch_err_msg: "-[UITouch force]",
    force_touch: 9-0,
}

fn meets_requirements(
    version: NSOperatingSystemVersion,
    required_major: NSInteger,
    required_minor: NSInteger,
) -> bool {
    (version.majorVersion, version.minorVersion) >= (required_major, required_minor)
}

fn get_version() -> NSOperatingSystemVersion {
    let process_info = NSProcessInfo::processInfo();
    let atleast_ios_8 = process_info.respondsToSelector(sel!(operatingSystemVersion));
    // Winit requires atleast iOS 8 because no one has put the time into supporting earlier os
    // versions. Older iOS versions are increasingly difficult to test. For example, Xcode 11 does
    // not support debugging on devices with an iOS version of less than 8. Another example, in
    // order to use an iOS simulator older than iOS 8, you must download an older version of Xcode
    // (<9), and at least Xcode 7 has been tested to not even run on macOS 10.15 - Xcode 8 might?
    //
    // The minimum required iOS version is likely to grow in the future.
    assert!(atleast_ios_8, "`winit` requires iOS version 8 or greater");
    process_info.operatingSystemVersion()
}

pub fn os_capabilities() -> OSCapabilities {
    // Cache the version lookup for efficiency
    static OS_CAPABILITIES: OnceLock<OSCapabilities> = OnceLock::new();
    OS_CAPABILITIES.get_or_init(|| OSCapabilities::from_os_version(get_version())).clone()
}
