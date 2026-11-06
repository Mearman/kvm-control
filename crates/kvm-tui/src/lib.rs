//! In-process terminal interface for an already connected `kvm_core::Device`.
//!
//! Integration assumptions: connection, authentication and device selection belong to the caller.
//! Video frames are rendered using coloured character cells; input is never retried after an error.

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
    pending: Option<kvm_core::Control>,
    command: Option<String>,
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
                self.message = "Press v to open live video".into();
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
        let Some(video) = &self.video else {
            if matches!(event.kind, MouseEventKind::Up(_))
                && let Err(error) = device.release_inputs().await
            {
                self.control = false;
                self.message = error.to_string();
            }
            return;
        };
        let Some(viewport) = video_viewport(area, video.width(), video.height()) else {
            if matches!(event.kind, MouseEventKind::Up(_))
                && let Err(error) = device.release_inputs().await
            {
                self.control = false;
                self.message = error.to_string();
            }
            return;
        };
        let Some((x, y)) = mouse_coordinates(event.column, event.row, viewport) else {
            if let MouseEventKind::Up(button) = event.kind {
                let name = match button {
                    MouseButton::Left => "left",
                    MouseButton::Right => "right",
                    MouseButton::Middle => "middle",
                };
                if let Err(error) = device.mouse_button(name, false).await {
                    self.control = false;
                    let cleanup = device.release_inputs().await;
                    self.message = match cleanup {
                        Ok(()) => error.to_string(),
                        Err(cleanup) => format!("{error}; input cleanup uncertain: {cleanup}"),
                    };
                }
            }
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
        self.key_event(
            device,
            crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
        )
        .await
    }

    async fn key_event(&mut self, device: &dyn Device, event: crossterm::event::KeyEvent) -> bool {
        let code = event.code;
        if let Some(operation) = self.pending.take() {
            if code == KeyCode::Char('y') {
                self.message = match device
                    .control(operation, kvm_core::Confirmation::new())
                    .await
                {
                    Ok(kvm_core::ControlOutcome::ObservedMatching) => {
                        "Request acknowledged; device state matches (causation not established)"
                            .into()
                    }
                    Ok(kvm_core::ControlOutcome::Unverified) => {
                        "Request acknowledged; outcome not verified".into()
                    }
                    Err(error) => error.to_string(),
                };
            } else {
                self.message = "Request cancelled".into();
            }
            return true;
        }
        if let Some(command) = &mut self.command {
            match code {
                KeyCode::Esc => {
                    self.command = None;
                    self.message = "Command cancelled".into();
                }
                KeyCode::Backspace => {
                    command.pop();
                    self.message = format!("Control: {command}");
                }
                KeyCode::Char(c) if !c.is_control() => {
                    command.push(c);
                    self.message = format!("Control: {command}");
                }
                KeyCode::Enter => {
                    let text = self.command.take().unwrap_or_default();
                    match parse_control(&text) {
                        Ok(operation) => {
                            let capability = self.capabilities.as_ref().map(|c| match operation {
                                kvm_core::Control::Power(_) => c.power,
                                kvm_core::Control::Switch { .. } => c.switch,
                                kvm_core::Control::Gpio { .. } => c.gpio,
                                kvm_core::Control::MediaConnect { .. }
                                | kvm_core::Control::MediaSelect { .. } => c.media,
                            });
                            if capability == Some(Capability::Supported) {
                                self.message = format!(
                                    "Confirm {operation:?}: y confirms, any other key cancels"
                                );
                                self.pending = Some(operation);
                            } else {
                                self.message = "Capability unsupported or unknown".into();
                            }
                        }
                        Err(error) => self.message = error.into(),
                    }
                }
                _ => {}
            }
            return true;
        }
        match code {
            KeyCode::Char(':') if !self.control => {
                self.command = Some(String::new());
                self.message = "Control: power on|off|off-hard|reset-hard; switch PORT; gpio CHANNEL on|off; media connect|disconnect|select IMAGE".into();
            }
            KeyCode::Esc if self.control => {
                self.control = false;
                if let Err(error) = device.release_inputs().await {
                    self.message = error.to_string();
                }
            }
            KeyCode::Char('q') if !self.control => return false,
            KeyCode::Char('r') if !self.control => self.refresh(device).await,
            KeyCode::Char('i') if !self.control => {
                if self.capabilities.as_ref().is_some_and(|c| {
                    c.keyboard == Capability::Supported || c.mouse == Capability::Supported
                }) {
                    self.control = true;
                } else {
                    self.message = "Keyboard control is unsupported or undiscovered".into();
                }
            }
            _ if self.control
                && self
                    .capabilities
                    .as_ref()
                    .is_some_and(|c| c.keyboard == Capability::Supported) =>
            {
                if let Some(key) = key_name(code) {
                    let mods = event.modifiers;
                    let mut modifiers = Vec::new();
                    if mods.contains(crossterm::event::KeyModifiers::CONTROL) {
                        modifiers.push("ControlLeft");
                    }
                    if mods.contains(crossterm::event::KeyModifiers::ALT) {
                        modifiers.push("AltLeft");
                    }
                    if mods.contains(crossterm::event::KeyModifiers::SUPER) {
                        modifiers.push("MetaLeft");
                    }
                    if mods.contains(crossterm::event::KeyModifiers::SHIFT)
                        || matches!(code, KeyCode::Char(c) if c.is_ascii_uppercase() || "_+{}|:\"~<>?!@#$%^&*()".contains(c))
                    {
                        modifiers.push("ShiftLeft");
                    }
                    let mut result = Ok(());
                    for modifier in &modifiers {
                        if let Err(error) = device.key(modifier, true).await {
                            result = Err(error);
                            break;
                        }
                    }
                    if result.is_ok() {
                        result = device.key(&key, true).await;
                    }
                    if result.is_ok() {
                        result = device.key(&key, false).await;
                    }
                    if result.is_ok() {
                        for modifier in modifiers.iter().rev() {
                            if let Err(error) = device.key(modifier, false).await {
                                result = Err(error);
                                break;
                            }
                        }
                    }
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

fn parse_control(text: &str) -> Result<kvm_core::Control, &'static str> {
    use kvm_core::{Control, PowerAction};
    let parts: Vec<_> = text.split_whitespace().collect();
    match parts.as_slice() {
        ["power", action] => Ok(Control::Power(match *action {
            "on" => PowerAction::On,
            "off" => PowerAction::Off,
            "off-hard" => PowerAction::OffHard,
            "reset-hard" => PowerAction::ResetHard,
            _ => return Err("Unknown power action"),
        })),
        ["switch", port] => Ok(Control::Switch {
            port: port.parse().map_err(|_| "Invalid switch port")?,
        }),
        ["gpio", channel, state] => Ok(Control::Gpio {
            channel: (*channel).into(),
            state: match *state {
                "on" => true,
                "off" => false,
                _ => return Err("GPIO state must be on or off"),
            },
        }),
        ["media", "connect"] => Ok(Control::MediaConnect { connected: true }),
        ["media", "disconnect"] => Ok(Control::MediaConnect { connected: false }),
        ["media", "select", image] => Ok(Control::MediaSelect {
            image: (*image).into(),
        }),
        _ => Err("Invalid control command"),
    }
}

fn video_viewport(
    area: ratatui::layout::Rect,
    width: u32,
    height: u32,
) -> Option<ratatui::layout::Rect> {
    let inner = area.inner(ratatui::layout::Margin::new(1, 1));
    if width == 0 || height == 0 || inner.width < 2 || inner.height < 2 {
        return None;
    }
    // Half-block cells represent two vertically stacked pixels.
    let pixel_height = u64::from(inner.height) * 2;
    let (w, h_pixels) =
        if u64::from(inner.width) * u64::from(height) <= pixel_height * u64::from(width) {
            (
                u64::from(inner.width),
                u64::from(inner.width) * u64::from(height) / u64::from(width),
            )
        } else {
            (
                pixel_height * u64::from(width) / u64::from(height),
                pixel_height,
            )
        };
    let w = u16::try_from(w).ok()?;
    let h = u16::try_from(h_pixels.div_ceil(2)).ok()?;
    if w < 2 || h < 2 {
        return None;
    }
    Some(ratatui::layout::Rect::new(
        inner.x + (inner.width - w) / 2,
        inner.y + (inner.height - h) / 2,
        w,
        h,
    ))
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
        KeyCode::Char(c) if c.is_ascii_alphabetic() => {
            Some(format!("Key{}", c.to_ascii_uppercase()))
        }
        KeyCode::Char(c) if c.is_ascii_digit() => Some(format!("Digit{c}")),
        KeyCode::F(n @ 1..=24) => Some(format!("F{n}")),
        KeyCode::Delete => Some("Delete".into()),
        KeyCode::Insert => Some("Insert".into()),
        KeyCode::Home => Some("Home".into()),
        KeyCode::End => Some("End".into()),
        KeyCode::PageUp => Some("PageUp".into()),
        KeyCode::PageDown => Some("PageDown".into()),
        KeyCode::Char(c) => match c {
            '-' | '_' => Some("Minus".into()),
            '=' | '+' => Some("Equal".into()),
            '[' | '{' => Some("BracketLeft".into()),
            ']' | '}' => Some("BracketRight".into()),
            '\\' | '|' => Some("Backslash".into()),
            ';' | ':' => Some("Semicolon".into()),
            '\'' | '"' => Some("Quote".into()),
            '`' | '~' => Some("Backquote".into()),
            ',' | '<' => Some("Comma".into()),
            '.' | '>' => Some("Period".into()),
            '/' | '?' => Some("Slash".into()),
            '!' => Some("Digit1".into()),
            '@' => Some("Digit2".into()),
            '#' => Some("Digit3".into()),
            '$' => Some("Digit4".into()),
            '%' => Some("Digit5".into()),
            '^' => Some("Digit6".into()),
            '&' => Some("Digit7".into()),
            '*' => Some("Digit8".into()),
            '(' => Some("Digit9".into()),
            ')' => Some("Digit0".into()),
            ' ' => Some("Space".into()),
            _ => None,
        },
        KeyCode::Enter => Some("Enter".into()),
        KeyCode::Backspace => Some("Backspace".into()),
        KeyCode::Tab => Some("Tab".into()),
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
    frame.render_widget(
        Block::default().title("Live video").borders(Borders::ALL),
        areas[1],
    );
    if let Some(image) = &app.video {
        if let Some(viewport) = video_viewport(areas[1], image.width(), image.height()) {
            let scaled = image::imageops::resize(
                image,
                u32::from(viewport.width),
                u32::from(viewport.height) * 2,
                image::imageops::FilterType::Triangle,
            );
            for y in 0..viewport.height {
                for x in 0..viewport.width {
                    let top = scaled.get_pixel(u32::from(x), u32::from(y) * 2).0;
                    let bottom = scaled.get_pixel(u32::from(x), u32::from(y) * 2 + 1).0;
                    if let Some(cell) = frame
                        .buffer_mut()
                        .cell_mut((viewport.x + x, viewport.y + y))
                    {
                        cell.set_symbol("▀")
                            .set_fg(ratatui::style::Color::Rgb(top[0], top[1], top[2]))
                            .set_bg(ratatui::style::Color::Rgb(bottom[0], bottom[1], bottom[2]));
                    }
                }
            }
        }
    } else {
        let status = match &app.status {
            Some(s) => format!("Info: {}\nHID: {}\nStreamer: {}", s.info, s.hid, s.streamer),
            None => "Status unavailable".into(),
        };
        frame.render_widget(
            Paragraph::new(status).wrap(Wrap { trim: false }),
            areas[1].inner(ratatui::layout::Margin::new(1, 1)),
        );
    }
    let help = if app.control {
        "Input mode: Escape returns to controls"
    } else {
        "q quit | r refresh | v video | i input | : control"
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

struct TerminalGuard<T: TerminalModes> {
    terminal: T,
    restored: bool,
}
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
        Ok(Self {
            terminal,
            restored: false,
        })
    }
    fn restore(&mut self) -> io::Result<()> {
        let mouse = self.terminal.mouse_off();
        let screen = self.terminal.leave();
        let raw = self.terminal.cooked();
        self.restored = mouse.is_ok() && screen.is_ok() && raw.is_ok();
        let errors: Vec<String> = [mouse, screen, raw]
            .into_iter()
            .filter_map(Result::err)
            .map(|e| e.to_string())
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(errors.join("; ")))
        }
    }
}
impl<T: TerminalModes> Drop for TerminalGuard<T> {
    fn drop(&mut self) {
        if !self.restored {
            let _ = self.restore();
        }
    }
}

/// Runs the TUI for an already authenticated device. Input cleanup is attempted on every normal return.
pub async fn run(device: &dyn Device) -> io::Result<()> {
    let mut guard = TerminalGuard::enter(SystemTerminal)?;
    let picker = ratatui_image::picker::Picker::from_query_stdio()
        .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = run_loop(&mut terminal, device, &picker, || {
        if event::poll(Duration::from_millis(25))? {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    })
    .await;
    let cleanup = device.release_inputs().await.map_err(io::Error::other);
    let restored = guard.restore();
    let errors: Vec<String> = [result, cleanup, restored]
        .into_iter()
        .filter_map(Result::err)
        .map(|e| e.to_string())
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(errors.join("; ")))
    }
}

async fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    device: &dyn Device,
    picker: &ratatui_image::picker::Picker,
    mut next_event: impl FnMut() -> io::Result<Option<Event>>,
) -> io::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let mut app = App::default();
    app.refresh(device).await;
    let mut video_rx: Option<
        tokio::sync::watch::Receiver<Option<Result<kvm_core::Snapshot, String>>>,
    > = None;
    struct StreamTask(tokio::task::JoinHandle<()>);
    impl Drop for StreamTask {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let mut stream: Option<StreamTask> = None;
    loop {
        terminal
            .draw(|frame| {
                draw(frame, &app);
                if let Some(image) = &app.video {
                    let areas = Layout::vertical([
                        Constraint::Length(3),
                        Constraint::Min(3),
                        Constraint::Length(4),
                    ])
                    .split(frame.area());
                    if let Some(area) = video_viewport(areas[1], image.width(), image.height()) {
                        let mut protocol = picker
                            .new_resize_protocol(image::DynamicImage::ImageRgb8(image.clone()));
                        frame.render_stateful_widget(
                            ratatui_image::StatefulImage::default(),
                            area,
                            &mut protocol,
                        );
                    }
                }
            })
            .map_err(io::Error::other)?;
        if let Some(receiver) = &mut video_rx
            && receiver.has_changed().unwrap_or(false)
        {
            let item = receiver.borrow_and_update().clone();
            if let Some(item) = item {
                match item {
                    Ok(frame) => match image::load_from_memory_with_format(
                        &frame.jpeg,
                        image::ImageFormat::Jpeg,
                    ) {
                        Ok(image) => {
                            app.video = Some(image.to_rgb8());
                            app.message = "Live video (character-cell fallback)".into();
                        }
                        Err(error) => {
                            app.video = None;
                            app.message = format!("Invalid video frame: {error}");
                            drop(stream.take());
                        }
                    },
                    Err(error) => {
                        app.video = None;
                        app.message = error;
                        drop(stream.take());
                    }
                }
            }
        }
        if let Some(event) = next_event()? {
            match event {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press
                        || (app.control && key.kind == KeyEventKind::Repeat) =>
                {
                    if !app.control
                        && app.command.is_none()
                        && app.pending.is_none()
                        && key.code == KeyCode::Char('v')
                    {
                        drop(stream.take());
                        app.video = None;
                        video_rx = None;
                        match device.video().await {
                            Ok(mut video) => {
                                app.video = None;
                                let (sender, receiver) = tokio::sync::watch::channel(None);
                                video_rx = Some(receiver);
                                drop(stream.replace(StreamTask(tokio::spawn(async move {
                                    loop {
                                        match video.next_frame().await {
                                            Ok(Some(frame)) => {
                                                sender.send_replace(Some(Ok(frame)));
                                            }
                                            Ok(None) => {
                                                sender.send_replace(Some(Err(
                                                    "Video stream ended".into(),
                                                )));
                                                break;
                                            }
                                            Err(error) => {
                                                sender.send_replace(Some(Err(error.to_string())));
                                                break;
                                            }
                                        }
                                    }
                                }))));
                                app.message = "Waiting for video".into();
                            }
                            Err(error) => {
                                app.video = None;
                                app.message = error.to_string();
                            }
                        }
                    } else if !app.key_event(device, key).await {
                        break;
                    }
                }
                Event::Mouse(mouse) => {
                    let areas = Layout::vertical([
                        Constraint::Length(3),
                        Constraint::Min(3),
                        Constraint::Length(4),
                    ])
                    .split(terminal.size().map_err(io::Error::other)?.into());
                    app.mouse(device, mouse, areas[1]).await;
                }
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
        video_calls: std::sync::atomic::AtomicUsize,
        fail_restart: bool,
        video_frame: Option<Vec<u8>>,
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    struct FakeStream {
        jpeg: Option<Vec<u8>>,
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl Drop for FakeStream {
        fn drop(&mut self) {
            self.dropped
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    #[async_trait::async_trait]
    impl kvm_core::VideoStream for FakeStream {
        async fn next_frame(&mut self) -> kvm_core::Result<Option<kvm_core::Snapshot>> {
            if let Some(jpeg) = self.jpeg.take() {
                Ok(Some(kvm_core::Snapshot { jpeg }))
            } else {
                std::future::pending().await
            }
        }
    }
    #[async_trait::async_trait]
    impl Device for FakeDevice {
        async fn video(&self) -> kvm_core::Result<Box<dyn kvm_core::VideoStream>> {
            let calls = self
                .video_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail_restart && calls > 0 {
                return Err(kvm_core::Error::Transport("restart failed".into()));
            }
            match &self.video_frame {
                Some(jpeg) => Ok(Box::new(FakeStream {
                    jpeg: Some(jpeg.clone()),
                    dropped: self.dropped.clone(),
                })),
                None => Err(kvm_core::Error::Unsupported("video".into())),
            }
        }
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
                power: Capability::Supported,
                switch: Capability::Unknown,
                gpio: Capability::Unknown,
                media: Capability::Unknown,
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
        async fn control(
            &self,
            _: kvm_core::Control,
            _: kvm_core::Confirmation,
        ) -> kvm_core::Result<kvm_core::ControlOutcome> {
            self.calls.lock().unwrap().push("power".into());
            Ok(kvm_core::ControlOutcome::Unverified)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fake_stream_runs_through_ui_and_releases_on_exit() {
        let image = image::RgbImage::from_pixel(8, 8, image::Rgb([220, 10, 10]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&image)
            .unwrap();
        let device = FakeDevice {
            video_frame: Some(jpeg),
            ..FakeDevice::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        let mut rounds = 0;
        let result = run_loop(
            &mut terminal,
            &device,
            &ratatui_image::picker::Picker::halfblocks(),
            || {
                rounds += 1;
                if rounds == 1 {
                    Ok(Some(Event::Key(crossterm::event::KeyEvent::new(
                        KeyCode::Char('v'),
                        crossterm::event::KeyModifiers::NONE,
                    ))))
                } else if rounds == 100 {
                    Ok(Some(Event::Key(crossterm::event::KeyEvent::new(
                        KeyCode::Char('q'),
                        crossterm::event::KeyModifiers::NONE,
                    ))))
                } else {
                    std::thread::yield_now();
                    Ok(None)
                }
            },
        )
        .await;
        assert!(result.is_ok());
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.symbol() == "▀")
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !device.dropped.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stream task must release the stream after exit");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn video_key_is_routed_to_command_and_confirmation() {
        let device = FakeDevice::default();
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        let sequence = [
            ':', 'p', 'o', 'w', 'e', 'r', ' ', 'o', 'f', 'f', '\n', 'v', 'q',
        ];
        let mut events = sequence.into_iter();
        run_loop(
            &mut terminal,
            &device,
            &ratatui_image::picker::Picker::halfblocks(),
            || {
                let c = events.next().expect("quit event");
                Ok(Some(Event::Key(crossterm::event::KeyEvent::new(
                    if c == '\n' {
                        KeyCode::Enter
                    } else {
                        KeyCode::Char(c)
                    },
                    crossterm::event::KeyModifiers::NONE,
                ))))
            },
        )
        .await
        .unwrap();
        assert_eq!(
            device.video_calls.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(device.calls.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_restart_drops_old_stream() {
        let image = image::RgbImage::from_pixel(8, 8, image::Rgb([220, 10, 10]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&image)
            .unwrap();
        let device = FakeDevice {
            video_frame: Some(jpeg),
            fail_restart: true,
            ..FakeDevice::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        let mut count = 0;
        run_loop(
            &mut terminal,
            &device,
            &ratatui_image::picker::Picker::halfblocks(),
            || {
                count += 1;
                let code = if count == 1 || count == 100 {
                    Some('v')
                } else if count == 200 {
                    Some('q')
                } else {
                    None
                };
                std::thread::yield_now();
                Ok(code.map(|c| {
                    Event::Key(crossterm::event::KeyEvent::new(
                        KeyCode::Char(c),
                        crossterm::event::KeyModifiers::NONE,
                    ))
                }))
            },
        )
        .await
        .unwrap();
        assert_eq!(
            device.video_calls.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !device.dropped.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("restart failed"));
        assert!(!content.contains('▀'));
    }

    #[test]
    fn parses_all_control_variants() {
        for input in [
            "power on",
            "power off",
            "power off-hard",
            "power reset-hard",
            "switch 2",
            "gpio fan on",
            "gpio fan off",
            "media connect",
            "media disconnect",
            "media select disk.iso",
        ] {
            assert!(parse_control(input).is_ok(), "{input}");
        }
        for input in [
            "switch invalid",
            "gpio fan invalid",
            "power unknown",
            "media select",
        ] {
            assert!(parse_control(input).is_err(), "{input}");
        }
    }

    #[tokio::test]
    async fn modifiers_and_outside_mouse_release_are_forwarded() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new(
                KeyCode::Char('A'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
        )
        .await;
        app.video = Some(image::RgbImage::new(2, 4));
        let area = ratatui::layout::Rect::new(0, 0, 4, 4);
        app.mouse(
            &device,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            area,
        )
        .await;
        app.mouse(
            &device,
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 10,
                row: 10,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            area,
        )
        .await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            [
                "key:ControlLeft:true",
                "key:ShiftLeft:true",
                "key:KeyA:true",
                "key:KeyA:false",
                "key:ShiftLeft:false",
                "key:ControlLeft:false",
                "move:-32768:-32768",
                "button:left:true",
                "button:left:false"
            ]
        );
    }

    #[tokio::test]
    async fn punctuation_includes_shift_and_balanced_key_events() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        app.key(&device, KeyCode::Char('!')).await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            [
                "key:ShiftLeft:true",
                "key:Digit1:true",
                "key:Digit1:false",
                "key:ShiftLeft:false"
            ]
        );
    }

    #[tokio::test]
    async fn mouse_only_device_can_enter_input_mode() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.capabilities.as_mut().unwrap().keyboard = Capability::Unsupported;
        app.key(&device, KeyCode::Char('i')).await;
        assert!(app.control);
    }

    #[tokio::test]
    async fn power_requires_explicit_confirmation() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        for confirmation in ['n', 'y'] {
            app.key(&device, KeyCode::Char(':')).await;
            for c in "power off".chars() {
                app.key(&device, KeyCode::Char(c)).await;
            }
            app.key(&device, KeyCode::Enter).await;
            assert!(device.calls.lock().unwrap().is_empty());
            app.key(&device, KeyCode::Char(confirmation)).await;
        }
        assert_eq!(*device.calls.lock().unwrap(), ["power"]);
        assert!(app.message.contains("not verified"));
    }

    #[tokio::test]
    async fn input_events_are_ordered_and_cleaned_up() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        app.key(&device, KeyCode::Char('a')).await;
        app.video = Some(image::RgbImage::new(2, 4));
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
    fn video_frame_renders_to_test_backend() {
        let mut terminal = Terminal::new(TestBackend::new(30, 14)).unwrap();
        let app = App {
            video: Some(image::RgbImage::from_pixel(8, 8, image::Rgb([255, 0, 0]))),
            ..App::default()
        };
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let cells = terminal.backend().buffer().content();
        assert!(
            cells.iter().any(
                |cell| cell.symbol() == "▀" && cell.fg == ratatui::style::Color::Rgb(255, 0, 0)
            )
        );
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
        fail_restore: bool,
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
            if self.fail_restore {
                Err(io::Error::other("leave failed"))
            } else {
                Ok(())
            }
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
    fn restoration_reports_error_and_still_restores_raw_mode() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut guard = TerminalGuard::enter(FakeModes {
            calls: calls.clone(),
            fail_alternate: false,
            fail_restore: true,
        })
        .unwrap();
        assert!(
            guard
                .restore()
                .unwrap_err()
                .to_string()
                .contains("leave failed")
        );
        assert_eq!(
            *calls.borrow(),
            [
                "raw",
                "alternate",
                "mouse_on",
                "mouse_off",
                "leave",
                "cooked"
            ]
        );
    }

    #[test]
    fn restores_terminal_after_exit_and_failed_setup() {
        for fail in [false, true] {
            let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let guard = TerminalGuard::enter(FakeModes {
                calls: calls.clone(),
                fail_alternate: fail,
                fail_restore: false,
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
        let area = ratatui::layout::Rect::new(11, 6, 10, 5);
        assert_eq!(mouse_coordinates(11, 6, area), Some((i16::MIN, i16::MIN)));
        assert_eq!(mouse_coordinates(20, 10, area), Some((i16::MAX, i16::MAX)));
        assert_eq!(mouse_coordinates(10, 5, area), None);
        assert_eq!(mouse_coordinates(21, 10, area), None);
    }

    #[test]
    fn maps_only_known_keys() {
        assert_eq!(key_name(KeyCode::Char('a')).as_deref(), Some("KeyA"));
        assert_eq!(key_name(KeyCode::Char('!')).as_deref(), Some("Digit1"));
        assert_eq!(key_name(KeyCode::F(12)).as_deref(), Some("F12"));
        assert_eq!(key_name(KeyCode::Delete).as_deref(), Some("Delete"));
        assert_eq!(key_name(KeyCode::Char('é')), None);
    }
}
