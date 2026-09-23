//! The viewer window: presents decoded frames and turns window input into
//! display-stream control messages.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

use crate::keymap;
use crate::media::{Frame, Video};
use crate::net;
use crate::protocol::{Cursor, MAX_CLIPBOARD_BYTES, message};

/// Wait this long after the last window resize before asking the guest to
/// change resolution, so a drag does not produce a mode switch per pixel.
const RESIZE_SETTLE: Duration = Duration::from_millis(400);
/// Pixel scroll distance that counts as one wheel step.
const PIXELS_PER_WHEEL_STEP: f64 = 50.0;

pub enum UiEvent {
    /// A new frame is waiting in the shared slot.
    Frame,
    Net(net::Event),
    /// The stream ended, with the reason or the error.
    Closed(Result<String, String>),
}

pub struct Options {
    pub instance: String,
    pub control: bool,
    pub resize_guest: bool,
    pub clipboard: bool,
}

/// The window area that shows the guest display, letterboxed to keep its
/// aspect ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Viewport {
    pub fn fit(source: (u32, u32), window: (u32, u32)) -> Option<Self> {
        let (sw, sh) = source;
        let (ww, wh) = window;
        if sw == 0 || sh == 0 || ww == 0 || wh == 0 {
            return None;
        }
        let scale = (f64::from(ww) / f64::from(sw)).min(f64::from(wh) / f64::from(sh));
        let width = ((f64::from(sw) * scale).round() as u32).clamp(1, ww);
        let height = ((f64::from(sh) * scale).round() as u32).clamp(1, wh);
        Some(Self {
            x: (ww - width) / 2,
            y: (wh - height) / 2,
            width,
            height,
        })
    }

    /// Map a window position to guest pixels, or `None` outside the display.
    pub fn guest_position(self, source: (u32, u32), x: f64, y: f64) -> Option<(u32, u32)> {
        let local_x = x - f64::from(self.x);
        let local_y = y - f64::from(self.y);
        if local_x < 0.0
            || local_y < 0.0
            || local_x >= f64::from(self.width)
            || local_y >= f64::from(self.height)
        {
            return None;
        }
        let gx = (local_x * f64::from(source.0) / f64::from(self.width)) as u32;
        let gy = (local_y * f64::from(source.1) / f64::from(self.height)) as u32;
        Some((gx.min(source.0 - 1), gy.min(source.1 - 1)))
    }
}

/// Copy `frame` into the viewport of a `stride`-wide buffer. The pipeline
/// normally scales to the viewport size already, so this is a row copy;
/// nearest-neighbour scaling covers the frames around a size change.
fn blit(frame: &Frame, target: &mut [u32], stride: u32, view: Viewport) {
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    let (vw, vh) = (view.width as usize, view.height as usize);
    let stride = stride as usize;
    let start = |row: usize| (view.y as usize + row) * stride + view.x as usize;
    if fw == vw && fh == vh {
        for row in 0..vh {
            let at = start(row);
            target[at..at + vw].copy_from_slice(&frame.pixels[row * fw..(row + 1) * fw]);
        }
        return;
    }
    let columns: Vec<usize> = (0..vw).map(|x| x * fw / vw).collect();
    for row in 0..vh {
        let source = &frame.pixels[(row * fh / vh) * fw..][..fw];
        let at = start(row);
        for (pixel, column) in target[at..at + vw].iter_mut().zip(&columns) {
            *pixel = source[*column];
        }
    }
}

fn draw_cursor(
    cursor: &Cursor,
    target: &mut [u32],
    stride: u32,
    view: Viewport,
    source: (u32, u32),
) {
    if !cursor.visible
        || !cursor.valid()
        || cursor.width == 0
        || cursor.height == 0
        || source.0 == 0
        || source.1 == 0
    {
        return;
    }
    let scale_x = f64::from(view.width) / f64::from(source.0);
    let scale_y = f64::from(view.height) / f64::from(source.1);
    let left = i64::from(view.x)
        + ((f64::from(cursor.x) - f64::from(cursor.hot_x)) * scale_x).round() as i64;
    let top = i64::from(view.y)
        + ((f64::from(cursor.y) - f64::from(cursor.hot_y)) * scale_y).round() as i64;
    let width = (f64::from(cursor.width) * scale_x).round().max(1.0) as i64;
    let height = (f64::from(cursor.height) * scale_y).round().max(1.0) as i64;
    let x0 = left.max(i64::from(view.x)).max(0);
    let y0 = top.max(i64::from(view.y)).max(0);
    let x1 = (left + width)
        .min(i64::from(view.x + view.width))
        .min(i64::from(stride));
    let rows = target.len() / stride as usize;
    let y1 = (top + height)
        .min(i64::from(view.y + view.height))
        .min(rows as i64);
    for y in y0..y1 {
        let cy = ((y - top) * i64::from(cursor.height) / height) as usize;
        for x in x0..x1 {
            let cx = ((x - left) * i64::from(cursor.width) / width) as usize;
            let at = (cy * cursor.width as usize + cx) * 4;
            let pixel = u32::from_le_bytes(cursor.data[at..at + 4].try_into().unwrap());
            let alpha = (pixel >> 24) & 0xff;
            if alpha == 0 {
                continue;
            }
            let target = &mut target[y as usize * stride as usize + x as usize];
            if alpha == 255 {
                *target = pixel & 0x00ff_ffff;
                continue;
            }
            let dst = *target;
            let blend = |shift: u32| {
                let src = (pixel >> shift) & 0xff_u32;
                let old = (dst >> shift) & 0xff_u32;
                ((src * alpha + old * (255 - alpha) + 127) / 255) << shift
            };
            *target = blend(16) | blend(8) | blend(0);
        }
    }
}

fn mouse_button(button: MouseButton) -> Option<u32> {
    // QEMU InputButton: left, middle, right, wheel-up, wheel-down, side, extra.
    Some(match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::Back => 5,
        MouseButton::Forward => 6,
        MouseButton::Other(_) => return None,
    })
}

pub struct App {
    options: Options,
    video: Arc<Video>,
    latest: Arc<Mutex<Option<Frame>>>,
    pending: Arc<AtomicBool>,
    outbound: Option<mpsc::UnboundedSender<String>>,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    frame: Option<Frame>,
    cursor: Option<Cursor>,
    pointer_in_view: bool,
    /// Last guest position reported by the host pointer.
    ///
    /// A winit click is not guaranteed to be preceded by a CursorMoved event
    /// in the same event batch. Keep this so a button press can re-establish
    /// QEMU's absolute pointer position before sending the button event.
    pointer_position: Option<(u32, u32)>,
    /// Guest display size from the latest video configuration.
    source: Option<(u32, u32)>,
    sized_to_guest: bool,
    decoder: Option<(String, bool)>,
    keys: HashSet<u32>,
    buttons: HashSet<u32>,
    wheel: f64,
    resize_at: Option<Instant>,
    clipboard: Option<arboard::Clipboard>,
    clipboard_available: bool,
    last_clipboard: Option<String>,
    pub failure: Option<String>,
}

impl App {
    pub fn new(
        options: Options,
        video: Arc<Video>,
        latest: Arc<Mutex<Option<Frame>>>,
        pending: Arc<AtomicBool>,
        outbound: mpsc::UnboundedSender<String>,
    ) -> Self {
        let clipboard = (options.clipboard && options.control)
            .then(|| {
                arboard::Clipboard::new()
                    .map_err(|error| warn!(%error, "host clipboard unavailable"))
                    .ok()
            })
            .flatten();
        Self {
            options,
            video,
            latest,
            pending,
            outbound: Some(outbound),
            window: None,
            surface: None,
            frame: None,
            cursor: None,
            pointer_in_view: false,
            pointer_position: None,
            source: None,
            sized_to_guest: false,
            decoder: None,
            keys: HashSet::new(),
            buttons: HashSet::new(),
            wheel: 0.0,
            resize_at: None,
            clipboard,
            clipboard_available: false,
            last_clipboard: None,
            failure: None,
        }
    }

    fn send(&self, text: String) {
        if let Some(outbound) = &self.outbound {
            let _ = outbound.send(text);
        }
    }

    fn control(&self, text: String) {
        if self.options.control {
            self.send(text);
        }
    }

    fn window_size(&self) -> Option<(u32, u32)> {
        let size = self.window.as_ref()?.inner_size();
        Some((size.width, size.height))
    }

    fn source_size(&self) -> Option<(u32, u32)> {
        self.source
            .or_else(|| self.frame.as_ref().map(|frame| (frame.width, frame.height)))
    }

    fn viewport(&self) -> Option<Viewport> {
        Viewport::fit(self.source_size()?, self.window_size()?)
    }

    /// Have the GPU post-processor scale to the viewport, so the frame that
    /// reaches the CPU is already the size it is shown at.
    fn update_output_size(&self) {
        let (Some(view), Some(source)) = (self.viewport(), self.source_size()) else {
            return;
        };
        let size = (view.width, view.height);
        self.video.set_output_size((size != source).then_some(size));
    }

    fn update_title(&self) {
        let Some(window) = &self.window else { return };
        let mut title = format!("{} - machineemu", self.options.instance);
        if let Some((name, hardware)) = &self.decoder {
            let kind = if *hardware { "hardware" } else { "software" };
            title.push_str(&format!(" [{name}, {kind} decode]"));
        }
        if !self.options.control {
            title.push_str(" [view only]");
        }
        window.set_title(&title);
    }

    fn draw(&mut self) {
        let Some((width, height)) = self.window_size() else {
            return;
        };
        let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else {
            return;
        };
        let view = self.viewport();
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        if let Err(error) = surface.resize(w, h) {
            warn!(%error, "cannot resize the window surface");
            return;
        }
        let mut buffer = match surface.buffer_mut() {
            Ok(buffer) => buffer,
            Err(error) => {
                warn!(%error, "cannot draw");
                return;
            }
        };
        buffer.fill(0);
        if let (Some(frame), Some(view)) = (&self.frame, view) {
            blit(frame, &mut buffer, width, view);
        }
        if let (Some(cursor), Some(view), Some(source)) = (&self.cursor, view, self.source) {
            draw_cursor(cursor, &mut buffer, width, view, source);
        }
        if let Err(error) = buffer.present() {
            warn!(%error, "cannot present");
        }
    }

    fn release_inputs(&mut self) {
        for key in std::mem::take(&mut self.keys) {
            self.control(message::key(key, false));
        }
        for button in std::mem::take(&mut self.buttons) {
            self.control(message::mouse_button(button, false));
        }
    }

    /// Offer the host clipboard to the guest when the window gains focus.
    fn push_clipboard(&mut self) {
        if !self.clipboard_available {
            return;
        }
        let Some(clipboard) = self.clipboard.as_mut() else {
            return;
        };
        let Ok(text) = clipboard.get_text() else {
            return;
        };
        if self.last_clipboard.as_deref() == Some(text.as_str()) {
            return;
        }
        if let Some(message) = message::clipboard_set(&text) {
            self.last_clipboard = Some(text);
            self.control(message);
        } else {
            warn!(
                bytes = text.len(),
                limit = MAX_CLIPBOARD_BYTES,
                "host clipboard too large; not sent"
            );
        }
    }

    fn handle_net(&mut self, event: net::Event) {
        match event {
            net::Event::Cursor(cursor) => {
                self.cursor = Some(cursor);
                if let Some(window) = &self.window {
                    window.set_cursor_visible(
                        !(self.options.control
                            && self.pointer_in_view
                            && self.cursor.as_ref().is_some_and(|cursor| cursor.visible)),
                    );
                    window.request_redraw();
                }
            }
            net::Event::Config(config) => {
                let size = (config.coded_width, config.coded_height);
                if self.source != Some(size) {
                    info!(
                        width = size.0,
                        height = size.1,
                        encoder = config.encoder,
                        hardware_encoder = config.hardware,
                        capture = config.capture,
                        "guest display size"
                    );
                }
                self.source = Some(size);
                if !self.sized_to_guest
                    && let Some(window) = &self.window
                {
                    self.sized_to_guest = true;
                    debug!(
                        width = size.0,
                        height = size.1,
                        "sizing window to the guest display"
                    );
                    let _ = window.request_inner_size(PhysicalSize::new(size.0, size.1));
                }
                self.update_output_size();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            net::Event::Decoder { name, hardware } => {
                info!(decoder = name, hardware, "decoding");
                self.decoder = Some((name, hardware));
                self.update_title();
            }
            net::Event::Clipboard { available, text } => {
                self.clipboard_available = available;
                if let (Some(text), Some(clipboard)) = (text, self.clipboard.as_mut()) {
                    match clipboard.set_text(text.clone()) {
                        Ok(()) => self.last_clipboard = Some(text),
                        Err(error) => warn!(%error, "cannot set host clipboard"),
                    }
                }
            }
        }
    }
}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(format!("{} - machineemu", self.options.instance))
            .with_inner_size(PhysicalSize::new(1280, 800));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Rc::new(window),
            Err(error) => {
                self.failure = Some(format!("cannot open a window: {error}"));
                event_loop.exit();
                return;
            }
        };
        let surface = softbuffer::Context::new(window.clone())
            .and_then(|context| softbuffer::Surface::new(&context, window.clone()));
        match surface {
            Ok(surface) => self.surface = Some(surface),
            Err(error) => {
                self.failure = Some(format!("cannot draw to the window: {error}"));
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
        self.update_title();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::Frame => {
                self.pending.store(false, Ordering::Release);
                if let Some(frame) = self.latest.lock().expect("frame lock poisoned").take() {
                    self.frame = Some(frame);
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            UiEvent::Net(event) => self.handle_net(event),
            UiEvent::Closed(result) => {
                match result {
                    Ok(reason) => info!(reason, "stream ended"),
                    Err(error) => self.failure = Some(error),
                }
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.release_inputs();
                self.outbound = None;
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::Resized(size) => {
                trace!(width = size.width, height = size.height, "window resized");
                self.update_output_size();
                if self.options.control && self.options.resize_guest && self.sized_to_guest {
                    self.resize_at = Some(Instant::now() + RESIZE_SETTLE);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::Focused(true) => self.push_clipboard(),
            WindowEvent::Focused(false) => {
                debug!(
                    keys = self.keys.len(),
                    buttons = self.buttons.len(),
                    "focus lost; releasing held input"
                );
                self.release_inputs();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let Some(key) = keymap::qnum(code) else {
                    debug!(?code, "key has no QEMU keycode; ignored");
                    return;
                };
                match event.state {
                    // Repeats are forwarded like a held key on real hardware.
                    ElementState::Pressed => {
                        self.keys.insert(key);
                        self.control(message::key(key, true));
                    }
                    ElementState::Released => {
                        if self.keys.remove(&key) {
                            self.control(message::key(key, false));
                        }
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let guest_position = self
                    .viewport()
                    .zip(self.source_size())
                    .and_then(|(view, source)| view.guest_position(source, position.x, position.y));
                self.pointer_in_view = guest_position.is_some();
                self.pointer_position = guest_position;
                if let Some(window) = &self.window {
                    window.set_cursor_visible(
                        !(self.options.control
                            && self.pointer_in_view
                            && self.cursor.as_ref().is_some_and(|cursor| cursor.visible)),
                    );
                }
                if let Some((x, y)) = guest_position {
                    self.control(message::mouse_abs(x, y));
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.pointer_in_view = false;
                if let Some(window) = &self.window {
                    window.set_cursor_visible(true);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(button) = mouse_button(button) else {
                    return;
                };
                match state {
                    ElementState::Pressed => {
                        if let Some((x, y)) = self.pointer_position {
                            self.control(message::mouse_abs(x, y));
                        }
                        self.buttons.insert(button);
                        self.control(message::mouse_button(button, true));
                    }
                    ElementState::Released => {
                        if self.buttons.remove(&button) {
                            self.control(message::mouse_button(button, false));
                        }
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.wheel += match delta {
                    MouseScrollDelta::LineDelta(_, y) => f64::from(y),
                    MouseScrollDelta::PixelDelta(position) => position.y / PIXELS_PER_WHEEL_STEP,
                };
                let steps = self.wheel.trunc();
                if steps != 0.0 {
                    self.wheel -= steps;
                    // Positive winit deltas scroll up; the stream uses
                    // negative steps for wheel-up.
                    self.control(message::mouse_wheel(-(steps as i64)));
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(deadline) = self.resize_at {
            if Instant::now() >= deadline {
                self.resize_at = None;
                match (self.window_size(), self.source) {
                    (Some(window), Some(source)) if window == source => {
                        debug!(?window, "window matches the guest display; no resize");
                    }
                    (Some(window), Some(source)) => match message::resize(window.0, window.1) {
                        Some(text) => {
                            info!(?window, ?source, "requesting guest resolution");
                            self.control(text);
                        }
                        None => debug!(?window, "window size outside the guest resize limits"),
                    },
                    _ => {}
                }
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                return;
            }
        }
        event_loop.set_control_flow(ControlFlow::Wait);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_cursor_at_hotspot_and_clips_to_viewport() {
        let cursor = Cursor {
            kind: "cursor".into(),
            x: 1,
            y: 0,
            visible: true,
            width: 2,
            height: 1,
            hot_x: 1,
            hot_y: 0,
            data: vec![0, 0, 255, 255, 255, 0, 0, 128],
        };
        let mut pixels = vec![0u32; 4 * 3];
        draw_cursor(
            &cursor,
            &mut pixels,
            4,
            Viewport {
                x: 1,
                y: 1,
                width: 2,
                height: 1,
            },
            (2, 1),
        );
        assert_eq!(pixels[5], 0x00ff0000);
        assert_eq!(pixels[6], 0x00000080);
        assert_eq!(pixels[4], 0);
    }

    #[test]
    fn letterboxes_and_maps_pointer() {
        let view = Viewport::fit((1920, 1080), (1000, 1000)).unwrap();
        assert_eq!(
            view,
            Viewport {
                x: 0,
                y: 218,
                width: 1000,
                height: 563
            }
        );
        assert_eq!(view.guest_position((1920, 1080), 500.0, 100.0), None);
        assert_eq!(view.guest_position((1920, 1080), 0.0, 218.0), Some((0, 0)));
        assert_eq!(
            view.guest_position((1920, 1080), 999.9, 780.9),
            Some((1919, 1079))
        );
    }

    #[test]
    fn blits_with_and_without_scaling() {
        let frame = Frame {
            width: 2,
            height: 1,
            pixels: vec![1, 2],
        };
        let mut target = vec![0u32; 4 * 2];
        blit(
            &frame,
            &mut target,
            4,
            Viewport {
                x: 1,
                y: 1,
                width: 2,
                height: 1,
            },
        );
        assert_eq!(target, [0, 0, 0, 0, 0, 1, 2, 0]);
        let mut target = vec![0u32; 4];
        blit(
            &frame,
            &mut target,
            4,
            Viewport {
                x: 0,
                y: 0,
                width: 4,
                height: 1,
            },
        );
        assert_eq!(target, [1, 1, 2, 2]);
    }
}
