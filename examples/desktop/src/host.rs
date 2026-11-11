// The window: winit's event loop on the main thread, and the render thread
// (view.rs) that owns the map and presents.
//
// The main thread only turns window events into `Message`s for the render
// thread and carries out what the render thread asks of the window (a
// surface, its title, its size, egui's cursor and links). Frames wait for
// the display on the render thread: on macOS, waiting in
// `-[CAMetalLayer nextDrawable]` on the main thread holds up the window's
// input with it, and a trackpad's first moves arrive ~100 ms late
// (rust-windowing/winit#1737).
//
// Every event goes to egui (egui-winit), whose input the render thread's
// controls (ui.rs) take. The map gets the pointer's events where the
// controls weren't, as of egui's last frame: a press over a control is the
// control's, and a press on the map keeps the pointer the map's until it is
// released, wherever it moves.
//
// Positions reach the map in its logical pixels: physical pixels divided by
// the pixel ratio the map was created with. The map keeps that ratio when the
// window moves to a display with another scale factor; egui follows the
// window's.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, mpsc};
use std::time::{Duration, Instant};

use mln_actor::RunLoopHandle;
use mln_gestures::{Button, Input, Modifiers, PinchPhase, PointerKind, WheelDelta};
use mln_util::geo::ScreenCoordinate;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::Args;
use crate::settings::Settings;
use crate::view::{self, Launch};

/// The window's size before it opens, in logical pixels (C++'s 1024x768).
const WINDOW_SIZE: LogicalSize<f64> = LogicalSize::new(1024.0, 768.0);

/// What the main thread hands the render thread, in the order it happened.
pub(crate) enum Message {
    /// Input for the gesture recognizer, in the map's logical pixels.
    Input {
        input: Input,
        at: Instant,
    },
    /// A key that the controls didn't take went down (or repeats) or up.
    Key {
        code: KeyCode,
        pressed: bool,
        modifiers: Modifiers,
        at: Instant,
    },
    /// The controls' input since the last.
    Ui(egui::RawInput),
    /// The window's new size in physical pixels.
    Resized(PhysicalSize<u32>),
    Focused {
        focused: bool,
        at: Instant,
    },
    /// The window was asked to close.
    Close,
}

/// What the render thread asks of the main thread.
pub(crate) enum UserEvent {
    /// A surface for the window, from this instance, sent back on the
    /// channel. The window's thread makes it.
    MakeSurface(wgpu::Instance, mpsc::Sender<Result<wgpu::Surface<'static>, String>>),
    SetTitle(String),
    RequestSize(LogicalSize<f64>),
    /// What egui asks of the window after a frame: its cursor, a link to open.
    Ui(Box<egui::PlatformOutput>),
    /// The render thread has dropped the map and ended.
    Ended,
}

/// The messages the render thread hasn't taken yet. Sending one wakes the
/// render thread's RunLoop.
#[derive(Default)]
pub(crate) struct Inbox {
    messages: Mutex<Vec<Message>>,
    waker: OnceLock<RunLoopHandle>,
}

impl Inbox {
    fn messages(&self) -> MutexGuard<'_, Vec<Message>> {
        self.messages.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn send(&self, message: Message) {
        self.messages().push(message);
        if let Some(waker) = self.waker.get() {
            waker.invoke(|| {});
        }
    }

    /// Wakes `run_loop` whenever a message arrives from now on.
    pub(crate) fn set_waker(&self, run_loop: RunLoopHandle) {
        let _ = self.waker.set(run_loop);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.messages().is_empty()
    }

    pub(crate) fn take(&self) -> Vec<Message> {
        std::mem::take(&mut *self.messages())
    }
}

/// A mouse button's key in `Open::buttons`.
fn button_key(button: Button) -> u8 {
    match button {
        Button::Primary => 0,
        Button::Secondary => 1,
        Button::Middle => 2,
    }
}

/// Opens the window and runs the app in it until it closes.
pub(crate) fn run(args: Args, settings: Settings) -> Result<(), winit::error::EventLoopError> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let mut host = Host { start: Some((args, settings)), proxy: event_loop.create_proxy(), open: None };
    event_loop.run_app(&mut host)
}

struct Host {
    /// What the render thread starts with, until the window opens.
    start: Option<(Args, Settings)>,
    proxy: EventLoopProxy<UserEvent>,
    open: Option<Open>,
}

/// The open window, on the main thread.
struct Open {
    window: Arc<Window>,
    inbox: Arc<Inbox>,
    egui: egui_winit::State,
    /// The map's pixel ratio: physical pixels per logical pixel.
    pixel_ratio: f64,
    /// Where the mouse is, in the map's logical pixels and in egui's points.
    cursor: ScreenCoordinate,
    cursor_points: egui::Pos2,
    /// The mouse buttons and touches that went down on the map.
    buttons: BTreeSet<u8>,
    touches: BTreeSet<u32>,
    modifiers: Modifiers,
    /// Whether the trackpad's pinch and rotation streams are under way:
    /// macOS phases each apart, and the map gets one pinch of both.
    pinching: bool,
    rotating: bool,
}

impl Open {
    fn logical(&self, position: PhysicalPosition<f64>) -> ScreenCoordinate {
        ScreenCoordinate::new(position.x / self.pixel_ratio, position.y / self.pixel_ratio)
    }

    /// Whether `position` is over one of the controls.
    fn over_controls(&self, position: PhysicalPosition<f64>) -> bool {
        let scale = self.window.scale_factor();
        crate::ui::over_controls(self.egui.egui_ctx(), (position.x / scale) as f32, (position.y / scale) as f32)
    }

    /// The controls' input so far, for the render thread.
    fn send_ui_input(&mut self, init: bool) {
        let mut input = self.egui.take_egui_input(&self.window);
        let viewport = input.viewports.entry(egui::ViewportId::ROOT).or_default();
        egui_winit::update_viewport_info(viewport, self.egui.egui_ctx(), &self.window, init);
        self.inbox.send(Message::Ui(input));
    }

    fn input(&self, input: Input) {
        self.inbox.send(Message::Input { input, at: Instant::now() });
    }

    /// One of the trackpad's two streams, as part of a pinch of both: the
    /// first to begin begins the pinch, and the last to end ends it.
    fn trackpad(&mut self, rotating: bool, scale: f64, rotation: f64, phase: TouchPhase) {
        let active = self.pinching || self.rotating;
        // A pinch that begins over the controls isn't the map's.
        let over_controls = egui_over(self.egui.egui_ctx(), self.cursor_points);
        let stream = if rotating { &mut self.rotating } else { &mut self.pinching };
        let phase = match phase {
            TouchPhase::Started if over_controls && !active => return,
            TouchPhase::Started => {
                *stream = true;
                if active {
                    return;
                }
                PinchPhase::Begin
            }
            TouchPhase::Moved if *stream => PinchPhase::Update,
            TouchPhase::Ended | TouchPhase::Cancelled if *stream => {
                *stream = false;
                if self.pinching || self.rotating {
                    return;
                }
                if phase == TouchPhase::Ended { PinchPhase::End } else { PinchPhase::Cancel }
            }
            _ => return,
        };
        self.input(Input::Pinch { center: self.cursor, scale, rotation, phase });
    }
}

fn egui_over(ctx: &egui::Context, at: egui::Pos2) -> bool {
    crate::ui::over_controls(ctx, at.x, at.y)
}

/// The recognizer's modifiers for winit's.
fn modifiers(state: winit::keyboard::ModifiersState) -> Modifiers {
    Modifiers { shift: state.shift_key(), ctrl: state.control_key(), alt: state.alt_key(), meta: state.super_key() }
}

/// The recognizer's pointer id for a touch: the mouse is 0.
fn touch_id(id: u64) -> u32 {
    (id % u64::from(u32::MAX)) as u32 + 1
}

impl ApplicationHandler<UserEvent> for Host {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let Some((args, settings)) = self.start.take() else {
            return;
        };
        let mut attributes = Window::default_attributes().with_title("mln-rs").with_inner_size(WINDOW_SIZE);
        if args.fullscreen {
            attributes = attributes.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(e) => {
                eprintln!("failed to initialize window: {e}");
                event_loop.exit();
                return;
            }
        };
        let pixel_ratio = window.scale_factor();
        // A frame per refresh of the display the window opens on.
        let refresh_hz = window
            .current_monitor()
            .and_then(|monitor| monitor.refresh_rate_millihertz())
            .map_or(60.0, |mhz| f64::from(mhz) / 1000.0);
        let inbox = Arc::new(Inbox::default());
        let ui = egui::Context::default();
        let egui = egui_winit::State::new(
            ui.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        let launch = Launch {
            ui,
            args,
            settings,
            pixel_ratio,
            size: window.inner_size(),
            frame_interval: Duration::from_secs_f64(1.0 / refresh_hz.max(1.0)),
            start: Instant::now(),
            inbox: inbox.clone(),
            proxy: self.proxy.clone(),
        };
        let mut open = Open {
            window,
            inbox,
            egui,
            pixel_ratio,
            cursor: ScreenCoordinate::default(),
            cursor_points: egui::Pos2::ZERO,
            buttons: BTreeSet::new(),
            touches: BTreeSet::new(),
            modifiers: Modifiers::default(),
            pinching: false,
            rotating: false,
        };
        open.send_ui_input(true);
        self.open = Some(open);

        let proxy = self.proxy.clone();
        let spawned = mln_util::thread::builder("render").spawn(move || {
            // Sent even if the render thread panics, so the window closes.
            struct Ended(EventLoopProxy<UserEvent>);
            impl Drop for Ended {
                fn drop(&mut self) {
                    let _ = self.0.send_event(UserEvent::Ended);
                }
            }
            let _ended = Ended(proxy);
            view::run(launch);
        });
        if let Err(e) = spawned {
            eprintln!("failed to start the render thread: {e}");
            event_loop.exit();
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let response = open.egui.on_window_event(&open.window, &event);
        if response.repaint {
            open.send_ui_input(false);
        }
        match event {
            // The render thread drops the map, then says it has ended.
            WindowEvent::CloseRequested => open.inbox.send(Message::Close),
            WindowEvent::Resized(size) => open.inbox.send(Message::Resized(size)),
            WindowEvent::Focused(focused) => open.inbox.send(Message::Focused { focused, at: Instant::now() }),
            WindowEvent::ModifiersChanged(state) => open.modifiers = modifiers(state.state()),
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } if !response.consumed => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    open.inbox.send(Message::Key {
                        code,
                        pressed: event.state == ElementState::Pressed,
                        modifiers: open.modifiers,
                        at: Instant::now(),
                    });
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                open.cursor = open.logical(position);
                let scale = open.window.scale_factor();
                open.cursor_points = egui::pos2((position.x / scale) as f32, (position.y / scale) as f32);
                if !open.buttons.is_empty() || !open.over_controls(position) {
                    open.input(Input::PointerMove { id: 0, position: open.cursor });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    MouseButton::Left => Button::Primary,
                    MouseButton::Right => Button::Secondary,
                    MouseButton::Middle => Button::Middle,
                    _ => return,
                };
                let position = open.cursor;
                match state {
                    ElementState::Pressed if !egui_over(open.egui.egui_ctx(), open.cursor_points) => {
                        open.buttons.insert(button_key(button));
                        open.input(Input::PointerDown {
                            id: 0,
                            kind: PointerKind::Mouse,
                            button,
                            position,
                            modifiers: open.modifiers,
                        });
                    }
                    ElementState::Released if open.buttons.remove(&button_key(button)) => {
                        open.input(Input::PointerUp { id: 0, position });
                    }
                    _ => {}
                }
            }
            // winit's positive y scrolls up, which zooms in; the recognizer's
            // scrolls down. Line deltas are a wheel's notches, pixel deltas a
            // trackpad's or a precise wheel's scrolling.
            WindowEvent::MouseWheel { delta, .. } => {
                let delta = match delta {
                    MouseScrollDelta::LineDelta(_, y) => WheelDelta::Notches(-f64::from(y)),
                    MouseScrollDelta::PixelDelta(pixels) => WheelDelta::Pixels(-pixels.y / open.pixel_ratio),
                };
                if delta != WheelDelta::Notches(0.0)
                    && delta != WheelDelta::Pixels(0.0)
                    && !egui_over(open.egui.egui_ctx(), open.cursor_points)
                {
                    open.input(Input::Wheel { position: open.cursor, delta, modifiers: open.modifiers });
                }
            }
            WindowEvent::Touch(touch) => {
                let id = touch_id(touch.id);
                let position = open.logical(touch.location);
                let input = match touch.phase {
                    TouchPhase::Started if !open.over_controls(touch.location) => {
                        open.touches.insert(id);
                        Input::PointerDown {
                            id,
                            kind: PointerKind::Touch,
                            button: Button::Primary,
                            position,
                            modifiers: open.modifiers,
                        }
                    }
                    TouchPhase::Moved if open.touches.contains(&id) => Input::PointerMove { id, position },
                    TouchPhase::Ended if open.touches.remove(&id) => Input::PointerUp { id, position },
                    TouchPhase::Cancelled if open.touches.remove(&id) => Input::PointerCancel { id },
                    _ => return,
                };
                open.input(input);
            }
            // macOS's trackpad magnify and rotate (winit has them nowhere
            // else). winit's rotation turns counterclockwise.
            WindowEvent::PinchGesture { delta, phase, .. } => open.trackpad(false, delta.exp(), 0.0, phase),
            WindowEvent::RotationGesture { delta, phase, .. } => {
                open.trackpad(true, 1.0, -f64::from(delta), phase);
            }
            // The render thread draws when it has something new.
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        match event {
            UserEvent::MakeSurface(instance, reply) => {
                let surface = instance
                    .create_surface(open.window.clone())
                    .map_err(|e| format!("Failed to create a surface for the window: {e}"));
                let _ = reply.send(surface);
            }
            UserEvent::SetTitle(title) => open.window.set_title(&title),
            UserEvent::Ui(output) => open.egui.handle_platform_output(&open.window, *output),
            UserEvent::RequestSize(size) => {
                let _ = open.window.request_inner_size(size);
            }
            UserEvent::Ended => event_loop.exit(),
        }
    }
}
