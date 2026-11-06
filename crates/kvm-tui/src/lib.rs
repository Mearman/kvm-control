//! In-process terminal interface for an already connected `kvm_core::Device`.
//!
//! Integration assumptions: connection, authentication and device selection belong to the caller.
//! The foundation exposes no streaming API, so live video cannot yet be rendered. Capabilities
//! are treated as authoritative and input is never retried after an error.

use std::{io, time::Duration};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
        MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use kvm_core::{Capabilities, Capability, Device, DeviceStatus};
use ratatui::{
    Frame, Terminal,
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Layout},
    widgets::{Block, Borders, Paragraph, Wrap},
};

/// State displayed by the interface. Device responses are untrusted screen content.
#[derive(Default)]
pub struct App {
    pub status: Option<DeviceStatus>,
    pub capabilities: Option<Capabilities>,
    pub message: String,
    pub control: bool,
    /// Most recently decoded video frame, if the stream has delivered one.
    pub video: Option<image::RgbImage>,
}

impl App {
    /// Refreshes capability and status information through the shared device API.
    pub async fn refresh(&mut self, device: &dyn Device) {
        match device.capabilities().await {
            Ok(capabilities) => {
                let status_supported = capabilities.status == Capability::Supported;
                self.capabilities = Some(capabilities);
                if status_supported {
                    match device.status().await {
                        Ok(status) => self.status = Some(status),
                        Err(error) => {
                            self.status = None;
                            self.message = error.to_string();
                            return;
                        }
                    }
                } else {
                    self.status = None;
                }
                self.message =
                    "Live video unavailable: streaming is not exposed by the core API".into();
            }
            Err(error) => {
                self.capabilities = None;
                self.status = None;
                self.message = error.to_string();
            }
        }
    }

    /// Forwards mouse events in the viewing area while input mode is active.
    pub async fn mouse(
        &mut self,
        device: &dyn Device,
        event: MouseEvent,
        area: ratatui::layout::Rect,
    ) {
        if !self.control
            || !self
                .capabilities
                .as_ref()
                .is_some_and(|c| c.mouse == Capability::Supported)
        {
            return;
        }
        let Some(video) = &self.video else { return; };
        let Some(viewport) = video_viewport(area, video.width(), video.height()) else { return; };
        let Some((x, y)) = mouse_coordinates(event.column, event.row, viewport) else {
            return;
        };
        let result = match event.kind {
            MouseEventKind::Moved | MouseEventKind::Drag(_) => device.mouse_move(x, y).await,
            MouseEventKind::Down(button) | MouseEventKind::Up(button) => {
                let button = match button {
                    MouseButton::Left => "left",
                    MouseButton::Right => "right",
                    MouseButton::Middle => "middle",
                };
                match device.mouse_move(x, y).await {
                    Ok(()) => {
                        device
                            .mouse_button(button, matches!(event.kind, MouseEventKind::Down(_)))
                            .await
                    }
                    Err(error) => Err(error),
                }
            }
            _ => return,
        };
        if let Err(error) = result {
            self.control = false;
            let cleanup = device.release_inputs().await;
            self.message = match cleanup {
                Ok(()) => error.to_string(),
                Err(cleanup) => format!("{error}; input cleanup uncertain: {cleanup}"),
            };
        }
    }

    /// Handles one key event. Returns false when the interface should exit.
    pub async fn key(&mut self, device: &dyn Device, code: KeyCode) -> bool {
        match code {
            KeyCode::Esc if self.control => {
                self.control = false;
                if let Err(error) = device.release_inputs().await {
                    self.message = error.to_string();
                }
            }
            KeyCode::Char('q') if !self.control => return false,
            KeyCode::Char('r') if !self.control => self.refresh(device).await,
            KeyCode::Char('i') if !self.control => {
                if self
                    .capabilities
                    .as_ref()
                    .is_some_and(|c| c.keyboard == Capability::Supported)
                {
                    self.control = true;
                } else {
                    self.message = "Keyboard control is unsupported or undiscovered".into();
                }
            }
            _ if self.control => {
                if let Some(key) = key_name(code) {
                    let result = match device.key(&key, true).await {
                        Ok(()) => device.key(&key, false).await,
                        Err(error) => Err(error),
                    };
                    if let Err(error) = result {
                        self.control = false;
                        let cleanup = device.release_inputs().await;
                        self.message = match cleanup {
                            Ok(()) => error.to_string(),
                            Err(cleanup) => format!("{error}; input cleanup uncertain: {cleanup}"),
                        };
                    }
                }
            }
            _ => {}
        }
        true
    }
}

fn video_viewport(area: ratatui::layout::Rect, width: u32, height: u32) -> Option<ratatui::layout::Rect> {
    let inner = area.inner(ratatui::layout::Margin::new(1, 1));
    if width == 0 || height == 0 || inner.width < 2 || inner.height < 2 { return None; }
    // Half-block cells represent two vertically stacked pixels.
    let pixel_height = u64::from(inner.height) * 2;
    let (w, h_pixels) = if u64::from(inner.width) * u64::from(height) <= pixel_height * u64::from(width) {
        (u64::from(inner.width), u64::from(inner.width) * u64::from(height) / u64::from(width))
    } else {
        (pixel_height * u64::from(width) / u64::from(height), pixel_height)
    };
    let w = u16::try_from(w).ok()?;
    let h = u16::try_from(h_pixels.div_ceil(2)).ok()?;
    if w < 2 || h < 2 { return None; }
    Some(ratatui::layout::Rect::new(inner.x + (inner.width - w) / 2, inner.y + (inner.height - h) / 2, w, h))
}

fn mouse_coordinates(column: u16, row: u16, area: ratatui::layout::Rect) -> Option<(i16, i16)> {
    let width = area.width;
    let height = area.height;
    if width < 2 || height < 2 {
        return None;
    }
    let x = column.checked_sub(area.x)?;
    let y = row.checked_sub(area.y)?;
    if x >= width || y >= height {
        return None;
    }
    let scale = |position: u16, size: u16| -> i16 {
        (i32::from(i16::MIN) + i32::from(position) * (i32::from(u16::MAX)) / i32::from(size - 1))
            as i16
    };
    Some((scale(x, width), scale(y, height)))
}

fn key_name(code: KeyCode) -> Option<String> {
    match code {
        KeyCode::Char(c) if c.is_ascii_lowercase() => {
            Some(format!("Key{}", c.to_ascii_uppercase()))
        }
        KeyCode::Char(c) if c.is_ascii_digit() => Some(format!("Digit{c}")),
        KeyCode::Enter => Some("Enter".into()),
        KeyCode::Backspace => Some("Backspace".into()),
        KeyCode::Tab => Some("Tab".into()),
        KeyCode::Char(' ') => Some("Space".into()),
        KeyCode::Up => Some("ArrowUp".into()),
        KeyCode::Down => Some("ArrowDown".into()),
        KeyCode::Left => Some("ArrowLeft".into()),
        KeyCode::Right => Some("ArrowRight".into()),
        _ => None,
    }
}

/// Draws the current state using any ratatui backend, including its test backend.
pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(4),
    ])
    .split(frame.area());
    let capabilities = match &app.capabilities {
        Some(c) => format!(
            "status: {:?}  snapshot: {:?}  keyboard: {:?}  mouse: {:?}  live: {:?}",
            c.status, c.snapshot, c.keyboard, c.mouse, c.live_video
        ),
        None => "Capabilities unknown".into(),
    };
    frame.render_widget(
        Paragraph::new(capabilities)
            .block(Block::default().title("Capabilities").borders(Borders::ALL)),
        areas[0],
    );
    let status = match &app.status {
        Some(s) => format!("Info: {}\nHID: {}\nStreamer: {}", s.info, s.hid, s.streamer),
        None => "Status unavailable".into(),
    };
    frame.render_widget(
        Paragraph::new(status).wrap(Wrap { trim: false }).block(
            Block::default()
                .title("Device status")
                .borders(Borders::ALL),
        ),
        areas[1],
    );
    let help = if app.control {
        "Input mode: Escape returns to controls"
    } else {
        "q quit | r refresh | i keyboard input"
    };
    frame.render_widget(
        Paragraph::new(format!("{help}\n{}", app.message))
            .wrap(Wrap { trim: false })
            .block(Block::default().title("Controls").borders(Borders::ALL)),
        areas[2],
    );
}

trait TerminalModes {
    fn raw(&mut self) -> io::Result<()>;
    fn alternate(&mut self) -> io::Result<()>;
    fn leave(&mut self) -> io::Result<()>;
    fn cooked(&mut self) -> io::Result<()>;
    fn mouse_on(&mut self) -> io::Result<()>;
    fn mouse_off(&mut self) -> io::Result<()>;
}

struct SystemTerminal;
impl TerminalModes for SystemTerminal {
    fn raw(&mut self) -> io::Result<()> {
        enable_raw_mode()
    }
    fn alternate(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnterAlternateScreen)
    }
    fn leave(&mut self) -> io::Result<()> {
        execute!(io::stdout(), LeaveAlternateScreen)
    }
    fn cooked(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }
    fn mouse_on(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnableMouseCapture)
    }
    fn mouse_off(&mut self) -> io::Result<()> {
        execute!(io::stdout(), DisableMouseCapture)
    }
}

struct TerminalGuard<T: TerminalModes>(T);
impl<T: TerminalModes> TerminalGuard<T> {
    fn enter(mut terminal: T) -> io::Result<Self> {
        terminal.raw()?;
        if let Err(error) = terminal.alternate() {
            let _ = terminal.leave();
            let _ = terminal.cooked();
            return Err(error);
        }
        if let Err(error) = terminal.mouse_on() {
            let _ = terminal.mouse_off();
            let _ = terminal.leave();
            let _ = terminal.cooked();
            return Err(error);
        }
        Ok(Self(terminal))
    }
}
impl<T: TerminalModes> Drop for TerminalGuard<T> {
    fn drop(&mut self) {
        let _ = self.0.mouse_off();
        let _ = self.0.leave();
        let _ = self.0.cooked();
    }
}

/// Runs the TUI for an already authenticated device. Input cleanup is attempted on every normal return.
pub async fn run(device: &dyn Device) -> io::Result<()> {
    let _guard = TerminalGuard::enter(SystemTerminal)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = run_loop(&mut terminal, device).await;
    let cleanup = device.release_inputs().await.map_err(io::Error::other);
    result.and(cleanup)
}

async fn run_loop<B: Backend<Error = io::Error>>(
    terminal: &mut Terminal<B>,
    device: &dyn Device,
) -> io::Result<()> {
    let mut app = App::default();
    app.refresh(device).await;
    loop {
        terminal.draw(|frame| draw(frame, &app))?;
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if !app.key(device, key.code).await {
                        break;
                    }
                }
                // Mouse events are not forwarded until a video viewport exists.
                Event::Mouse(_) => {}
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[derive(Default)]
    struct FakeDevice {
        calls: std::sync::Mutex<Vec<String>>,
        fail_key: bool,
    }
    #[async_trait::async_trait]
    impl Device for FakeDevice {
        async fn status(&self) -> kvm_core::Result<DeviceStatus> {
            Ok(DeviceStatus {
                info: serde_json::json!({}),
                hid: serde_json::json!({}),
                streamer: serde_json::json!({}),
            })
        }
        async fn capabilities(&self) -> kvm_core::Result<Capabilities> {
            Ok(Capabilities {
                status: Capability::Supported,
                snapshot: Capability::Unknown,
                keyboard: Capability::Supported,
                mouse: Capability::Supported,
                live_video: Capability::Unknown,
            })
        }
        async fn snapshot(&self) -> kvm_core::Result<kvm_core::Snapshot> {
            Err(kvm_core::Error::Unsupported("snapshot".into()))
        }
        async fn key(&self, key: &str, pressed: bool) -> kvm_core::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("key:{key}:{pressed}"));
            if self.fail_key {
                Err(kvm_core::Error::AmbiguousInput("failure".into()))
            } else {
                Ok(())
            }
        }
        async fn mouse_button(&self, button: &str, pressed: bool) -> kvm_core::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("button:{button}:{pressed}"));
            Ok(())
        }
        async fn mouse_move(&self, x: i16, y: i16) -> kvm_core::Result<()> {
            self.calls.lock().unwrap().push(format!("move:{x}:{y}"));
            Ok(())
        }
        async fn release_inputs(&self) -> kvm_core::Result<()> {
            self.calls.lock().unwrap().push("release".into());
            Ok(())
        }
    }

    #[tokio::test]
    async fn input_events_are_ordered_and_cleaned_up() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        app.key(&device, KeyCode::Char('a')).await;
        let area = ratatui::layout::Rect::new(0, 0, 4, 4);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.mouse(
                &device,
                MouseEvent {
                    kind,
                    column: 1,
                    row: 1,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
                area,
            )
            .await;
        }
        app.key(&device, KeyCode::Esc).await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            [
                "key:KeyA:true",
                "key:KeyA:false",
                "move:-32768:-32768",
                "button:left:true",
                "move:-32768:-32768",
                "button:left:false",
                "release"
            ]
        );
    }

    #[tokio::test]
    async fn ambiguous_press_is_not_replayed_and_triggers_cleanup() {
        let device = FakeDevice {
            fail_key: true,
            ..FakeDevice::default()
        };
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        app.key(&device, KeyCode::Char('a')).await;
        assert!(!app.control);
        assert_eq!(*device.calls.lock().unwrap(), ["key:KeyA:true", "release"]);
    }

    #[test]
    fn renders_unknown_status_and_stream_limitation() {
        let mut terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
        let app = App {
            message: "Live video unavailable".into(),
            ..App::default()
        };
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("Capabilities unknown"));
        assert!(content.contains("Status unavailable"));
        assert!(content.contains("Live video unavailable"));
    }

    struct FakeModes {
        calls: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
        fail_alternate: bool,
    }
    impl TerminalModes for FakeModes {
        fn raw(&mut self) -> io::Result<()> {
            self.calls.borrow_mut().push("raw");
            Ok(())
        }
        fn alternate(&mut self) -> io::Result<()> {
            self.calls.borrow_mut().push("alternate");
            if self.fail_alternate {
                Err(io::Error::other("failed"))
            } else {
                Ok(())
            }
        }
        fn leave(&mut self) -> io::Result<()> {
            self.calls.borrow_mut().push("leave");
            Ok(())
        }
        fn cooked(&mut self) -> io::Result<()> {
            self.calls.borrow_mut().push("cooked");
            Ok(())
        }
        fn mouse_on(&mut self) -> io::Result<()> {
            self.calls.borrow_mut().push("mouse_on");
            Ok(())
        }
        fn mouse_off(&mut self) -> io::Result<()> {
            self.calls.borrow_mut().push("mouse_off");
            Ok(())
        }
    }

    #[test]
    fn restores_terminal_after_exit_and_failed_setup() {
        for fail in [false, true] {
            let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let guard = TerminalGuard::enter(FakeModes {
                calls: calls.clone(),
                fail_alternate: fail,
            });
            assert_eq!(guard.is_err(), fail);
            drop(guard);
            let expected = if fail {
                vec!["raw", "alternate", "leave", "cooked"]
            } else {
                vec![
                    "raw",
                    "alternate",
                    "mouse_on",
                    "mouse_off",
                    "leave",
                    "cooked",
                ]
            };
            assert_eq!(*calls.borrow(), expected);
        }
    }

    #[test]
    fn maps_mouse_to_signed_device_coordinates() {
        let area = ratatui::layout::Rect::new(10, 5, 12, 7);
        assert_eq!(mouse_coordinates(11, 6, area), Some((i16::MIN, i16::MIN)));
        assert_eq!(mouse_coordinates(20, 10, area), Some((i16::MAX, i16::MAX)));
        assert_eq!(mouse_coordinates(10, 5, area), None);
        assert_eq!(mouse_coordinates(21, 10, area), None);
    }

    #[test]
    fn maps_only_known_keys() {
        assert_eq!(key_name(KeyCode::Char('a')).as_deref(), Some("KeyA"));
        assert_eq!(key_name(KeyCode::Char('!')), None);
    }
}
