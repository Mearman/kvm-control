//! In-process terminal interface for an already connected `kvm_core::Device`.
//!
//! Integration assumptions: connection, authentication and device selection belong to the caller.
//! The foundation exposes no streaming API, so live video cannot yet be rendered. Capabilities
//! are treated as authoritative and input is never retried after an error. The foundation uses
//! boolean capabilities; integration with the forthcoming tri-state capability API must display
//! Unknown separately from Unsupported and enable controls only when explicitly Supported.

use std::{io, time::Duration};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use kvm_core::{Capabilities, Device, DeviceStatus};
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
}

impl App {
    /// Refreshes capability and status information through the shared device API.
    pub async fn refresh(&mut self, device: &dyn Device) {
        match device.capabilities().await {
            Ok(capabilities) => {
                let status_supported = capabilities.status;
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
                if self.capabilities.as_ref().is_some_and(|c| c.keyboard) {
                    self.control = true;
                } else {
                    self.message = "Keyboard control is unsupported or undiscovered".into();
                }
            }
            _ if self.control => {
                if let Some(key) = key_name(code) {
                    if let Err(error) = device.key(&key, true).await {
                        self.message = error.to_string();
                        self.control = false;
                    } else if let Err(error) = device.key(&key, false).await {
                        self.message = error.to_string();
                        self.control = false;
                    }
                }
            }
            _ => {}
        }
        true
    }
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
            "status: {}  snapshot: {}  keyboard: {}  mouse: {}  live: {}",
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

struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// Runs the TUI for an already authenticated device. Input cleanup is attempted on every normal return.
pub async fn run(device: &dyn Device) -> io::Result<()> {
    let _guard = TerminalGuard::enter()?;
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
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && !app.key(device, key.code).await
        {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

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

    #[test]
    fn maps_only_known_keys() {
        assert_eq!(key_name(KeyCode::Char('a')).as_deref(), Some("KeyA"));
        assert_eq!(key_name(KeyCode::Char('!')), None);
    }
}
