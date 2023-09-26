//! Note that this file contains two similar paths - one for [`glow`], one for [`wgpu`].
//! When making changes to one you often also want to apply it to the other.

use std::{sync::Arc, time::Instant};

use egui::{epaint::ahash::HashMap, mutex::RwLock, ViewportBuilder, ViewportId};
use raw_window_handle::{HasRawDisplayHandle as _, HasRawWindowHandle as _};
use winit::event_loop::{
    ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget,
};

#[cfg(feature = "accesskit")]
use egui_winit::accesskit_winit;
use egui_winit::winit;

use crate::{epi, Result};

use super::epi_integration::{self, EpiIntegration};

// ----------------------------------------------------------------------------

pub const IS_DESKTOP: bool = cfg!(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd"
));

// ----------------------------------------------------------------------------

thread_local! {
    /// This makes `Context::create_viewport_sync` to have a native window in the same frame!
    pub static WINIT_EVENT_LOOP: RwLock<*const EventLoopWindowTarget<UserEvent>> = RwLock::new(std::ptr::null());
}

// ----------------------------------------------------------------------------

/// The custom even `eframe` uses with the [`winit`] event loop.
#[derive(Debug)]
pub enum UserEvent {
    /// A repaint is requested.
    RequestRepaint {
        /// What to repaint.
        id: ViewportId,

        /// When to repaint.
        when: Instant,

        /// What the frame number was when the repaint was _requested_.
        frame_nr: u64,
    },

    /// A request related to [`accesskit`](https://accesskit.dev/).
    #[cfg(feature = "accesskit")]
    AccessKitActionRequest(accesskit_winit::ActionRequestEvent),
}

#[cfg(feature = "accesskit")]
impl From<accesskit_winit::ActionRequestEvent> for UserEvent {
    fn from(inner: accesskit_winit::ActionRequestEvent) -> Self {
        Self::AccessKitActionRequest(inner)
    }
}

// ----------------------------------------------------------------------------

pub use epi::NativeOptions;

#[derive(Debug)]
enum EventResult {
    Wait,

    /// Causes a synchronous repaint inside the event handler. This should only
    /// be used in special situations if the window must be repainted while
    /// handling a specific event. This occurs on Windows when handling resizes.
    ///
    /// `RepaintNow` creates a new frame synchronously, and should therefore
    /// only be used for extremely urgent repaints.
    RepaintNow(winit::window::WindowId),

    /// Queues a repaint for once the event loop handles its next redraw. Exists
    /// so that multiple input events can be handled in one frame. Does not
    /// cause any delay like `RepaintNow`.
    RepaintNext(winit::window::WindowId),

    RepaintAt(winit::window::WindowId, Instant),

    Exit,
}

trait WinitApp {
    /// The current frame number, as reported by egui.
    fn frame_nr(&self) -> u64;

    fn is_focused(&self, window_id: winit::window::WindowId) -> bool;

    fn integration(&self) -> Option<Arc<RwLock<EpiIntegration>>>;

    fn window(
        &self,
        window_id: winit::window::WindowId,
    ) -> Option<Arc<RwLock<winit::window::Window>>>;

    fn get_window_winit_id(&self, id: ViewportId) -> Option<winit::window::WindowId>;

    fn get_window_id(&self, id: &winit::window::WindowId) -> Option<ViewportId>;

    fn save_and_destroy(&mut self);

    fn run_ui_and_paint(&mut self, window_id: winit::window::WindowId) -> Vec<EventResult>;

    fn on_event(
        &mut self,
        event_loop: &EventLoopWindowTarget<UserEvent>,
        event: &winit::event::Event<'_, UserEvent>,
    ) -> Result<EventResult>;
}

fn create_event_loop_builder(
    native_options: &mut epi::NativeOptions,
) -> EventLoopBuilder<UserEvent> {
    crate::profile_function!();
    let mut event_loop_builder = winit::event_loop::EventLoopBuilder::with_user_event();

    if let Some(hook) = std::mem::take(&mut native_options.event_loop_builder) {
        hook(&mut event_loop_builder);
    }

    event_loop_builder
}

fn create_event_loop(native_options: &mut epi::NativeOptions) -> EventLoop<UserEvent> {
    crate::profile_function!();
    let mut builder = create_event_loop_builder(native_options);

    crate::profile_scope!("EventLoopBuilder::build");
    builder.build()
}

/// Access a thread-local event loop.
///
/// We reuse the event-loop so we can support closing and opening an eframe window
/// multiple times. This is just a limitation of winit.
fn with_event_loop<R>(
    mut native_options: epi::NativeOptions,
    f: impl FnOnce(&mut EventLoop<UserEvent>, NativeOptions) -> R,
) -> R {
    use std::cell::RefCell;
    thread_local!(static EVENT_LOOP: RefCell<Option<EventLoop<UserEvent>>> = RefCell::new(None));

    EVENT_LOOP.with(|event_loop| {
        // Since we want to reference NativeOptions when creating the EventLoop we can't
        // do that as part of the lazy thread local storage initialization and so we instead
        // create the event loop lazily here
        let mut event_loop = event_loop.borrow_mut();
        let event_loop = event_loop.get_or_insert_with(|| create_event_loop(&mut native_options));
        f(event_loop, native_options)
    })
}

#[cfg(not(target_os = "ios"))]
fn run_and_return(
    event_loop: &mut EventLoop<UserEvent>,
    mut winit_app: impl WinitApp,
) -> Result<()> {
    use winit::platform::run_return::EventLoopExtRunReturn as _;

    log::debug!("Entering the winit event loop (run_return)…");

    let mut windows_next_repaint_times = HashMap::default();

    let mut returned_result = Ok(());

    event_loop.run_return(|event, event_loop, control_flow| {
        crate::profile_scope!("winit_event", short_event_description(&event));

        WINIT_EVENT_LOOP.with(|row_event_loop| *row_event_loop.write() = event_loop);

        let events = match &event {
            winit::event::Event::LoopDestroyed => {
                // On Mac, Cmd-Q we get here and then `run_return` doesn't return (despite its name),
                // so we need to save state now:
                log::debug!("Received Event::LoopDestroyed - saving app state…");
                winit_app.save_and_destroy();
                *control_flow = ControlFlow::Exit;
                return;
            }

            // Platform-dependent event handlers to workaround a winit bug
            // See: https://github.com/rust-windowing/winit/issues/987
            // See: https://github.com/rust-windowing/winit/issues/1619
            // #[cfg(target_os = "windows")]
            // winit::event::Event::RedrawEventsCleared => {
            // windows_next_repaint_times.clear();
            // winit_app.run_ui_and_paint(None)
            // vec![EventResult::Wait]
            // }
            // #[cfg(not(target_os = "windows"))]
            winit::event::Event::RedrawRequested(window_id) => {
                windows_next_repaint_times.remove(window_id);
                winit_app.run_ui_and_paint(*window_id)
            }

            winit::event::Event::UserEvent(UserEvent::RequestRepaint {
                when,
                frame_nr,
                id: window_id,
            }) => {
                if winit_app.frame_nr() == *frame_nr {
                    log::trace!("UserEvent::RequestRepaint scheduling repaint at {when:?}");
                    if let Some(window_id) = winit_app.get_window_winit_id(*window_id) {
                        vec![EventResult::RepaintAt(window_id, *when)]
                    } else {
                        vec![EventResult::Wait]
                    }
                } else {
                    log::trace!("Got outdated UserEvent::RequestRepaint");
                    vec![EventResult::Wait] // old request - we've already repainted
                }
            }

            winit::event::Event::NewEvents(winit::event::StartCause::ResumeTimeReached {
                ..
            }) => {
                log::trace!("Woke up to check next_repaint_time");
                vec![EventResult::Wait]
            }

            winit::event::Event::WindowEvent { window_id, .. }
                if winit_app.window(*window_id).is_none() =>
            {
                // This can happen if we close a window, and then reopen a new one,
                // or if we have multiple windows open.
                vec![EventResult::RepaintNext(*window_id)]
            }

            event => match winit_app.on_event(event_loop, event) {
                Ok(event_result) => vec![event_result],
                Err(err) => {
                    log::error!("Exiting because of error: {err:?} on event {event:?}");
                    returned_result = Err(err);
                    vec![EventResult::Exit]
                }
            },
        };

        for event in events {
            match event {
                EventResult::Wait => {
                    control_flow.set_wait();
                }
                EventResult::RepaintNow(window_id) => {
                    log::trace!("Repaint caused by winit::Event: {:?}", event);
                    if cfg!(target_os = "windows") {
                        // Fix flickering on Windows, see https://github.com/emilk/egui/pull/2280
                        windows_next_repaint_times.remove(&window_id);

                        winit_app.run_ui_and_paint(window_id);
                    } else {
                        // Fix for https://github.com/emilk/egui/issues/2425
                        windows_next_repaint_times.insert(window_id, Instant::now());
                    }
                }
                EventResult::RepaintNext(window_id) => {
                    log::trace!("Repaint caused by winit::Event: {:?}", event);
                    windows_next_repaint_times.insert(window_id, Instant::now());
                }
                EventResult::RepaintAt(window_id, repaint_time) => {
                    windows_next_repaint_times.insert(
                        window_id,
                        windows_next_repaint_times
                            .get(&window_id)
                            .map_or(repaint_time, |last| (*last).min(repaint_time)),
                    );
                }
                EventResult::Exit => {
                    log::debug!("Asking to exit event loop…");
                    winit_app.save_and_destroy();
                    *control_flow = ControlFlow::Exit;
                    return;
                }
            }
        }

        let mut next_repaint_time = Option::<Instant>::None;
        for (window_id, repaint_time) in &windows_next_repaint_times.clone() {
            if *repaint_time <= Instant::now() {
                if let Some(window) = winit_app.window(*window_id) {
                    window.read().request_redraw();
                    windows_next_repaint_times.remove(window_id);
                    control_flow.set_poll();
                } else {
                    windows_next_repaint_times.remove(window_id);
                    control_flow.set_wait();
                }
            } else {
                next_repaint_time =
                    Some(next_repaint_time.map_or(*repaint_time, |last| last.min(*repaint_time)));
            }
        }

        if let Some(next_repaint_time) = next_repaint_time {
            let time_until_next = next_repaint_time.saturating_duration_since(Instant::now());
            if time_until_next < std::time::Duration::from_secs(10_000) {
                log::trace!("WaitUntil {time_until_next:?}");
            }
            control_flow.set_wait_until(next_repaint_time);
        };
    });

    log::debug!("eframe window closed");

    drop(winit_app);

    // On Windows this clears out events so that we can later create another window.
    // See https://github.com/emilk/egui/pull/1889 for details.
    //
    // Note that this approach may cause issues on macOS (emilk/egui#2768); therefore,
    // we only apply this approach on Windows to minimize the affect.
    #[cfg(target_os = "windows")]
    {
        event_loop.run_return(|_, _, control_flow| {
            control_flow.set_exit();
        });
    }

    returned_result
}

fn run_and_exit(event_loop: EventLoop<UserEvent>, mut winit_app: impl WinitApp + 'static) -> ! {
    log::debug!("Entering the winit event loop (run)…");

    let mut windows_next_repaint_times = HashMap::default();

    event_loop.run(move |event, event_loop, control_flow| {
        crate::profile_scope!("winit_event", short_event_description(&event));

        WINIT_EVENT_LOOP.with(|row_event_loop| *row_event_loop.write() = event_loop);

        let events = match event {
            winit::event::Event::LoopDestroyed => {
                log::debug!("Received Event::LoopDestroyed");
                vec![EventResult::Exit]
            }

            // Platform-dependent event handlers to workaround a winit bug
            // See: https://github.com/rust-windowing/winit/issues/987
            // See: https://github.com/rust-windowing/winit/issues/1619
            winit::event::Event::RedrawEventsCleared if cfg!(target_os = "windows") => {
                // windows_next_repaint_times.clear();
                // winit_app.run_ui_and_paint(None)
                vec![]
            }
            winit::event::Event::RedrawRequested(window_id) if !cfg!(target_os = "windows") => {
                windows_next_repaint_times.remove(&window_id);
                winit_app.run_ui_and_paint(window_id)
            }

            winit::event::Event::UserEvent(UserEvent::RequestRepaint {
                when,
                frame_nr,
                id: window_id,
            }) => {
                if winit_app.frame_nr() == frame_nr {
                    if let Some(window_id) = winit_app.get_window_winit_id(window_id) {
                        vec![EventResult::RepaintAt(window_id, when)]
                    } else {
                        vec![EventResult::Wait]
                    }
                } else {
                    vec![EventResult::Wait] // old request - we've already repainted
                }
            }

            winit::event::Event::NewEvents(winit::event::StartCause::ResumeTimeReached {
                ..
            }) => vec![EventResult::Wait], // We just woke up to check next_repaint_time

            event => match winit_app.on_event(event_loop, &event) {
                Ok(event_result) => vec![event_result],
                Err(err) => {
                    panic!("eframe encountered a fatal error: {err}");
                }
            },
        };

        for event in events {
            match event {
                EventResult::Wait => {}
                EventResult::RepaintNow(window_id) => {
                    if cfg!(target_os = "windows") {
                        // Fix flickering on Windows, see https://github.com/emilk/egui/pull/2280
                        windows_next_repaint_times.remove(&window_id);

                        winit_app.run_ui_and_paint(window_id);
                    } else {
                        // Fix for https://github.com/emilk/egui/issues/2425
                        windows_next_repaint_times.insert(window_id, Instant::now());
                    }
                }
                EventResult::RepaintNext(window_id) => {
                    windows_next_repaint_times.insert(window_id, Instant::now());
                }
                EventResult::RepaintAt(window_id, repaint_time) => {
                    windows_next_repaint_times.insert(
                        window_id,
                        windows_next_repaint_times
                            .get(&window_id)
                            .map_or(repaint_time, |last| (*last).min(repaint_time)),
                    );
                }
                EventResult::Exit => {
                    log::debug!("Quitting - saving app state…");
                    winit_app.save_and_destroy();
                    #[allow(clippy::exit)]
                    std::process::exit(0);
                }
            }
        }

        let mut next_repaint_time = Option::<Instant>::None;
        for (window_id, repaint_time) in &windows_next_repaint_times.clone() {
            if *repaint_time <= Instant::now() {
                if let Some(window) = winit_app.window(*window_id) {
                    log::trace!("request_redraw");
                    window.read().request_redraw();
                    windows_next_repaint_times.remove(window_id);
                }
                control_flow.set_poll();
            } else {
                next_repaint_time =
                    Some(next_repaint_time.map_or(*repaint_time, |last| last.min(*repaint_time)));
            }
        }

        if let Some(next_repaint_time) = next_repaint_time {
            let time_until_next = next_repaint_time.saturating_duration_since(Instant::now());
            if time_until_next < std::time::Duration::from_secs(10_000) {
                log::trace!("WaitUntil {time_until_next:?}");
            }

            // WaitUntil seems to not work on iOS
            #[cfg(target_os = "ios")]
            winit_app
                .get_window_winit_id(ViewportId::MAIN)
                .map(|window_id| {
                    winit_app
                        .window(window_id)
                        .map(|window| window.read().request_redraw())
                });

            control_flow.set_wait_until(next_repaint_time);
        };
    })
}

// ----------------------------------------------------------------------------
/// Run an egui app
#[cfg(feature = "glow")]
mod glow_integration {
    use std::sync::Arc;

    use egui::{
        epaint::ahash::HashMap, mutex::RwLock, NumExt as _, ViewportIdPair, ViewportOutput,
        ViewportRender,
    };
    use egui_winit::{
        changes_between_builders, create_winit_window_builder, process_viewport_commands,
        EventResponse,
    };
    use glutin::{
        display::GetGlDisplay,
        prelude::{GlDisplay, NotCurrentGlContextSurfaceAccessor, PossiblyCurrentGlContext},
        surface::GlSurface,
    };

    use super::*;

    // Note: that the current Glutin API design tightly couples the GL context with
    // the Window which means it's not practically possible to just destroy the
    // window and re-create a new window while continuing to use the same GL context.
    //
    // For now this means it's not possible to support Android as well as we can with
    // wgpu because we're basically forced to destroy and recreate _everything_ when
    // the application suspends and resumes.
    //
    // There is work in progress to improve the Glutin API so it has a separate Surface
    // API that would allow us to just destroy a Window/Surface when suspending, see:
    // https://github.com/rust-windowing/glutin/pull/1435
    //

    /// State that is initialized when the application is first starts running via
    /// a Resumed event. On Android this ensures that any graphics state is only
    /// initialized once the application has an associated `SurfaceView`.
    struct GlowWinitRunning {
        gl: Arc<glow::Context>,
        painter: Arc<RwLock<egui_glow::Painter>>,
        integration: Arc<RwLock<epi_integration::EpiIntegration>>,
        app: Arc<RwLock<Box<dyn epi::App>>>,
        // Conceptually this will be split out eventually so that the rest of the state
        // can be persistent.
        glutin_ctx: Arc<RwLock<GlutinWindowContext>>,
    }

    struct Window {
        gl_surface: Option<glutin::surface::Surface<glutin::surface::WindowSurface>>,
        window: Option<Arc<RwLock<winit::window::Window>>>,
        pair: ViewportIdPair,
        render: Option<Arc<Box<ViewportRender>>>,
        egui_winit: Option<egui_winit::State>,
    }

    /// This struct will contain both persistent and temporary glutin state.
    ///
    /// Platform Quirks:
    /// * Microsoft Windows: requires that we create a window before opengl context.
    /// * Android: window and surface should be destroyed when we receive a suspend event. recreate on resume event.
    ///
    /// winit guarantees that we will get a Resumed event on startup on all platforms.
    /// * Before Resumed event: `gl_config`, `gl_context` can be created at any time. on windows, a window must be created to get `gl_context`.
    /// * Resumed: `gl_surface` will be created here. `window` will be re-created here for android.
    /// * Suspended: on android, we drop window + surface.  on other platforms, we don't get Suspended event.
    ///
    /// The setup is divided between the `new` fn and `on_resume` fn. we can just assume that `on_resume` is a continuation of
    /// `new` fn on all platforms. only on android, do we get multiple resumed events because app can be suspended.
    struct GlutinWindowContext {
        swap_interval: glutin::surface::SwapInterval,
        gl_config: glutin::config::Config,

        current_gl_context: Option<glutin::context::PossiblyCurrentContext>,
        not_current_gl_context: Option<glutin::context::NotCurrentContext>,

        viewports: HashMap<ViewportId, Arc<RwLock<Window>>>,
        viewports_maps: HashMap<winit::window::WindowId, ViewportId>,
        builders: HashMap<ViewportId, ViewportBuilder>,
    }

    #[allow(unsafe_code)]
    unsafe impl Sync for GlutinWindowContext {}
    #[allow(unsafe_code)]
    unsafe impl Send for GlutinWindowContext {}

    impl GlutinWindowContext {
        /// There is a lot of complexity with opengl creation, so prefer extensive logging to get all the help we can to debug issues.
        ///
        #[allow(unsafe_code)]
        unsafe fn new(
            window_builder: ViewportBuilder,
            native_options: &epi::NativeOptions,
            event_loop: &EventLoopWindowTarget<UserEvent>,
        ) -> Result<Self> {
            crate::profile_function!();

            use glutin::prelude::*;
            // convert native options to glutin options
            let hardware_acceleration = match native_options.hardware_acceleration {
                crate::HardwareAcceleration::Required => Some(true),
                crate::HardwareAcceleration::Preferred => None,
                crate::HardwareAcceleration::Off => Some(false),
            };
            let swap_interval = if native_options.vsync {
                glutin::surface::SwapInterval::Wait(std::num::NonZeroU32::new(1).unwrap())
            } else {
                glutin::surface::SwapInterval::DontWait
            };
            /*  opengl setup flow goes like this:
                1. we create a configuration for opengl "Display" / "Config" creation
                2. choose between special extensions like glx or egl or wgl and use them to create config/display
                3. opengl context configuration
                4. opengl context creation
            */
            // start building config for gl display
            let config_template_builder = glutin::config::ConfigTemplateBuilder::new()
                .prefer_hardware_accelerated(hardware_acceleration)
                .with_depth_size(native_options.depth_buffer)
                .with_stencil_size(native_options.stencil_buffer)
                .with_transparency(native_options.transparent);
            // we don't know if multi sampling option is set. so, check if its more than 0.
            let config_template_builder = if native_options.multisampling > 0 {
                config_template_builder.with_multisampling(
                    native_options
                        .multisampling
                        .try_into()
                        .expect("failed to fit multisamples option of native_options into u8"),
                )
            } else {
                config_template_builder
            };

            log::debug!(
                "trying to create glutin Display with config: {:?}",
                &config_template_builder
            );

            // Create GL display. This may probably create a window too on most platforms. Definitely on `MS windows`. Never on Android.
            let display_builder = glutin_winit::DisplayBuilder::new()
                // we might want to expose this option to users in the future. maybe using an env var or using native_options.
                .with_preference(glutin_winit::ApiPrefence::FallbackEgl) // https://github.com/emilk/egui/issues/2520#issuecomment-1367841150
                .with_window_builder(Some(create_winit_window_builder(&window_builder)));

            let (window, gl_config) = {
                crate::profile_scope!("DisplayBuilder::build");

                display_builder
                    .build(
                        event_loop,
                        config_template_builder.clone(),
                        |mut config_iterator| {
                            let config = config_iterator.next().expect(
                            "failed to find a matching configuration for creating glutin config",
                        );
                            log::debug!(
                                "using the first config from config picker closure. config: {:?}",
                                &config
                            );
                            config
                        },
                    )
                    .map_err(|e| {
                        crate::Error::NoGlutinConfigs(config_template_builder.build(), e)
                    })?
            };

            let gl_display = gl_config.display();
            log::debug!(
                "successfully created GL Display with version: {} and supported features: {:?}",
                gl_display.version_string(),
                gl_display.supported_features()
            );
            let raw_window_handle = window.as_ref().map(|w| w.raw_window_handle());
            log::debug!(
                "creating gl context using raw window handle: {:?}",
                raw_window_handle
            );

            // create gl context. if core context cannot be created, try gl es context as fallback.
            let context_attributes =
                glutin::context::ContextAttributesBuilder::new().build(raw_window_handle);
            let fallback_context_attributes = glutin::context::ContextAttributesBuilder::new()
                .with_context_api(glutin::context::ContextApi::Gles(None))
                .build(raw_window_handle);

            let gl_context_result = {
                crate::profile_scope!("create_context");
                gl_config
                    .display()
                    .create_context(&gl_config, &context_attributes)
            };

            let gl_context = match gl_context_result {
                Ok(it) => it,
                Err(err) => {
                    log::warn!("failed to create context using default context attributes {context_attributes:?} due to error: {err}");
                    log::debug!("retrying with fallback context attributes: {fallback_context_attributes:?}");
                    gl_config
                        .display()
                        .create_context(&gl_config, &fallback_context_attributes)?
                }
            };
            let not_current_gl_context = Some(gl_context);

            let mut window_maps = HashMap::default();
            if let Some(window) = &window {
                window_maps.insert(window.id(), ViewportId::MAIN);
            }

            let mut windows = HashMap::default();
            windows.insert(
                ViewportId::MAIN,
                Arc::new(RwLock::new(Window {
                    gl_surface: None,
                    window: window.map(|w| Arc::new(RwLock::new(w))),
                    egui_winit: None,
                    render: None,
                    pair: ViewportIdPair::MAIN,
                })),
            );

            let mut builders = HashMap::default();
            builders.insert(ViewportId::MAIN, window_builder);

            // the fun part with opengl gl is that we never know whether there is an error. the context creation might have failed, but
            // it could keep working until we try to make surface current or swap buffers or something else. future glutin improvements might
            // help us start from scratch again if we fail context creation and go back to preferEgl or try with different config etc..
            // https://github.com/emilk/egui/pull/2541#issuecomment-1370767582
            Ok(GlutinWindowContext {
                swap_interval,
                gl_config,
                current_gl_context: None,
                not_current_gl_context,
                viewports: windows,
                builders,
                viewports_maps: window_maps,
            })
        }

        /// This will be run after `new`. on android, it might be called multiple times over the course of the app's lifetime.
        /// roughly,
        /// 1. check if window already exists. otherwise, create one now.
        /// 2. create attributes for surface creation.
        /// 3. create surface.
        /// 4. make surface and context current.
        ///
        /// we presently assume that we will
        fn on_resume(&mut self, event_loop: &EventLoopWindowTarget<UserEvent>) -> Result<()> {
            crate::profile_function!();

            let values = self
                .viewports
                .values()
                .cloned()
                .collect::<Vec<Arc<RwLock<Window>>>>();
            for win in values {
                if win.read().gl_surface.is_some() {
                    continue;
                }
                self.init_window(&win, event_loop)?;
            }
            Ok(())
        }

        #[allow(unsafe_code)]
        pub(crate) fn init_window(
            &mut self,
            win: &Arc<RwLock<Window>>,
            event_loop: &EventLoopWindowTarget<UserEvent>,
        ) -> Result<()> {
            let builder = &self.builders[&win.read().pair.this];
            let mut win = win.write();
            // make sure we have a window or create one.
            let window = win.window.take().unwrap_or_else(|| {
                log::debug!("window doesn't exist yet. creating one now with finalize_window");
                Arc::new(RwLock::new(
                    glutin_winit::finalize_window(
                        event_loop,
                        create_winit_window_builder(builder),
                        &self.gl_config,
                    )
                    .expect("failed to finalize glutin window"),
                ))
            });
            {
                let window = window.read();
                // surface attributes
                let (width, height): (u32, u32) = window.inner_size().into();
                let width = std::num::NonZeroU32::new(width.at_least(1)).unwrap();
                let height = std::num::NonZeroU32::new(height.at_least(1)).unwrap();
                let surface_attributes = glutin::surface::SurfaceAttributesBuilder::<
                    glutin::surface::WindowSurface,
                >::new()
                .build(window.raw_window_handle(), width, height);
                log::debug!(
                    "creating surface with attributes: {:?}",
                    &surface_attributes
                );
                // create surface
                let gl_surface = unsafe {
                    self.gl_config
                        .display()
                        .create_window_surface(&self.gl_config, &surface_attributes)?
                };
                log::debug!("surface created successfully: {gl_surface:?}.making context current");
                // make surface and context current.
                let not_current_gl_context =
                    if let Some(not_current_context) = self.not_current_gl_context.take() {
                        not_current_context
                    } else {
                        self.current_gl_context
                            .take()
                            .unwrap()
                            .make_not_current()
                            .unwrap()
                    };
                let current_gl_context = not_current_gl_context.make_current(&gl_surface)?;
                // try setting swap interval. but its not absolutely necessary, so don't panic on failure.
                log::debug!("made context current. setting swap interval for surface");
                if let Err(e) =
                    gl_surface.set_swap_interval(&current_gl_context, self.swap_interval)
                {
                    log::error!("failed to set swap interval due to error: {e:?}");
                }
                // we will reach this point only once in most platforms except android.
                // create window/surface/make context current once and just use them forever.

                let native_pixels_per_point = window.scale_factor() as f32;

                if win.egui_winit.is_none() {
                    let mut egui_winit = egui_winit::State::new(event_loop);
                    // egui_winit.set_max_texture_side(max_texture_side);
                    egui_winit.set_pixels_per_point(native_pixels_per_point);
                    win.egui_winit = Some(egui_winit);
                }

                win.gl_surface = Some(gl_surface);
                self.current_gl_context = Some(current_gl_context);
                self.viewports_maps.insert(window.id(), win.pair.this);
            }
            win.window = Some(window);
            Ok(())
        }

        /// only applies for android. but we basically drop surface + window and make context not current
        fn on_suspend(&mut self) -> Result<()> {
            log::debug!("received suspend event. dropping window and surface");
            for window in self.viewports.values() {
                let mut window = window.write();
                window.gl_surface.take();
                window.window.take();
            }
            if let Some(current) = self.current_gl_context.take() {
                log::debug!("context is current, so making it non-current");
                self.not_current_gl_context = Some(current.make_not_current()?);
            } else {
                log::debug!("context is already not current??? could be duplicate suspend event");
            }
            Ok(())
        }

        fn window(&self, viewport_id: ViewportId) -> Arc<RwLock<Window>> {
            self.viewports
                .get(&viewport_id)
                .cloned()
                .expect("winit window doesn't exist")
        }

        fn resize(
            &mut self,
            viewport_id: ViewportId,
            physical_size: winit::dpi::PhysicalSize<u32>,
        ) {
            let width = std::num::NonZeroU32::new(physical_size.width.at_least(1)).unwrap();
            let height = std::num::NonZeroU32::new(physical_size.height.at_least(1)).unwrap();

            if let Some(window) = self.viewports.get(&viewport_id) {
                let window = window.read();
                if let Some(gl_surface) = &window.gl_surface {
                    self.current_gl_context = Some(
                        self.current_gl_context
                            .take()
                            .unwrap()
                            .make_not_current()
                            .unwrap()
                            .make_current(gl_surface)
                            .unwrap(),
                    );
                    gl_surface.resize(
                        self.current_gl_context
                            .as_ref()
                            .expect("failed to get current context to resize surface"),
                        width,
                        height,
                    );
                }
            }
        }

        fn get_proc_address(&self, addr: &std::ffi::CStr) -> *const std::ffi::c_void {
            self.gl_config.display().get_proc_address(addr)
        }
    }

    struct GlowWinitApp {
        repaint_proxy: Arc<egui::mutex::Mutex<EventLoopProxy<UserEvent>>>,
        app_name: String,
        native_options: epi::NativeOptions,
        running: Arc<RwLock<Option<GlowWinitRunning>>>,

        // Note that since this `AppCreator` is FnOnce we are currently unable to support
        // re-initializing the `GlowWinitRunning` state on Android if the application
        // suspends and resumes.
        app_creator: Option<epi::AppCreator>,
        is_focused: Arc<RwLock<Option<ViewportId>>>,
    }

    impl GlowWinitApp {
        fn new(
            event_loop: &EventLoop<UserEvent>,
            app_name: &str,
            native_options: epi::NativeOptions,
            app_creator: epi::AppCreator,
        ) -> Self {
            crate::profile_function!();
            Self {
                repaint_proxy: Arc::new(egui::mutex::Mutex::new(event_loop.create_proxy())),
                app_name: app_name.to_owned(),
                native_options,
                running: Arc::new(RwLock::new(None)),
                app_creator: Some(app_creator),
                is_focused: Arc::new(RwLock::new(Some(ViewportId::MAIN))),
            }
        }

        #[allow(unsafe_code)]
        fn create_glutin_windowed_context(
            event_loop: &EventLoopWindowTarget<UserEvent>,
            storage: Option<&dyn epi::Storage>,
            title: &str,
            native_options: &NativeOptions,
        ) -> Result<(GlutinWindowContext, glow::Context)> {
            crate::profile_function!();

            let window_settings = epi_integration::load_window_settings(storage);

            let winit_window_builder =
                epi_integration::window_builder(event_loop, title, native_options, window_settings);
            let mut glutin_window_context = unsafe {
                GlutinWindowContext::new(winit_window_builder, native_options, event_loop)?
            };
            glutin_window_context.on_resume(event_loop)?;

            if let Some(window) = &glutin_window_context.viewports.get(&ViewportId::MAIN) {
                let window = window.read();
                if let Some(window) = &window.window {
                    epi_integration::apply_native_options_to_window(
                        &window.read(),
                        native_options,
                        window_settings,
                    );
                }
            }

            let gl = unsafe {
                crate::profile_scope!("glow::Context::from_loader_function");
                glow::Context::from_loader_function(|s| {
                    let s = std::ffi::CString::new(s)
                        .expect("failed to construct C string from string for gl proc address");

                    glutin_window_context.get_proc_address(&s)
                })
            };

            Ok((glutin_window_context, gl))
        }

        fn init_run_state(&mut self, event_loop: &EventLoopWindowTarget<UserEvent>) -> Result<()> {
            crate::profile_function!();
            let storage = epi_integration::create_storage(
                self.native_options
                    .app_id
                    .as_ref()
                    .unwrap_or(&self.app_name),
            );

            let (gl_window, gl) = Self::create_glutin_windowed_context(
                event_loop,
                storage.as_deref(),
                &self.app_name,
                &self.native_options,
            )?;
            let gl = Arc::new(gl);

            let painter =
                egui_glow::Painter::new(gl.clone(), "", self.native_options.shader_version)
                    .unwrap_or_else(|err| panic!("An OpenGL error occurred: {err}\n"));

            let system_theme = system_theme(
                &gl_window
                    .window(ViewportId::MAIN)
                    .read()
                    .window
                    .as_ref()
                    .unwrap()
                    .read(),
                &self.native_options,
            );
            let mut integration = epi_integration::EpiIntegration::new(
                &gl_window
                    .window(ViewportId::MAIN)
                    .read()
                    .window
                    .as_ref()
                    .unwrap()
                    .read(),
                system_theme,
                &self.app_name,
                &self.native_options,
                storage,
                IS_DESKTOP,
                Some(gl.clone()),
                #[cfg(feature = "wgpu")]
                None,
            );
            #[cfg(feature = "accesskit")]
            {
                let window = &gl_window.viewports[&ViewportId::MAIN];
                let window = &mut *window.write();
                integration.init_accesskit(
                    window.egui_winit.as_mut().unwrap(),
                    &window.window.as_ref().unwrap().read(),
                    self.repaint_proxy.lock().clone(),
                );
            }
            let theme = system_theme.unwrap_or(self.native_options.default_theme);
            integration.egui_ctx.set_visuals(theme.egui_visuals());

            if self.native_options.mouse_passthrough {
                gl_window
                    .window(ViewportId::MAIN)
                    .read()
                    .window
                    .as_ref()
                    .unwrap()
                    .read()
                    .set_cursor_hittest(false)
                    .unwrap();
            }

            {
                let event_loop_proxy = self.repaint_proxy.clone();
                integration
                    .egui_ctx
                    .set_request_repaint_callback(move |info| {
                        log::trace!("request_repaint_callback: {info:?}");
                        let when = Instant::now() + info.after;
                        let frame_nr = info.current_frame_nr;
                        event_loop_proxy
                            .lock()
                            .send_event(UserEvent::RequestRepaint {
                                id: info.viewport_id,
                                when,
                                frame_nr,
                            })
                            .ok();
                    });
            }

            let app_creator = std::mem::take(&mut self.app_creator)
                .expect("Single-use AppCreator has unexpectedly already been taken");
            let mut app;
            {
                let window = gl_window.window(ViewportId::MAIN);
                let window = &mut *window.write();
                app = app_creator(&epi::CreationContext {
                    egui_ctx: integration.egui_ctx.clone(),
                    integration_info: integration.frame.info().clone(),
                    storage: integration.frame.storage(),
                    gl: Some(gl.clone()),
                    #[cfg(feature = "wgpu")]
                    wgpu_render_state: None,
                    raw_display_handle: window.window.as_ref().unwrap().read().raw_display_handle(),
                    raw_window_handle: window.window.as_ref().unwrap().read().raw_window_handle(),
                });

                if app.warm_up_enabled() {
                    integration.warm_up(
                        app.as_mut(),
                        &window.window.as_ref().unwrap().read(),
                        window.egui_winit.as_mut().unwrap(),
                    );
                }
            }

            let glutin_ctx = Arc::new(RwLock::new(gl_window));
            let painter = Arc::new(RwLock::new(painter));

            // c_* means that will be taken by the next closure

            let c_glutin = glutin_ctx.clone();
            let c_gl = gl.clone();
            let c_painter = painter.clone();
            let c_time = integration.beginning;

            // ## Sync Rendering
            integration.egui_ctx.set_render_sync_callback(
                move |egui_ctx, mut viewport_builder, pair, render| {

                    let has_window = c_glutin.read().viewports.get(&pair).is_some();
                    if !has_window{
                        if viewport_builder.icon.is_none(){
                            viewport_builder.icon = c_glutin.read().builders.get(&pair.parent).and_then(|b|b.icon.clone());
                        }

                        {
                            let mut glutin = c_glutin.write();
                            glutin.viewports.entry(pair.this).or_insert(Arc::new(RwLock::new(Window{ gl_surface: None, window: None, pair, render: None, egui_winit: None })));
                            glutin.builders.entry(pair.this).or_insert(viewport_builder);
                        }

                        let win = c_glutin.read().viewports[&pair].clone();
                        let event_loop;
                        #[allow(unsafe_code)]
                        unsafe{
                            event_loop = WINIT_EVENT_LOOP.with(|event_loop|event_loop.read().as_ref().unwrap());
                        }
                        c_glutin.write().init_window(&win, event_loop).expect("Cannot init window on egui::Context::create_viewport_sync");
                    }

                    'try_render: {
                        let window = c_glutin.read().viewports.get(&pair).cloned();
                        if let Some(window) = window {
                            let output;
                            {
                                let window = &mut *window.write();
                                if let Some(winit_state) = &mut window.egui_winit {
                                    if let Some(win) = window.window.clone() {
                                        let win = win.read();
                                        let mut input = winit_state.take_egui_input(&win);
                                        input.time = Some(c_time.elapsed().as_secs_f64());
                                        output = egui_ctx.run(
                                            input,
                                            pair,
                                            |ctx| {
                                                render(ctx);
                                            },
                                        );
                                        let glutin = &mut *c_glutin.write();

                                        let screen_size_in_pixels: [u32; 2] =
                                            win.inner_size().into();

                                        let clipped_primitives = egui_ctx.tessellate(output.shapes);

                                        glutin.current_gl_context = Some(
                                            glutin
                                                .current_gl_context
                                                .take()
                                                .unwrap()
                                                .make_not_current()
                                                .unwrap()
                                                .make_current(window.gl_surface.as_ref().unwrap())
                                                .unwrap(),
                                        );

                                        if !window
                                            .gl_surface
                                            .as_ref()
                                            .unwrap()
                                            .is_current(glutin.current_gl_context.as_ref().unwrap())
                                        {
                                            let builder = &&glutin.builders[&window.pair];
                                            log::error!("egui::create_viewport_sync with title: `{}` is not created in main thread, try to use wgpu!", builder.title);
                                        }

                                        egui_glow::painter::clear(
                                            &c_gl,
                                            screen_size_in_pixels,
                                            [0.0, 0.0, 0.0, 0.0],
                                        );

                                        c_painter.write().paint_and_update_textures(
                                            screen_size_in_pixels,
                                            egui_ctx.pixels_per_point(),
                                            &clipped_primitives,
                                            &output.textures_delta,
                                        );
                                        crate::profile_scope!("swap_buffers");
                                        let _ = window
                                            .gl_surface
                                            .as_ref()
                                            .expect("failed to get surface to swap buffers")
                                            .swap_buffers(
                                                glutin.current_gl_context.as_ref().expect(
                                                    "failed to get current context to swap buffers",
                                                ),
                                            );
                                        winit_state.handle_platform_output(
                                            &win,
                                            egui_ctx,
                                            output.platform_output,
                                        );
                                    } else {
                                        break 'try_render;
                                    }
                                } else {
                                    break 'try_render;
                                }
                            }
                        }
                    }
                },
            );

            *self.running.write() = Some(GlowWinitRunning {
                glutin_ctx,
                gl,
                painter,
                integration: Arc::new(RwLock::new(integration)),
                app: Arc::new(RwLock::new(app)),
            });

            Ok(())
        }

        fn process_viewport_builders(
            glutin_ctx: &Arc<RwLock<GlutinWindowContext>>,
            mut viewports: Vec<ViewportOutput>,
        ) {
            let mut active_viewports_ids = vec![ViewportId::MAIN];

            viewports.retain_mut(
                |ViewportOutput {
                     builder,
                     pair: ViewportIdPair { this: id, .. },
                     render,
                 }| {
                    let mut glutin = glutin_ctx.write();
                    let last_builder = glutin.builders.entry(*id).or_insert(builder.clone());
                    let (commands, recreate) = changes_between_builders(builder, last_builder);
                    drop(glutin);
                    if let Some(w) = glutin_ctx.read().viewports.get(id) {
                        let mut w = w.write();
                        if recreate {
                            w.window = None;
                            w.gl_surface = None;
                            w.render = render.clone();
                            w.pair.parent = *id;
                        }
                        if let Some(w) = w.window.clone() {
                            process_viewport_commands(commands, *id, None, &w);
                        }
                        active_viewports_ids.push(*id);
                        false
                    } else {
                        true
                    }
                },
            );

            for ViewportOutput {
                mut builder,
                pair,
                render,
            } in viewports
            {
                let default_icon = glutin_ctx
                    .read()
                    .builders
                    .get(&pair.parent)
                    .and_then(|b| b.icon.clone());

                if builder.icon.is_none() {
                    builder.icon = default_icon;
                }
                {
                    let mut glutin = glutin_ctx.write();
                    glutin.viewports.insert(
                        pair.this,
                        Arc::new(RwLock::new(Window {
                            gl_surface: None,
                            window: None,
                            egui_winit: None,
                            render,
                            pair,
                        })),
                    );
                    glutin.builders.insert(pair.this, builder);
                }
                active_viewports_ids.push(pair.this);
            }

            let mut gl_window = glutin_ctx.write();
            gl_window
                .viewports
                .retain(|id, _| active_viewports_ids.contains(id));
            gl_window
                .builders
                .retain(|id, _| active_viewports_ids.contains(id));
            gl_window
                .viewports_maps
                .retain(|_, id| active_viewports_ids.contains(id));
        }
    }

    impl WinitApp for GlowWinitApp {
        fn frame_nr(&self) -> u64 {
            self.running
                .read()
                .as_ref()
                .map_or(0, |r| r.integration.read().egui_ctx.frame_nr())
        }

        fn is_focused(&self, window_id: winit::window::WindowId) -> bool {
            if let Some(is_focused) = self.is_focused.read().as_ref() {
                if let Some(running) = self.running.read().as_ref() {
                    if let Some(window_id) =
                        running.glutin_ctx.read().viewports_maps.get(&window_id)
                    {
                        return *is_focused == *window_id;
                    }
                }
            }
            false
        }

        fn integration(&self) -> Option<Arc<RwLock<EpiIntegration>>> {
            self.running.read().as_ref().map(|r| r.integration.clone())
        }

        fn window(
            &self,
            window_id: winit::window::WindowId,
        ) -> Option<Arc<RwLock<winit::window::Window>>> {
            self.running.read().as_ref().and_then(|r| {
                let glutin_ctx = r.glutin_ctx.read();
                if let Some(viewport_id) = glutin_ctx.viewports_maps.get(&window_id) {
                    if let Some(viewport) = glutin_ctx.viewports.get(viewport_id) {
                        if let Some(window) = viewport.read().window.as_ref() {
                            return Some(window.clone());
                        }
                    }
                }
                None
            })
        }

        fn get_window_winit_id(&self, id: ViewportId) -> Option<winit::window::WindowId> {
            self.running.read().as_ref().and_then(|r| {
                if let Some(window) = r.glutin_ctx.read().viewports.get(&id) {
                    return window.read().window.as_ref().map(|w| w.read().id());
                }
                None
            })
        }

        fn get_window_id(&self, id: &winit::window::WindowId) -> Option<ViewportId> {
            self.running
                .read()
                .as_ref()
                .and_then(|r| r.glutin_ctx.read().viewports_maps.get(id).copied())
        }

        fn save_and_destroy(&mut self) {
            crate::profile_function!();
            if let Some(running) = self.running.write().take() {
                crate::profile_function!();

                running.integration.write().save(
                    running.app.write().as_mut(),
                    running
                        .glutin_ctx
                        .read()
                        .window(ViewportId::MAIN)
                        .read()
                        .window
                        .clone(),
                );
                running.app.write().on_exit(Some(&running.gl));
                running.painter.write().destroy();
            }
        }

        fn run_ui_and_paint(&mut self, window_id: winit::window::WindowId) -> Vec<EventResult> {
            if self.running.read().is_none() {
                return vec![EventResult::Wait];
            }

            if let Some(viewport_id) = self.get_window_id(&window_id) {
                #[cfg(feature = "puffin")]
                puffin::GlobalProfiler::lock().new_frame();
                crate::profile_scope!("frame");

                let (integration, app, glutin, painter) = {
                    let running = self.running.read();
                    let running = running.as_ref().unwrap();
                    (
                        running.integration.clone(),
                        running.app.clone(),
                        running.glutin_ctx.clone(),
                        running.painter.clone(),
                    )
                };

                // This will only happen if the viewport is sync
                // That means that the viewport cannot be rendered by itself and needs his parent to be rendered
                {
                    let win = &glutin.read().viewports[&viewport_id].clone();
                    if win.read().render.is_none() && viewport_id != ViewportId::MAIN {
                        if let Some(win) = glutin.read().viewports.get(&win.read().pair.parent) {
                            if let Some(w) = win.read().window.as_ref() {
                                return vec![EventResult::RepaintNow(w.read().id())];
                            }
                        }
                        return vec![];
                    }
                }

                let mut window_map = HashMap::default();
                for (id, window) in &glutin.read().viewports {
                    if let Some(win) = &window.read().window {
                        window_map.insert(*id, win.read().id());
                    }
                }

                let egui::FullOutput {
                    platform_output,
                    repaint_after,
                    textures_delta,
                    shapes,
                    viewports,
                    viewport_commands,
                };

                let control_flow;
                {
                    // let window = gl_window.window(window_index);
                    let win = glutin.read().viewports.get(&viewport_id).cloned();
                    let win = win.unwrap();

                    let screen_size_in_pixels: [u32; 2] = win
                        .read()
                        .window
                        .as_ref()
                        .unwrap()
                        .read()
                        .inner_size()
                        .into();

                    {
                        let win = &mut *win.write();
                        egui::FullOutput {
                            platform_output,
                            repaint_after,
                            textures_delta,
                            shapes,
                            viewports,
                            viewport_commands,
                        } = integration.write().update(
                            app.write().as_mut(),
                            &win.window.as_ref().unwrap().read(),
                            win.egui_winit.as_mut().unwrap(),
                            &win.render.clone(),
                            win.pair,
                        );

                        integration.write().handle_platform_output(
                            &win.window.as_ref().unwrap().read(),
                            platform_output,
                            win.egui_winit.as_mut().unwrap(),
                        );
                    }

                    let clipped_primitives = {
                        crate::profile_scope!("tessellate");
                        integration.read().egui_ctx.tessellate(shapes)
                    };
                    {
                        let mut gl_window = glutin.write();
                        gl_window.current_gl_context = Some(
                            gl_window
                                .current_gl_context
                                .take()
                                .unwrap()
                                .make_not_current()
                                .unwrap()
                                .make_current(win.read().gl_surface.as_ref().unwrap())
                                .unwrap(),
                        );
                    };

                    let gl = self.running.read().as_ref().unwrap().gl.clone();

                    egui_glow::painter::clear(
                        &gl,
                        screen_size_in_pixels,
                        app.read()
                            .clear_color(&integration.read().egui_ctx.style().visuals),
                    );

                    painter.write().paint_and_update_textures(
                        screen_size_in_pixels,
                        integration.read().egui_ctx.pixels_per_point(),
                        &clipped_primitives,
                        &textures_delta,
                    );

                    let mut integration = integration.write();
                    {
                        let screenshot_requested =
                            &mut integration.frame.output.screenshot_requested;

                        if *screenshot_requested {
                            *screenshot_requested = false;
                            let screenshot = painter.read().read_screen_rgba(screen_size_in_pixels);
                            integration.frame.screenshot.set(Some(screenshot));
                        }

                        integration.post_rendering(
                            app.write().as_mut(),
                            &win.read().window.as_ref().unwrap().read(),
                        );
                    }

                    {
                        crate::profile_scope!("swap_buffers");
                        let _ = win
                            .read()
                            .gl_surface
                            .as_ref()
                            .expect("failed to get surface to swap buffers")
                            .swap_buffers(
                                glutin
                                    .read()
                                    .current_gl_context
                                    .as_ref()
                                    .expect("failed to get current context to swap buffers"),
                            );
                    }

                    integration.post_present(&win.read().window.as_ref().unwrap().read());

                    #[cfg(feature = "__screenshot")]
                    // give it time to settle:
                    if integration.egui_ctx.frame_nr() == 2 {
                        if let Ok(path) = std::env::var("EFRAME_SCREENSHOT_TO") {
                            assert!(
                                path.ends_with(".png"),
                                "Expected EFRAME_SCREENSHOT_TO to end with '.png', got {path:?}"
                            );
                            let screenshot = painter.read().read_screen_rgba(screen_size_in_pixels);
                            image::save_buffer(
                                &path,
                                screenshot.as_raw(),
                                screenshot.width() as u32,
                                screenshot.height() as u32,
                                image::ColorType::Rgba8,
                            )
                            .unwrap_or_else(|err| {
                                panic!("Failed to save screenshot to {path:?}: {err}");
                            });
                            eprintln!("Screenshot saved to {path:?}.");
                            std::process::exit(0);
                        }
                    }

                    control_flow = if integration.should_close() {
                        vec![EventResult::Exit]
                    } else {
                        repaint_after
                            .into_iter()
                            .filter_map(|(id, time)| {
                                if time.is_zero() {
                                    window_map.get(&id).map(|id| EventResult::RepaintNext(*id))
                                } else if let Some(repaint_after_instant) =
                                    std::time::Instant::now().checked_add(time)
                                {
                                    // if repaint_after is something huge and can't be added to Instant,
                                    // we will use `ControlFlow::Wait` instead.
                                    // technically, this might lead to some weird corner cases where the user *WANTS*
                                    // winit to use `WaitUntil(MAX_INSTANT)` explicitly. they can roll their own
                                    // egui backend impl i guess.

                                    window_map.get(&id).map(|id| {
                                        EventResult::RepaintAt(*id, repaint_after_instant)
                                    })
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<EventResult>>()
                    };

                    integration
                        .maybe_autosave(app.write().as_mut(), win.read().window.clone().unwrap());

                    if win.read().window.as_ref().unwrap().read().is_minimized() == Some(true) {
                        // On Mac, a minimized Window uses up all CPU:
                        // https://github.com/emilk/egui/issues/325
                        crate::profile_scope!("minimized_sleep");
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }

                Self::process_viewport_builders(&glutin, viewports);

                egui_winit::process_viewports_commands(
                    viewport_commands,
                    *self.is_focused.read(),
                    |viewport_id| {
                        glutin
                            .read()
                            .viewports
                            .get(&viewport_id)
                            .and_then(|w| w.read().window.clone())
                    },
                );

                control_flow
            } else {
                vec![EventResult::Wait]
            }
        }

        fn on_event(
            &mut self,
            event_loop: &EventLoopWindowTarget<UserEvent>,
            event: &winit::event::Event<'_, UserEvent>,
        ) -> Result<EventResult> {
            crate::profile_function!();

            Ok(match event {
                winit::event::Event::Resumed => {
                    // first resume event.
                    // we can actually move this outside of event loop.
                    // and just run the on_resume fn of gl_window
                    if self.running.read().is_none() {
                        self.init_run_state(event_loop)?;
                    } else {
                        // not the first resume event. create whatever you need.
                        self.running
                            .write()
                            .as_mut()
                            .unwrap()
                            .glutin_ctx
                            .write()
                            .on_resume(event_loop)?;
                    }
                    EventResult::RepaintNow(
                        self.running
                            .read()
                            .as_ref()
                            .unwrap()
                            .glutin_ctx
                            .read()
                            .window(ViewportId::MAIN)
                            .read()
                            .window
                            .as_ref()
                            .unwrap()
                            .read()
                            .id(),
                    )
                }
                winit::event::Event::Suspended => {
                    self.running
                        .write()
                        .as_mut()
                        .unwrap()
                        .glutin_ctx
                        .write()
                        .on_suspend()?;

                    EventResult::Wait
                }

                winit::event::Event::MainEventsCleared => {
                    if let Some(running) = self.running.read().as_ref() {
                        let _ = running.glutin_ctx.write().on_resume(event_loop);
                    }
                    EventResult::Wait
                }

                winit::event::Event::WindowEvent { event, window_id } => {
                    if let Some(running) = self.running.write().as_mut() {
                        // On Windows, if a window is resized by the user, it should repaint synchronously, inside the
                        // event handler.
                        //
                        // If this is not done, the compositor will assume that the window does not want to redraw,
                        // and continue ahead.
                        //
                        // In eframe's case, that causes the window to rapidly flicker, as it struggles to deliver
                        // new frames to the compositor in time.
                        //
                        // The flickering is technically glutin or glow's fault, but we should be responding properly
                        // to resizes anyway, as doing so avoids dropping frames.
                        //
                        // See: https://github.com/emilk/egui/issues/903
                        let mut repaint_asap = false;

                        match &event {
                            winit::event::WindowEvent::Focused(new_focused) => {
                                *self.is_focused.write() = new_focused
                                    .then(|| {
                                        running
                                            .glutin_ctx
                                            .write()
                                            .viewports_maps
                                            .get(window_id)
                                            .copied()
                                    })
                                    .flatten();
                            }
                            winit::event::WindowEvent::Resized(physical_size) => {
                                repaint_asap = true;

                                // Resize with 0 width and height is used by winit to signal a minimize event on Windows.
                                // See: https://github.com/rust-windowing/winit/issues/208
                                // This solves an issue where the app would panic when minimizing on Windows.
                                let glutin_ctx = &mut *running.glutin_ctx.write();
                                if 0 < physical_size.width && 0 < physical_size.height {
                                    if let Some(id) = glutin_ctx.viewports_maps.get(window_id) {
                                        glutin_ctx.resize(*id, *physical_size);
                                    }
                                }
                            }
                            winit::event::WindowEvent::ScaleFactorChanged {
                                new_inner_size,
                                ..
                            } => {
                                let glutin_ctx = &mut *running.glutin_ctx.write();
                                repaint_asap = true;
                                if let Some(id) = glutin_ctx.viewports_maps.get(window_id) {
                                    glutin_ctx.resize(*id, **new_inner_size);
                                }
                            }
                            winit::event::WindowEvent::CloseRequested
                                if running
                                    .glutin_ctx
                                    .write()
                                    .viewports
                                    .iter()
                                    .filter_map(|(_, window)| {
                                        if let Some(win) = window.read().window.as_ref() {
                                            let win = win.read();
                                            if win.id() == *window_id {
                                                Some(window.read().pair.this)
                                            } else {
                                                None
                                            }
                                        } else {
                                            None
                                        }
                                    })
                                    .filter_map(|id| {
                                        if id == ViewportId::MAIN {
                                            Some(())
                                        } else {
                                            None
                                        }
                                    })
                                    .count()
                                    == 1
                                    && running.integration.read().should_close() =>
                            {
                                log::debug!("Received WindowEvent::CloseRequested");
                                return Ok(EventResult::Exit);
                            }
                            _ => {}
                        }

                        let event_response = 'res: {
                            let glutin_ctx = running.glutin_ctx.read();
                            if let Some(viewport_id) =
                                glutin_ctx.viewports_maps.get(window_id).copied()
                            {
                                if let Some(viewport) =
                                    glutin_ctx.viewports.get(&viewport_id).cloned()
                                {
                                    let viewport = &mut *viewport.write();

                                    break 'res running.integration.write().on_event(
                                        running.app.write().as_mut(),
                                        event,
                                        viewport.egui_winit.as_mut().unwrap(),
                                        viewport.pair.this,
                                    );
                                }
                            }

                            EventResponse {
                                consumed: false,
                                repaint: false,
                            }
                        };

                        if running.integration.read().should_close() {
                            EventResult::Exit
                        } else if event_response.repaint {
                            if repaint_asap {
                                EventResult::RepaintNow(*window_id)
                            } else {
                                EventResult::RepaintNext(*window_id)
                            }
                        } else {
                            EventResult::Wait
                        }
                    } else {
                        EventResult::Wait
                    }
                }

                #[cfg(feature = "accesskit")]
                winit::event::Event::UserEvent(UserEvent::AccessKitActionRequest(
                    accesskit_winit::ActionRequestEvent { request, window_id },
                )) => {
                    if let Some(running) = self.running.read().as_ref() {
                        crate::profile_scope!("on_accesskit_action_request");

                        let glutin_ctx = running.glutin_ctx.read();
                        if let Some(viewport_id) = glutin_ctx.viewports_maps.get(window_id).copied()
                        {
                            if let Some(viewport) = glutin_ctx.viewports.get(&viewport_id).cloned()
                            {
                                let mut viewport = viewport.write();
                                viewport
                                    .egui_winit
                                    .as_mut()
                                    .unwrap()
                                    .on_accesskit_action_request(request.clone());
                            }
                        }
                        // As a form of user input, accessibility actions should
                        // lead to a repaint.
                        EventResult::RepaintNext(*window_id)
                    } else {
                        EventResult::Wait
                    }
                }
                _ => EventResult::Wait,
            })
        }
    }

    pub fn run_glow(
        app_name: &str,
        mut native_options: epi::NativeOptions,
        app_creator: epi::AppCreator,
    ) -> Result<()> {
        #[cfg(not(target_os = "ios"))]
        if native_options.run_and_return {
            with_event_loop(native_options, |event_loop, native_options| {
                let glow_eframe =
                    GlowWinitApp::new(event_loop, app_name, native_options, app_creator);
                run_and_return(event_loop, glow_eframe)
            })
        } else {
            let event_loop = create_event_loop(&mut native_options);
            let glow_eframe = GlowWinitApp::new(&event_loop, app_name, native_options, app_creator);
            run_and_exit(event_loop, glow_eframe);
        }

        #[cfg(target_os = "ios")]
        {
            let event_loop = create_event_loop(&mut native_options);
            let glow_eframe = GlowWinitApp::new(&event_loop, app_name, native_options, app_creator);
            run_and_exit(event_loop, glow_eframe);
        }
    }
}

#[cfg(feature = "glow")]
pub use glow_integration::run_glow;
// ----------------------------------------------------------------------------

#[cfg(feature = "wgpu")]
mod wgpu_integration {
    use std::sync::Arc;

    use egui::{ViewportIdPair, ViewportOutput, ViewportRender};
    use egui_winit::create_winit_window_builder;
    use parking_lot::Mutex;

    use super::*;

    #[derive(Clone)]
    pub struct Window {
        window: Option<Arc<RwLock<winit::window::Window>>>,
        state: Arc<RwLock<Option<egui_winit::State>>>,
        render: Option<Arc<Box<ViewportRender>>>,
        parent_id: ViewportId,
    }

    #[derive(Clone)]
    pub struct Viewports(Arc<RwLock<HashMap<ViewportId, Window>>>);

    #[allow(unsafe_code)]
    unsafe impl Send for Viewports {}
    #[allow(unsafe_code)]
    unsafe impl Sync for Viewports {}

    impl std::ops::Deref for Viewports {
        type Target = Arc<RwLock<HashMap<ViewportId, Window>>>;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    /// State that is initialized when the application is first starts running via
    /// a Resumed event. On Android this ensures that any graphics state is only
    /// initialized once the application has an associated `SurfaceView`.
    struct WgpuWinitRunning {
        painter: Arc<RwLock<egui_wgpu::winit::Painter>>,
        integration: Arc<RwLock<epi_integration::EpiIntegration>>,
        app: Box<dyn epi::App>,
        viewports: Viewports,
        builders: Arc<RwLock<HashMap<ViewportId, ViewportBuilder>>>,
        windows_id: Arc<RwLock<HashMap<winit::window::WindowId, ViewportId>>>,
    }

    struct WgpuWinitApp {
        repaint_proxy: Arc<Mutex<EventLoopProxy<UserEvent>>>,
        app_name: String,
        native_options: epi::NativeOptions,
        app_creator: Option<epi::AppCreator>,
        running: Option<WgpuWinitRunning>,

        /// Window surface state that's initialized when the app starts running via a Resumed event
        /// and on Android will also be destroyed if the application is paused.
        is_focused: Arc<RwLock<Option<ViewportId>>>,
    }

    impl WgpuWinitApp {
        fn new(
            event_loop: &EventLoop<UserEvent>,
            app_name: &str,
            native_options: epi::NativeOptions,
            app_creator: epi::AppCreator,
        ) -> Self {
            crate::profile_function!();
            #[cfg(feature = "__screenshot")]
            assert!(
                std::env::var("EFRAME_SCREENSHOT_TO").is_err(),
                "EFRAME_SCREENSHOT_TO not yet implemented for wgpu backend"
            );

            Self {
                repaint_proxy: Arc::new(Mutex::new(event_loop.create_proxy())),
                app_name: app_name.to_owned(),
                native_options,
                running: None,
                app_creator: Some(app_creator),
                is_focused: Arc::new(RwLock::new(Some(ViewportId::MAIN))),
            }
        }

        fn create_window(
            event_loop: &EventLoopWindowTarget<UserEvent>,
            storage: Option<&dyn epi::Storage>,
            title: &str,
            native_options: &NativeOptions,
        ) -> std::result::Result<(winit::window::Window, ViewportBuilder), winit::error::OsError>
        {
            crate::profile_function!();

            let window_settings = epi_integration::load_window_settings(storage);
            let window_builder =
                epi_integration::window_builder(event_loop, title, native_options, window_settings);
            let window = {
                crate::profile_scope!("WindowBuilder::build");
                create_winit_window_builder(&window_builder).build(event_loop)?
            };
            epi_integration::apply_native_options_to_window(
                &window,
                native_options,
                window_settings,
            );
            Ok((window, window_builder))
        }

        fn build_windows(&mut self, event_loop: &EventLoopWindowTarget<UserEvent>) {
            let Some(running) = &mut self.running else {return};
            let viewport_builders = running.builders.read();

            for (id, Window { window, state, .. }) in running.viewports.write().iter_mut() {
                let builder = viewport_builders.get(id).unwrap();
                if window.is_some() {
                    continue;
                }

                Self::init_window(
                    *id,
                    builder,
                    &mut running.windows_id.write(),
                    &running.painter,
                    window,
                    state,
                    event_loop,
                );
            }
        }

        fn init_window(
            id: ViewportId,
            builder: &ViewportBuilder,
            windows_id: &mut HashMap<winit::window::WindowId, ViewportId>,
            painter: &Arc<RwLock<egui_wgpu::winit::Painter>>,
            window: &mut Option<Arc<RwLock<winit::window::Window>>>,
            state: &Arc<RwLock<Option<egui_winit::State>>>,
            event_loop: &EventLoopWindowTarget<UserEvent>,
        ) {
            if let Ok(new_window) = create_winit_window_builder(builder).build(event_loop) {
                windows_id.insert(new_window.id(), id);

                if let Err(err) =
                    pollster::block_on(painter.write().set_window(id, Some(&new_window)))
                {
                    log::error!("on set_window: viewport_id {id} {err}");
                }
                *window = Some(Arc::new(RwLock::new(new_window)));
                *state.write() = Some(egui_winit::State::new(event_loop));
            }
        }

        fn set_window(&mut self, id: ViewportId) -> std::result::Result<(), egui_wgpu::WgpuError> {
            if let Some(running) = &mut self.running {
                crate::profile_function!();
                if let Some(Window { window, .. }) = running.viewports.read().get(&id) {
                    let window = window.clone();
                    if let Some(win) = &window {
                        return pollster::block_on(
                            running.painter.write().set_window(id, Some(&*win.read())),
                        );
                    } else {
                        return pollster::block_on(running.painter.write().set_window(id, None));
                    };
                }
            }
            Ok(())
        }

        #[allow(unsafe_code)]
        #[cfg(target_os = "android")]
        fn drop_window(&mut self) -> std::result::Result<(), egui_wgpu::WgpuError> {
            if let Some(running) = &mut self.running {
                running.viewports.write().remove(&ViewportId::MAIN);
                pollster::block_on(running.painter.write().set_window(ViewportId::MAIN, None))?;
            }
            Ok(())
        }

        fn init_run_state(
            &mut self,
            event_loop: &EventLoopWindowTarget<UserEvent>,
            storage: Option<Box<dyn epi::Storage>>,
            window: winit::window::Window,
            builder: ViewportBuilder,
        ) -> std::result::Result<(), egui_wgpu::WgpuError> {
            crate::profile_function!();

            #[allow(unsafe_code, unused_mut, unused_unsafe)]
            let mut painter = egui_wgpu::winit::Painter::new(
                self.native_options.wgpu_options.clone(),
                self.native_options.multisampling.max(1) as _,
                egui_wgpu::depth_format_from_bits(
                    self.native_options.depth_buffer,
                    self.native_options.stencil_buffer,
                ),
                self.native_options.transparent,
            );
            pollster::block_on(painter.set_window(ViewportId::MAIN, Some(&window)))?;

            let wgpu_render_state = painter.render_state();

            let system_theme = system_theme(&window, &self.native_options);
            let mut integration = epi_integration::EpiIntegration::new(
                &window,
                system_theme,
                &self.app_name,
                &self.native_options,
                storage,
                IS_DESKTOP,
                #[cfg(feature = "glow")]
                None,
                wgpu_render_state.clone(),
            );

            let mut state = egui_winit::State::new(event_loop);
            #[cfg(feature = "accesskit")]
            {
                integration.init_accesskit(&mut state, &window, self.repaint_proxy.lock().clone());
            }
            let theme = system_theme.unwrap_or(self.native_options.default_theme);
            integration.egui_ctx.set_visuals(theme.egui_visuals());

            {
                let event_loop_proxy = self.repaint_proxy.clone();

                integration
                    .egui_ctx
                    .set_request_repaint_callback(move |info| {
                        log::trace!("request_repaint_callback: {info:?}");
                        let when = Instant::now() + info.after;
                        let frame_nr = info.current_frame_nr;

                        event_loop_proxy
                            .lock()
                            .send_event(UserEvent::RequestRepaint {
                                when,
                                frame_nr,
                                id: info.viewport_id,
                            })
                            .ok();
                    });
            }

            let app_creator = std::mem::take(&mut self.app_creator)
                .expect("Single-use AppCreator has unexpectedly already been taken");
            let cc = epi::CreationContext {
                egui_ctx: integration.egui_ctx.clone(),
                integration_info: integration.frame.info().clone(),
                storage: integration.frame.storage(),
                #[cfg(feature = "glow")]
                gl: None,
                wgpu_render_state,
                raw_display_handle: window.raw_display_handle(),
                raw_window_handle: window.raw_window_handle(),
            };
            let mut app = {
                crate::profile_scope!("user_app_creator");
                app_creator(&cc)
            };

            if app.warm_up_enabled() {
                integration.warm_up(app.as_mut(), &window, &mut state);
            }

            let mut windows_id = HashMap::default();
            windows_id.insert(window.id(), ViewportId::MAIN);
            let windows_id = Arc::new(RwLock::new(windows_id));

            let viewports = Viewports(Arc::new(RwLock::new(HashMap::default())));
            viewports.write().insert(
                ViewportId::MAIN,
                Window {
                    window: Some(Arc::new(RwLock::new(window))),
                    state: Arc::new(RwLock::new(Some(state))),
                    render: None,
                    parent_id: ViewportId::MAIN,
                },
            );

            let builders = Arc::new(RwLock::new(HashMap::default()));
            builders.write().insert(ViewportId::MAIN, builder);

            let painter = Arc::new(RwLock::new(painter));

            // c_* means that will be taken by the next closure

            let c_viewports = viewports.clone();
            let c_builders = builders.clone();
            let c_time = integration.beginning;
            let c_painter = painter.clone();
            let c_windows_id = windows_id.clone();

            // ## Sync Rendering
            integration.egui_ctx.set_render_sync_callback(
                move |egui_ctx, mut viewport_builder, ViewportIdPair{ this: viewport_id, parent: parent_id }, render| {
                    // Creating a new native window
                    if c_viewports.read().get(&viewport_id).is_none(){
                        let mut _windows = c_viewports.write();

                        {
                            let builders = c_builders.read();
                            if viewport_builder.icon.is_none() && builders.get(&viewport_id).is_none(){
                                viewport_builder.icon = builders.get(&parent_id).unwrap().icon.clone();
                            }
                        }

                        let Window{window, state, ..} = _windows.entry(viewport_id).or_insert(Window{window: None, state: Arc::new(RwLock::new(None)), render: None, parent_id });
                        let _ = c_builders.write().entry(viewport_id).or_insert(viewport_builder.clone());

                        let event_loop;

                        #[allow(unsafe_code)]
                        unsafe{
                            event_loop = WINIT_EVENT_LOOP.with(|event_loop|event_loop.read().as_ref().unwrap());
                        }
                        Self::init_window(viewport_id, &viewport_builder, &mut c_windows_id.write(), &c_painter, window, state, event_loop);
                    }
                    'try_render: {
                        let window = c_viewports.read().get(&viewport_id).cloned();
                        if let Some(window) = window {
                            let output;
                            {
                                if let Some(winit_state) = &mut *window.state.write() {
                                    if let Some(win) = window.window {
                                        let win = win.read();
                                        let mut input = winit_state.take_egui_input(&win);
                                        input.time = Some(c_time.elapsed().as_secs_f64());
                                        output = egui_ctx.run(
                                            input,
                                            ViewportIdPair::new(viewport_id, parent_id),
                                            |ctx| {
                                                render(ctx);
                                            },
                                        );

                                        if let Err(err) = pollster::block_on(
                                            c_painter.write().set_window(viewport_id, Some(&win)),
                                        ){
                                            log::error!("when rendering viewport_id: {viewport_id}, set_window Error {err}");
                                        }

                                        let clipped_primitives = egui_ctx.tessellate(output.shapes);
                                        c_painter.write().paint_and_update_textures(
                                            viewport_id,
                                            egui_ctx.pixels_per_point(),
                                            [0.0, 0.0, 0.0, 0.0],
                                            &clipped_primitives,
                                            &output.textures_delta,
                                            false,
                                        );

                                        winit_state.handle_platform_output(
                                            &win,
                                            egui_ctx,
                                            output.platform_output,
                                        );
                                    } else {
                                        break 'try_render;
                                    }
                                } else {
                                    break 'try_render;
                                }
                            }
                        }
                    }
                },
            );

            self.running = Some(WgpuWinitRunning {
                painter,
                integration: Arc::new(RwLock::new(integration)),
                app,
                viewports,
                windows_id,
                builders,
            });

            Ok(())
        }
    }

    impl WinitApp for WgpuWinitApp {
        fn frame_nr(&self) -> u64 {
            self.running
                .as_ref()
                .map_or(0, |r| r.integration.read().egui_ctx.frame_nr())
        }

        fn is_focused(&self, window_id: winit::window::WindowId) -> bool {
            if let Some(focus) = *self.is_focused.read() {
                self.get_window_id(&window_id).map_or(false, |i| i == focus)
            } else {
                false
            }
        }

        fn integration(&self) -> Option<Arc<RwLock<EpiIntegration>>> {
            self.running.as_ref().map(|r| r.integration.clone())
        }

        fn window(
            &self,
            window_id: winit::window::WindowId,
        ) -> Option<Arc<RwLock<winit::window::Window>>> {
            self.running
                .as_ref()
                .and_then(|r| {
                    r.windows_id
                        .read()
                        .get(&window_id)
                        .and_then(|id| r.viewports.read().get(id).map(|w| w.window.clone()))
                })
                .flatten()
        }

        fn get_window_winit_id(&self, id: ViewportId) -> Option<winit::window::WindowId> {
            self.running.as_ref().and_then(|r| {
                r.viewports
                    .read()
                    .get(&id)
                    .and_then(|w| w.window.as_ref().map(|w| w.read().id()))
            })
        }

        fn save_and_destroy(&mut self) {
            if let Some(mut running) = self.running.take() {
                crate::profile_function!();
                if let Some(Window { window, .. }) = running.viewports.read().get(&ViewportId::MAIN)
                {
                    running
                        .integration
                        .write()
                        .save(running.app.as_mut(), window.clone());
                }

                #[cfg(feature = "glow")]
                running.app.on_exit(None);

                #[cfg(not(feature = "glow"))]
                running.app.on_exit();

                running.painter.write().destroy();
            }
        }

        fn run_ui_and_paint(&mut self, window_id: winit::window::WindowId) -> Vec<EventResult> {
            if let Some(running) = &mut self.running {
                #[cfg(feature = "puffin")]
                puffin::GlobalProfiler::lock().new_frame();
                crate::profile_scope!("frame");

                let WgpuWinitRunning {
                    app,
                    integration,
                    painter,
                    viewports: windows,
                    windows_id,
                    builders: viewport_builders,
                } = running;

                let egui::FullOutput {
                    platform_output,
                    repaint_after,
                    textures_delta,
                    shapes,
                    mut viewports,
                    viewport_commands,
                };
                {
                    let Some((viewport_id, Window{window: Some(window), state, render, parent_id })) = windows_id.read().get(&window_id).and_then(|id|(windows.read().get(id).map(|w|(*id, w.clone())))) else{return vec![]};
                    // This is used to not render a viewport if is sync
                    if viewport_id != ViewportId::MAIN && render.is_none() {
                        if let Some(window) = running.viewports.read().get(&parent_id) {
                            if let Some(w) = window.window.as_ref() {
                                return vec![EventResult::RepaintNow(w.read().id())];
                            }
                        }
                        return vec![];
                    }

                    let _ = pollster::block_on(
                        painter
                            .write()
                            .set_window(viewport_id, Some(&window.read())),
                    );

                    egui::FullOutput {
                        platform_output,
                        repaint_after,
                        textures_delta,
                        shapes,
                        viewports,
                        viewport_commands,
                    } = integration.write().update(
                        app.as_mut(),
                        &window.read(),
                        state.write().as_mut().unwrap(),
                        &render.clone(),
                        ViewportIdPair::new(viewport_id, parent_id),
                    );

                    integration.write().handle_platform_output(
                        &window.read(),
                        platform_output,
                        state.write().as_mut().unwrap(),
                    );

                    let clipped_primitives = {
                        crate::profile_scope!("tessellate");
                        integration.read().egui_ctx.tessellate(shapes)
                    };

                    let integration = &mut *integration.write();
                    let screenshot_requested = &mut integration.frame.output.screenshot_requested;

                    let screenshot = painter.write().paint_and_update_textures(
                        viewport_id,
                        integration.egui_ctx.pixels_per_point(),
                        app.clear_color(&integration.egui_ctx.style().visuals),
                        &clipped_primitives,
                        &textures_delta,
                        *screenshot_requested,
                    );
                    *screenshot_requested = false;
                    integration.frame.screenshot.set(screenshot);

                    integration.post_rendering(app.as_mut(), &window.read());
                    integration.post_present(&window.read());
                }

                let mut active_viewports_ids = vec![ViewportId::MAIN];

                viewports.retain_mut(
                    |ViewportOutput {
                         pair: ViewportIdPair { this: id, parent },
                         render,
                         ..
                     }| {
                        if let Some(w) = windows.write().get_mut(id) {
                            w.render = render.clone();
                            w.parent_id = *parent;
                            active_viewports_ids.push(*id);
                            false
                        } else {
                            true
                        }
                    },
                );

                for ViewportOutput {
                    mut builder,
                    pair:
                        ViewportIdPair {
                            this: id,
                            parent: parent_id,
                        },
                    render,
                } in viewports
                {
                    if builder.icon.is_none() {
                        builder.icon = viewport_builders
                            .write()
                            .get_mut(&parent_id)
                            .and_then(|w| w.icon.clone());
                    }

                    windows.write().insert(
                        id,
                        Window {
                            window: None,
                            state: Arc::new(RwLock::new(None)),
                            render,
                            parent_id,
                        },
                    );
                    viewport_builders.write().insert(id, builder);
                    active_viewports_ids.push(id);
                }

                egui_winit::process_viewports_commands(
                    viewport_commands,
                    *self.is_focused.read(),
                    |viewport_id| {
                        windows
                            .read()
                            .get(&viewport_id)
                            .and_then(|w| w.window.clone())
                    },
                );

                windows
                    .write()
                    .retain(|id, _| active_viewports_ids.contains(id));
                windows_id
                    .write()
                    .retain(|_, id| active_viewports_ids.contains(id));
                painter.write().clean_surfaces(&active_viewports_ids);

                let mut control_flow = vec![EventResult::Wait];
                for repaint_after in repaint_after {
                    control_flow.push(if integration.read().should_close() {
                        EventResult::Exit
                    } else if repaint_after.1.is_zero() {
                        if let Some(Window {
                            window: Some(window),
                            ..
                        }) = windows.read().get(&repaint_after.0)
                        {
                            EventResult::RepaintNext(window.read().id())
                        } else {
                            EventResult::Wait
                        }
                    } else if let Some(repaint_after_instant) =
                        std::time::Instant::now().checked_add(repaint_after.1)
                    {
                        // if repaint_after is something huge and can't be added to Instant,
                        // we will use `ControlFlow::Wait` instead.
                        // technically, this might lead to some weird corner cases where the user *WANTS*
                        // winit to use `WaitUntil(MAX_INSTANT)` explicitly. they can roll their own
                        // egui backend impl i guess.
                        if let Some(Window {
                            window: Some(window),
                            ..
                        }) = windows.read().get(&repaint_after.0)
                        {
                            EventResult::RepaintAt(window.read().id(), repaint_after_instant)
                        } else {
                            EventResult::Wait
                        }
                    } else {
                        EventResult::Wait
                    });
                }

                let Some((_, Window{window: Some(window), ..})) = windows_id.read().get(&window_id).and_then(|id|(windows.read().get(id).map(|w|(*id, w.clone())))) else{return vec![]};
                integration
                    .write()
                    .maybe_autosave(app.as_mut(), window.clone());

                if window.read().is_minimized() == Some(true) {
                    // On Mac, a minimized Window uses up all CPU:
                    // https://github.com/emilk/egui/issues/325
                    crate::profile_scope!("minimized_sleep");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }

                control_flow
            } else {
                vec![EventResult::Wait]
            }
        }

        fn on_event(
            &mut self,
            event_loop: &EventLoopWindowTarget<UserEvent>,
            event: &winit::event::Event<'_, UserEvent>,
        ) -> Result<EventResult> {
            crate::profile_function!();
            self.build_windows(event_loop);

            Ok(match event {
                winit::event::Event::Resumed => {
                    if let Some(running) = &self.running {
                        if running.viewports.read().get(&ViewportId::MAIN).is_none() {
                            let _ = Self::create_window(
                                event_loop,
                                running.integration.read().frame.storage(),
                                &self.app_name,
                                &self.native_options,
                            )?;
                            self.set_window(ViewportId::MAIN)?;
                        }
                    } else {
                        let storage = epi_integration::create_storage(
                            self.native_options
                                .app_id
                                .as_ref()
                                .unwrap_or(&self.app_name),
                        );
                        let (window, builder) = Self::create_window(
                            event_loop,
                            storage.as_deref(),
                            &self.app_name,
                            &self.native_options,
                        )?;
                        self.init_run_state(event_loop, storage, window, builder)?;
                    }
                    EventResult::RepaintNow(
                        self.running
                            .as_ref()
                            .unwrap()
                            .viewports
                            .read()
                            .get(&ViewportId::MAIN)
                            .unwrap()
                            .window
                            .as_ref()
                            .unwrap()
                            .read()
                            .id(),
                    )
                }
                winit::event::Event::Suspended => {
                    #[cfg(target_os = "android")]
                    self.drop_window()?;
                    EventResult::Wait
                }

                winit::event::Event::WindowEvent { event, window_id } => {
                    let viewport_id = self.get_window_id(window_id);
                    if let Some(running) = &mut self.running {
                        // On Windows, if a window is resized by the user, it should repaint synchronously, inside the
                        // event handler.
                        //
                        // If this is not done, the compositor will assume that the window does not want to redraw,
                        // and continue ahead.
                        //
                        // In eframe's case, that causes the window to rapidly flicker, as it struggles to deliver
                        // new frames to the compositor in time.
                        //
                        // The flickering is technically glutin or glow's fault, but we should be responding properly
                        // to resizes anyway, as doing so avoids dropping frames.
                        //
                        // See: https://github.com/emilk/egui/issues/903
                        let mut repaint_asap = false;

                        match &event {
                            winit::event::WindowEvent::Focused(new_focused) => {
                                *self.is_focused.write() =
                                    new_focused.then(|| viewport_id).flatten();
                            }
                            winit::event::WindowEvent::Resized(physical_size) => {
                                repaint_asap = true;

                                // Resize with 0 width and height is used by winit to signal a minimize event on Windows.
                                // See: https://github.com/rust-windowing/winit/issues/208
                                // This solves an issue where the app would panic when minimizing on Windows.
                                if let Some(viewport_id) =
                                    running.windows_id.read().get(window_id).copied()
                                {
                                    if 0 < physical_size.width && 0 < physical_size.height {
                                        running.painter.write().on_window_resized(
                                            viewport_id,
                                            physical_size.width,
                                            physical_size.height,
                                        );
                                    }
                                }
                            }
                            winit::event::WindowEvent::ScaleFactorChanged {
                                new_inner_size,
                                ..
                            } => {
                                if let Some(viewport_id) =
                                    running.windows_id.read().get(window_id).copied()
                                {
                                    repaint_asap = true;
                                    running.painter.write().on_window_resized(
                                        viewport_id,
                                        new_inner_size.width,
                                        new_inner_size.height,
                                    );
                                }
                            }
                            winit::event::WindowEvent::CloseRequested
                                if running.integration.read().should_close() =>
                            {
                                log::debug!("Received WindowEvent::CloseRequested");
                                return Ok(EventResult::Exit);
                            }
                            _ => {}
                        };

                        let event_response = if let Some((id, Window { state, .. })) =
                            running.windows_id.read().get(window_id).and_then(|id| {
                                running.viewports.read().get(id).map(|w| (*id, w.clone()))
                            }) {
                            if let Some(state) = &mut *state.write() {
                                Some(running.integration.write().on_event(
                                    running.app.as_mut(),
                                    event,
                                    state,
                                    id,
                                ))
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                        if running.integration.read().should_close() {
                            EventResult::Exit
                        } else if let Some(event_response) = event_response {
                            if event_response.repaint {
                                if repaint_asap {
                                    EventResult::RepaintNow(*window_id)
                                } else {
                                    EventResult::RepaintNext(*window_id)
                                }
                            } else {
                                EventResult::Wait
                            }
                        } else {
                            EventResult::Wait
                        }
                    } else {
                        EventResult::Wait
                    }
                }
                #[cfg(feature = "accesskit")]
                winit::event::Event::UserEvent(UserEvent::AccessKitActionRequest(
                    accesskit_winit::ActionRequestEvent { request, window_id },
                )) => {
                    if let Some(running) = &mut self.running {
                        if let Some(Window { state, .. }) = running
                            .windows_id
                            .read()
                            .get(window_id)
                            .and_then(|id| running.viewports.read().get(id).cloned())
                        {
                            if let Some(state) = &mut *state.write() {
                                state.on_accesskit_action_request(request.clone());
                            }
                        }
                        // As a form of user input, accessibility actions should
                        // lead to a repaint.
                        EventResult::RepaintNext(*window_id)
                    } else {
                        EventResult::Wait
                    }
                }
                _ => EventResult::Wait,
            })
        }

        fn get_window_id(&self, id: &winit::window::WindowId) -> Option<ViewportId> {
            self.running
                .as_ref()
                .and_then(|r| r.windows_id.read().get(id).copied())
        }
    }

    pub fn run_wgpu(
        app_name: &str,
        mut native_options: epi::NativeOptions,
        app_creator: epi::AppCreator,
    ) -> Result<()> {
        #[cfg(not(target_os = "ios"))]
        if native_options.run_and_return {
            with_event_loop(native_options, |event_loop, native_options| {
                let wgpu_eframe =
                    WgpuWinitApp::new(event_loop, app_name, native_options, app_creator);
                run_and_return(event_loop, wgpu_eframe)
            })
        } else {
            let event_loop = create_event_loop(&mut native_options);
            let wgpu_eframe = WgpuWinitApp::new(&event_loop, app_name, native_options, app_creator);
            run_and_exit(event_loop, wgpu_eframe);
        }

        #[cfg(target_os = "ios")]
        {
            let event_loop = create_event_loop(&mut native_options);
            let wgpu_eframe = WgpuWinitApp::new(&event_loop, app_name, native_options, app_creator);
            run_and_exit(event_loop, wgpu_eframe);
        }
    }
}

#[cfg(feature = "wgpu")]
pub use wgpu_integration::run_wgpu;

// ----------------------------------------------------------------------------

fn system_theme(window: &winit::window::Window, options: &NativeOptions) -> Option<crate::Theme> {
    if options.follow_system_theme {
        window
            .theme()
            .map(super::epi_integration::theme_from_winit_theme)
    } else {
        None
    }
}

// For the puffin profiler!
#[allow(dead_code)] // Only used for profiling
fn short_event_description(event: &winit::event::Event<'_, UserEvent>) -> &'static str {
    use winit::event::{DeviceEvent, Event, StartCause, WindowEvent};

    match event {
        Event::Suspended => "Event::Suspended",
        Event::Resumed => "Event::Resumed",
        Event::MainEventsCleared => "Event::MainEventsCleared",
        Event::RedrawRequested(_) => "Event::RedrawRequested",
        Event::RedrawEventsCleared => "Event::RedrawEventsCleared",
        Event::LoopDestroyed => "Event::LoopDestroyed",
        Event::UserEvent(user_event) => match user_event {
            UserEvent::RequestRepaint { .. } => "UserEvent::RequestRepaint",
            #[cfg(feature = "accesskit")]
            UserEvent::AccessKitActionRequest(_) => "UserEvent::AccessKitActionRequest",
        },
        Event::DeviceEvent { event, .. } => match event {
            DeviceEvent::Added { .. } => "DeviceEvent::Added",
            DeviceEvent::Removed { .. } => "DeviceEvent::Removed",
            DeviceEvent::MouseMotion { .. } => "DeviceEvent::MouseMotion",
            DeviceEvent::MouseWheel { .. } => "DeviceEvent::MouseWheel",
            DeviceEvent::Motion { .. } => "DeviceEvent::Motion",
            DeviceEvent::Button { .. } => "DeviceEvent::Button",
            DeviceEvent::Key { .. } => "DeviceEvent::Key",
            DeviceEvent::Text { .. } => "DeviceEvent::Text",
        },
        Event::NewEvents(start_cause) => match start_cause {
            StartCause::ResumeTimeReached { .. } => "NewEvents::ResumeTimeReached",
            StartCause::WaitCancelled { .. } => "NewEvents::WaitCancelled",
            StartCause::Poll => "NewEvents::Poll",
            StartCause::Init => "NewEvents::Init",
        },
        Event::WindowEvent { event, .. } => match event {
            WindowEvent::Resized { .. } => "WindowEvent::Resized",
            WindowEvent::Moved { .. } => "WindowEvent::Moved",
            WindowEvent::CloseRequested { .. } => "WindowEvent::CloseRequested",
            WindowEvent::Destroyed { .. } => "WindowEvent::Destroyed",
            WindowEvent::DroppedFile { .. } => "WindowEvent::DroppedFile",
            WindowEvent::HoveredFile { .. } => "WindowEvent::HoveredFile",
            WindowEvent::HoveredFileCancelled { .. } => "WindowEvent::HoveredFileCancelled",
            WindowEvent::ReceivedCharacter { .. } => "WindowEvent::ReceivedCharacter",
            WindowEvent::Focused { .. } => "WindowEvent::Focused",
            WindowEvent::KeyboardInput { .. } => "WindowEvent::KeyboardInput",
            WindowEvent::ModifiersChanged { .. } => "WindowEvent::ModifiersChanged",
            WindowEvent::Ime { .. } => "WindowEvent::Ime",
            WindowEvent::CursorMoved { .. } => "WindowEvent::CursorMoved",
            WindowEvent::CursorEntered { .. } => "WindowEvent::CursorEntered",
            WindowEvent::CursorLeft { .. } => "WindowEvent::CursorLeft",
            WindowEvent::MouseWheel { .. } => "WindowEvent::MouseWheel",
            WindowEvent::MouseInput { .. } => "WindowEvent::MouseInput",
            WindowEvent::TouchpadMagnify { .. } => "WindowEvent::TouchpadMagnify",
            WindowEvent::SmartMagnify { .. } => "WindowEvent::SmartMagnify",
            WindowEvent::TouchpadRotate { .. } => "WindowEvent::TouchpadRotate",
            WindowEvent::TouchpadPressure { .. } => "WindowEvent::TouchpadPressure",
            WindowEvent::AxisMotion { .. } => "WindowEvent::AxisMotion",
            WindowEvent::Touch { .. } => "WindowEvent::Touch",
            WindowEvent::ScaleFactorChanged { .. } => "WindowEvent::ScaleFactorChanged",
            WindowEvent::ThemeChanged { .. } => "WindowEvent::ThemeChanged",
            WindowEvent::Occluded { .. } => "WindowEvent::Occluded",
        },
    }
}
