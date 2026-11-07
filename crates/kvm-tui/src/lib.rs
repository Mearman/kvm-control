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
pub struct App {
    pub status: Option<DeviceStatus>,
    pub capabilities: Option<Capabilities>,
    pub message: String,
    video_message: String,
    pub control: bool,
    enhanced_keyboard: bool,
    cell_size: (u16, u16),
    drag_modifiers: Vec<&'static str>,
    held_modifiers: std::collections::BTreeSet<&'static str>,
    synthetic_modifiers: std::collections::BTreeSet<&'static str>,
    /// Most recently decoded video frame, if the stream has delivered one.
    pub video: Option<image::RgbImage>,
    pending: Option<kvm_core::Control>,
    command: Option<String>,
    submitted: Option<kvm_core::Control>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            status: None,
            capabilities: None,
            message: String::new(),
            video_message: String::new(),
            control: false,
            enhanced_keyboard: false,
            cell_size: (1, 2),
            drag_modifiers: Vec::new(),
            held_modifiers: Default::default(),
            synthetic_modifiers: Default::default(),
            video: None,
            pending: None,
            command: None,
            submitted: None,
        }
    }
}

impl App {
    fn video_arrived(&mut self) {
        self.video_message = "Live video".into();
    }

    fn clear_input_state(&mut self) {
        self.held_modifiers.clear();
        self.synthetic_modifiers.clear();
        self.drag_modifiers.clear();
    }

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
            || !self.capabilities.as_ref().is_some_and(|c| {
                c.mouse == Capability::Supported && c.mouse_mode == kvm_core::MouseMode::Absolute
            })
        {
            return;
        }
        let Some(video) = &self.video else {
            if matches!(event.kind, MouseEventKind::Up(_)) {
                self.clear_input_state();
                if let Err(error) = device.release_inputs().await {
                    self.control = false;
                    self.message = error.to_string();
                }
            }
            return;
        };
        let Some(viewport) = video_viewport(area, video.width(), video.height(), self.cell_size)
        else {
            if matches!(event.kind, MouseEventKind::Up(_)) {
                self.clear_input_state();
                if let Err(error) = device.release_inputs().await {
                    self.control = false;
                    self.message = error.to_string();
                }
            }
            return;
        };
        let Some((x, y)) = video_mouse_coordinates(event.column, event.row, &viewport) else {
            if let MouseEventKind::Up(button) = event.kind {
                if !self.enhanced_keyboard {
                    for (flag, modifier) in [
                        (crossterm::event::KeyModifiers::CONTROL, "ControlLeft"),
                        (crossterm::event::KeyModifiers::ALT, "AltLeft"),
                        (crossterm::event::KeyModifiers::SHIFT, "ShiftLeft"),
                        (crossterm::event::KeyModifiers::SUPER, "MetaLeft"),
                    ] {
                        if self.drag_modifiers.contains(&modifier)
                            && !event.modifiers.contains(flag)
                        {
                            if let Err(error) = device.key(modifier, false).await {
                                self.control = false;
                                self.clear_input_state();
                                self.message = error.to_string();
                                let _ = device.release_inputs().await;
                                return;
                            }
                            self.drag_modifiers.retain(|held| *held != modifier);
                        }
                    }
                }
                for (flag, modifier, right) in [
                    (
                        crossterm::event::KeyModifiers::CONTROL,
                        "ControlLeft",
                        "ControlRight",
                    ),
                    (crossterm::event::KeyModifiers::ALT, "AltLeft", "AltRight"),
                    (
                        crossterm::event::KeyModifiers::SHIFT,
                        "ShiftLeft",
                        "ShiftRight",
                    ),
                    (
                        crossterm::event::KeyModifiers::SUPER,
                        "MetaLeft",
                        "MetaRight",
                    ),
                ] {
                    let held = if self.enhanced_keyboard {
                        self.held_modifiers.contains(modifier)
                            || self.held_modifiers.contains(right)
                    } else {
                        self.drag_modifiers.contains(&modifier)
                    };
                    if event.modifiers.contains(flag) && !held {
                        if let Err(error) = device.key(modifier, true).await {
                            self.control = false;
                            self.clear_input_state();
                            let cleanup = device.release_inputs().await;
                            self.message = match cleanup {
                                Ok(()) => error.to_string(),
                                Err(cleanup) => {
                                    format!("{error}; input cleanup uncertain: {cleanup}")
                                }
                            };
                            return;
                        }
                        if self.enhanced_keyboard {
                            self.held_modifiers.insert(modifier);
                            self.synthetic_modifiers.insert(modifier);
                        } else {
                            self.drag_modifiers.push(modifier);
                        }
                    }
                }
                let name = match button {
                    MouseButton::Left => "left",
                    MouseButton::Right => "right",
                    MouseButton::Middle => "middle",
                };
                if let Err(error) = device.mouse_button(name, false).await {
                    self.control = false;
                    self.clear_input_state();
                    let cleanup = device.release_inputs().await;
                    self.message = match cleanup {
                        Ok(()) => error.to_string(),
                        Err(cleanup) => format!("{error}; input cleanup uncertain: {cleanup}"),
                    };
                }
                if !self.enhanced_keyboard {
                    for modifier in std::mem::take(&mut self.drag_modifiers).iter().rev() {
                        if let Err(error) = device.key(modifier, false).await {
                            self.control = false;
                            self.message = error.to_string();
                            let _ = device.release_inputs().await;
                            break;
                        }
                    }
                }
            }
            return;
        };
        let mut modifiers = Vec::new();
        for (flag, name) in [
            (crossterm::event::KeyModifiers::CONTROL, "ControlLeft"),
            (crossterm::event::KeyModifiers::ALT, "AltLeft"),
            (crossterm::event::KeyModifiers::SHIFT, "ShiftLeft"),
            (crossterm::event::KeyModifiers::SUPER, "MetaLeft"),
        ] {
            if event.modifiers.contains(flag) {
                modifiers.push(name);
            }
        }
        let mut modifier_error = None;
        if self.enhanced_keyboard {
            for modifier in &modifiers {
                let right = match *modifier {
                    "ShiftLeft" => "ShiftRight",
                    "ControlLeft" => "ControlRight",
                    "AltLeft" => "AltRight",
                    _ => "MetaRight",
                };
                if !self.held_modifiers.contains(modifier) && !self.held_modifiers.contains(right) {
                    if let Err(error) = device.key(modifier, true).await {
                        modifier_error = Some(error);
                        break;
                    }
                    self.held_modifiers.insert(modifier);
                    self.synthetic_modifiers.insert(modifier);
                }
            }
        } else {
            for modifier in self.drag_modifiers.clone() {
                if !modifiers.contains(&modifier) {
                    if let Err(error) = device.key(modifier, false).await {
                        modifier_error = Some(error);
                        break;
                    }
                    self.drag_modifiers.retain(|held| *held != modifier);
                }
            }
            for modifier in &modifiers {
                if modifier_error.is_some() {
                    break;
                }
                if self.drag_modifiers.contains(modifier) {
                    continue;
                }
                if let Err(error) = device.key(modifier, true).await {
                    modifier_error = Some(error);
                    break;
                }
                self.drag_modifiers.push(modifier);
            }
        }
        let mut result = if let Some(error) = modifier_error {
            Err(error)
        } else {
            match event.kind {
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
                _ => Ok(()),
            }
        };
        if !self.enhanced_keyboard
            && result.is_ok()
            && matches!(event.kind, MouseEventKind::Down(_))
            && self.drag_modifiers.is_empty()
        {
            self.drag_modifiers = modifiers.clone();
        }
        if !self.enhanced_keyboard
            && result.is_ok()
            && !matches!(
                event.kind,
                MouseEventKind::Down(_) | MouseEventKind::Drag(_)
            )
        {
            let release = if self.drag_modifiers.is_empty() {
                modifiers
            } else {
                std::mem::take(&mut self.drag_modifiers)
            };
            for modifier in release.iter().rev() {
                if let Err(error) = device.key(modifier, false).await {
                    result = Err(error);
                    break;
                }
            }
        }
        if let Err(error) = result {
            self.control = false;
            self.clear_input_state();
            let cleanup = device.release_inputs().await;
            self.message = match cleanup {
                Ok(()) => error.to_string(),
                Err(cleanup) => format!("{error}; input cleanup uncertain: {cleanup}"),
            };
        }
    }

    #[cfg(test)]
    async fn key(&mut self, device: &dyn Device, code: KeyCode) -> bool {
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
                self.submitted = Some(operation);
                self.message = "Request submitted; outcome pending".into();
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
            KeyCode::F(12) if self.control => {
                self.control = false;
                self.clear_input_state();
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
                    self.message = match self.capabilities.as_ref().map(|c| c.mouse_mode) {
                        Some(kvm_core::MouseMode::Absolute) => {
                            "Input mode: F12 exits; Escape is remote".into()
                        }
                        Some(mode) => {
                            format!("Mouse mode {mode:?}: viewport mouse unavailable; F12 exits")
                        }
                        None => "Mouse mode unknown; F12 exits".into(),
                    };
                    if !self.enhanced_keyboard {
                        self.message
                            .push_str("; terminal lacks key releases, keys are tapped");
                    }
                } else {
                    self.message = "Keyboard and mouse are unsupported or undiscovered".into();
                }
            }
            _ if self.control
                && self
                    .capabilities
                    .as_ref()
                    .is_some_and(|c| c.keyboard == Capability::Supported) =>
            {
                if let Some(key) = key_name(code) {
                    if self.enhanced_keyboard {
                        {
                            for (flag, name) in [
                                (crossterm::event::KeyModifiers::SHIFT, "ShiftLeft"),
                                (crossterm::event::KeyModifiers::CONTROL, "ControlLeft"),
                                (crossterm::event::KeyModifiers::ALT, "AltLeft"),
                                (crossterm::event::KeyModifiers::SUPER, "MetaLeft"),
                            ] {
                                if event.modifiers.contains(flag)
                                    && key != name
                                    && key
                                        != match name {
                                            "ShiftLeft" => "ShiftRight",
                                            "ControlLeft" => "ControlRight",
                                            "AltLeft" => "AltRight",
                                            _ => "MetaRight",
                                        }
                                    && !self.held_modifiers.contains(name)
                                    && !self.held_modifiers.contains(match name {
                                        "ShiftLeft" => "ShiftRight",
                                        "ControlLeft" => "ControlRight",
                                        "AltLeft" => "AltRight",
                                        _ => "MetaRight",
                                    })
                                {
                                    if let Err(error) = device.key(name, true).await {
                                        self.control = false;
                                        self.clear_input_state();
                                        self.message = error.to_string();
                                        let _ = device.release_inputs().await;
                                        return true;
                                    }
                                    self.held_modifiers.insert(name);
                                    self.synthetic_modifiers.insert(name);
                                }
                            }
                        }
                        let release_key = match key.as_str() {
                            "ShiftRight"
                                if !self.held_modifiers.contains("ShiftRight")
                                    && self.synthetic_modifiers.contains("ShiftLeft") =>
                            {
                                "ShiftLeft"
                            }
                            "ControlRight"
                                if !self.held_modifiers.contains("ControlRight")
                                    && self.synthetic_modifiers.contains("ControlLeft") =>
                            {
                                "ControlLeft"
                            }
                            "AltRight"
                                if !self.held_modifiers.contains("AltRight")
                                    && self.synthetic_modifiers.contains("AltLeft") =>
                            {
                                "AltLeft"
                            }
                            "MetaRight"
                                if !self.held_modifiers.contains("MetaRight")
                                    && self.synthetic_modifiers.contains("MetaLeft") =>
                            {
                                "MetaLeft"
                            }
                            _ => key.as_str(),
                        };
                        let synthetic_flag = match release_key {
                            "ShiftLeft" => Some(crossterm::event::KeyModifiers::SHIFT),
                            "ControlLeft" => Some(crossterm::event::KeyModifiers::CONTROL),
                            "AltLeft" => Some(crossterm::event::KeyModifiers::ALT),
                            "MetaLeft" => Some(crossterm::event::KeyModifiers::SUPER),
                            _ => None,
                        };
                        let keep_synthetic = event.kind == KeyEventKind::Release
                            && self.synthetic_modifiers.contains(release_key)
                            && synthetic_flag.is_some_and(|flag| event.modifiers.contains(flag));
                        let result = match event.kind {
                            KeyEventKind::Press => device.key(&key, true).await,
                            KeyEventKind::Release if !keep_synthetic => {
                                device.key(release_key, false).await
                            }
                            KeyEventKind::Release | KeyEventKind::Repeat => Ok(()),
                        };
                        if result.is_ok() && matches!(code, KeyCode::Modifier(_)) {
                            let name = match if event.kind == KeyEventKind::Release {
                                release_key
                            } else {
                                key.as_str()
                            } {
                                "ShiftLeft" => Some("ShiftLeft"),
                                "ControlLeft" => Some("ControlLeft"),
                                "AltLeft" => Some("AltLeft"),
                                "MetaLeft" => Some("MetaLeft"),
                                "ShiftRight" => Some("ShiftRight"),
                                "ControlRight" => Some("ControlRight"),
                                "AltRight" => Some("AltRight"),
                                "MetaRight" => Some("MetaRight"),
                                _ => None,
                            };
                            if let Some(name) = name {
                                if event.kind == KeyEventKind::Release && !keep_synthetic {
                                    self.held_modifiers.remove(name);
                                    self.synthetic_modifiers.remove(name);
                                } else if event.kind == KeyEventKind::Press {
                                    self.synthetic_modifiers.remove(name);
                                    self.held_modifiers.insert(name);
                                }
                            }
                        }
                        let result = if result.is_ok() && event.kind == KeyEventKind::Release {
                            let pair = match key.as_str() {
                                "ShiftRight" => {
                                    Some(("ShiftLeft", crossterm::event::KeyModifiers::SHIFT))
                                }
                                "ControlRight" => {
                                    Some(("ControlLeft", crossterm::event::KeyModifiers::CONTROL))
                                }
                                "AltRight" => {
                                    Some(("AltLeft", crossterm::event::KeyModifiers::ALT))
                                }
                                "MetaRight" => {
                                    Some(("MetaLeft", crossterm::event::KeyModifiers::SUPER))
                                }
                                _ => None,
                            };
                            if let Some((left, flag)) = pair
                                && self.synthetic_modifiers.contains(left)
                                && !event.modifiers.contains(flag)
                            {
                                match device.key(left, false).await {
                                    Ok(()) => {
                                        self.synthetic_modifiers.remove(left);
                                        self.held_modifiers.remove(left);
                                        Ok(())
                                    }
                                    Err(error) => Err(error),
                                }
                            } else {
                                result
                            }
                        } else {
                            result
                        };
                        if let Err(error) = result {
                            self.control = false;
                            self.clear_input_state();
                            let cleanup = device.release_inputs().await;
                            self.message = match cleanup {
                                Ok(()) => error.to_string(),
                                Err(cleanup) => {
                                    format!("{error}; input cleanup uncertain: {cleanup}")
                                }
                            };
                        }
                        return true;
                    }
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
                        || code == KeyCode::BackTab
                        || matches!(code, KeyCode::Char(c) if c.is_ascii_uppercase() || "_+{}|:\"~<>?!@#$%^&*()".contains(c))
                    {
                        modifiers.push("ShiftLeft");
                    }
                    modifiers.retain(|modifier| !self.drag_modifiers.contains(modifier));
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

#[derive(Debug, PartialEq, Eq)]
struct VideoViewport {
    cells: ratatui::layout::Rect,
    pixels: (u32, u32),
    cell_size: (u16, u16),
}

fn fit_pixels(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let ratio =
        (f64::from(max_width) / f64::from(width)).min(f64::from(max_height) / f64::from(height));
    (
        (f64::from(width) * ratio).round().max(1.0) as u32,
        (f64::from(height) * ratio).round().max(1.0) as u32,
    )
}

fn video_viewport(
    area: ratatui::layout::Rect,
    width: u32,
    height: u32,
    cell_size: (u16, u16),
) -> Option<VideoViewport> {
    let inner = area.inner(ratatui::layout::Margin::new(1, 1));
    if width == 0
        || height == 0
        || cell_size.0 == 0
        || cell_size.1 == 0
        || inner.width < 2
        || inner.height < 2
    {
        return None;
    }
    let (cw, ch) = (u32::from(cell_size.0), u32::from(cell_size.1));
    let (pixel_width, pixel_height) = fit_pixels(
        width,
        height,
        u32::from(inner.width) * cw,
        u32::from(inner.height) * ch,
    );
    let w = u16::try_from(pixel_width.div_ceil(cw)).ok()?;
    let h = u16::try_from(pixel_height.div_ceil(ch)).ok()?;
    if w < 2 || h < 2 || w > inner.width || h > inner.height {
        return None;
    }
    let pixels = fit_pixels(width, height, u32::from(w) * cw, u32::from(h) * ch);
    Some(VideoViewport {
        cells: ratatui::layout::Rect::new(
            inner.x + (inner.width - w) / 2,
            inner.y + (inner.height - h) / 2,
            w,
            h,
        ),
        pixels,
        cell_size,
    })
}

fn video_mouse_coordinates(column: u16, row: u16, viewport: &VideoViewport) -> Option<(i16, i16)> {
    let x = u32::from(column.checked_sub(viewport.cells.x)?);
    let y = u32::from(row.checked_sub(viewport.cells.y)?);
    let axis = |cell: u32, cell_size: u16, pixels: u32| -> Option<i16> {
        let cell_size = u32::from(cell_size);
        let start = cell.checked_mul(cell_size)?;
        if start >= pixels {
            return None;
        }
        if pixels < 2 {
            return None;
        }
        let centre = (start + (cell_size - 1) / 2).min(pixels - 1);
        let value =
            i64::from(i16::MIN) + i64::from(centre) * i64::from(u16::MAX) / i64::from(pixels - 1);
        Some(value as i16)
    };
    if x >= u32::from(viewport.cells.width) || y >= u32::from(viewport.cells.height) {
        return None;
    }
    Some((
        axis(x, viewport.cell_size.0, viewport.pixels.0)?,
        axis(y, viewport.cell_size.1, viewport.pixels.1)?,
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
        KeyCode::Modifier(modifier) => {
            use crossterm::event::ModifierKeyCode::*;
            Some(
                match modifier {
                    LeftShift => "ShiftLeft",
                    RightShift => "ShiftRight",
                    LeftControl => "ControlLeft",
                    RightControl => "ControlRight",
                    LeftAlt => "AltLeft",
                    RightAlt => "AltRight",
                    LeftSuper => "MetaLeft",
                    RightSuper => "MetaRight",
                    _ => return None,
                }
                .into(),
            )
        }
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
        KeyCode::Esc => Some("Escape".into()),
        KeyCode::Enter => Some("Enter".into()),
        KeyCode::Backspace => Some("Backspace".into()),
        KeyCode::Tab | KeyCode::BackTab => Some("Tab".into()),
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
        Block::default()
            .title(if app.video_message.is_empty() {
                "Live video"
            } else {
                &app.video_message
            })
            .borders(Borders::ALL),
        areas[1],
    );
    if let Some(image) = &app.video {
        if let Some(viewport) =
            video_viewport(areas[1], image.width(), image.height(), app.cell_size)
        {
            let scaled = image::imageops::resize(
                image,
                u32::from(viewport.cells.width),
                u32::from(viewport.cells.height) * 2,
                image::imageops::FilterType::Triangle,
            );
            for y in 0..viewport.cells.height {
                for x in 0..viewport.cells.width {
                    let top = scaled.get_pixel(u32::from(x), u32::from(y) * 2).0;
                    let bottom = scaled.get_pixel(u32::from(x), u32::from(y) * 2 + 1).0;
                    if let Some(cell) = frame
                        .buffer_mut()
                        .cell_mut((viewport.cells.x + x, viewport.cells.y + y))
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
        "Input mode: F12 returns to controls; Escape is sent remotely"
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
        execute!(
            io::stdout(),
            EnableMouseCapture,
            crossterm::event::EnableFocusChange
        )
    }
    fn mouse_off(&mut self) -> io::Result<()> {
        execute!(
            io::stdout(),
            DisableMouseCapture,
            crossterm::event::DisableFocusChange
        )
    }
}

struct TerminalGuard<T: TerminalModes> {
    terminal: T,
    restored: bool,
    keyboard: bool,
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
            keyboard: false,
        })
    }
    fn restore(&mut self) -> io::Result<()> {
        let keyboard = if self.keyboard {
            let result = execute!(io::stdout(), crossterm::event::PopKeyboardEnhancementFlags);
            if result.is_ok() {
                self.keyboard = false;
            }
            result
        } else {
            Ok(())
        };
        let mouse = self.terminal.mouse_off();
        let screen = self.terminal.leave();
        let raw = self.terminal.cooked();
        self.restored = keyboard.is_ok() && mouse.is_ok() && screen.is_ok() && raw.is_ok();
        let errors: Vec<String> = [keyboard, mouse, screen, raw]
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

struct InputCleanup(Option<std::sync::Arc<dyn Device>>);
impl Drop for InputCleanup {
    fn drop(&mut self) {
        if let Some(device) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                if let Err(error) = device.release_inputs().await {
                    eprintln!("Input cleanup uncertain after cancellation: {error}");
                }
            });
        }
    }
}

struct ImagePicker {
    cell_size: (u16, u16),
    protocol: ratatui_image::picker::ProtocolType,
    tmux: bool,
}

fn tmux_passthrough_warning(
    tmux: bool,
    protocol: ratatui_image::picker::ProtocolType,
) -> Option<&'static str> {
    if tmux && protocol != ratatui_image::picker::ProtocolType::Halfblocks {
        Some("Native images in tmux require allow-passthrough to be enabled externally")
    } else {
        None
    }
}

impl ImagePicker {
    #[cfg(test)]
    fn halfblocks() -> Self {
        Self {
            cell_size: (10, 20),
            protocol: ratatui_image::picker::ProtocolType::Halfblocks,
            tmux: false,
        }
    }

    fn new_resize_protocol(
        &self,
        image: image::DynamicImage,
    ) -> ratatui_image::protocol::StatefulProtocol {
        use ratatui_image::picker::ProtocolType;
        use ratatui_image::protocol::{ImageSource, StatefulProtocol, StatefulProtocolType};
        let kind = match self.protocol {
            ProtocolType::Halfblocks => StatefulProtocolType::Halfblocks(Default::default()),
            ProtocolType::Sixel => {
                StatefulProtocolType::Sixel(ratatui_image::protocol::sixel::Sixel {
                    is_tmux: self.tmux,
                    ..Default::default()
                })
            }
            ProtocolType::Iterm2 => {
                StatefulProtocolType::ITerm2(ratatui_image::protocol::iterm2::Iterm2 {
                    is_tmux: self.tmux,
                    ..Default::default()
                })
            }
            ProtocolType::Kitty => {
                static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
                let id = NEXT_ID
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    .wrapping_add(std::process::id().rotate_left(16));
                StatefulProtocolType::Kitty(ratatui_image::protocol::kitty::StatefulKitty::new(
                    id, self.tmux,
                ))
            }
        };
        StatefulProtocol::new(
            ImageSource::new(image, self.cell_size, image::Rgba([0, 0, 0, 0])),
            self.cell_size,
            kind,
        )
    }
}

fn terminal_capabilities() -> io::Result<(ImagePicker, bool)> {
    use ratatui_image::picker::ProtocolType;
    let term = std::env::var("TERM").unwrap_or_default();
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let tmux = std::env::var_os("TMUX").is_some() || term.starts_with("tmux") || program == "tmux";
    let protocol = match std::env::var("KVM_TUI_IMAGE_PROTOCOL").as_deref() {
        Ok("halfblocks") => ProtocolType::Halfblocks,
        Ok("kitty") => ProtocolType::Kitty,
        Ok("sixel") => ProtocolType::Sixel,
        Ok("iterm2") => ProtocolType::Iterm2,
        Ok(_) => return Err(io::Error::other("invalid KVM_TUI_IMAGE_PROTOCOL")),
        Err(std::env::VarError::NotPresent) => match (tmux, term.as_str(), program.as_str()) {
            (false, "xterm-kitty" | "xterm-ghostty", _) => ProtocolType::Kitty,
            (false, _, "iTerm.app" | "WezTerm") => ProtocolType::Iterm2,
            _ => ProtocolType::Halfblocks,
        },
        Err(_) => return Err(io::Error::other("invalid KVM_TUI_IMAGE_PROTOCOL encoding")),
    };
    let enhanced = !tmux && matches!(term.as_str(), "xterm-kitty" | "xterm-ghostty" | "wezterm");
    // Terminal ioctls may omit pixel dimensions; use the image library's default cell size in that case.
    let cell_size = match crossterm::terminal::window_size() {
        Ok(size)
            if size.columns > 0
                && size.rows > 0
                && size.width >= size.columns
                && size.height >= size.rows =>
        {
            (size.width / size.columns, size.height / size.rows)
        }
        _ => (10, 20),
    };
    Ok((
        ImagePicker {
            cell_size,
            protocol,
            tmux,
        },
        enhanced,
    ))
}

/// Runs the TUI for an already authenticated device. Cancellation schedules best-effort input release while the runtime remains active.
pub async fn run(device: std::sync::Arc<dyn Device>) -> io::Result<()> {
    let mut input_cleanup = InputCleanup(Some(device.clone()));
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut guard = TerminalGuard::enter(SystemTerminal)?;
    let (picker, enhanced) = terminal_capabilities()?;
    if enhanced {
        guard.keyboard = true;
        execute!(
            io::stdout(),
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | crossterm::event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | crossterm::event::KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        )?;
    }
    guard.keyboard = enhanced;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let run = run_loop(&mut terminal, device.as_ref(), &picker, enhanced, |_| {
        if event::poll(Duration::from_millis(25))? {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    });
    #[cfg(unix)]
    let result = wait_for_termination(run, &mut terminate).await;
    #[cfg(not(unix))]
    let result = run.await;
    let result = cleanup_terminal(result, device.as_ref(), &mut guard).await;
    input_cleanup.0 = None;
    result
}

async fn cleanup_terminal<T: TerminalModes>(
    result: io::Result<()>,
    device: &dyn Device,
    guard: &mut TerminalGuard<T>,
) -> io::Result<()> {
    let restored = guard.restore();
    let cleanup = device.release_inputs().await.map_err(io::Error::other);
    finish_terminal(result, cleanup, restored)
}

fn finish_terminal(
    result: io::Result<()>,
    cleanup: io::Result<()>,
    restored: io::Result<()>,
) -> io::Result<()> {
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

#[cfg(unix)]
async fn wait_for_termination<F: std::future::Future<Output = io::Result<()>>>(
    run: F,
    terminate: &mut tokio::signal::unix::Signal,
) -> io::Result<()> {
    tokio::select! { result = run => result, _ = terminate.recv() => Err(io::Error::other("terminated by SIGTERM")) }
}

async fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    device: &dyn Device,
    picker: &ImagePicker,
    enhanced: bool,
    mut next_event: impl FnMut(&Terminal<B>) -> io::Result<Option<Event>>,
) -> io::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let mut app = App {
        enhanced_keyboard: enhanced,
        cell_size: picker.cell_size,
        ..App::default()
    };
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
    let mut image_protocol: Option<ratatui_image::protocol::StatefulProtocol> = None;
    let mut control_request: Option<
        std::pin::Pin<
            Box<dyn std::future::Future<Output = kvm_core::Result<kvm_core::ControlOutcome>> + '_>,
        >,
    > = None;
    loop {
        tokio::task::yield_now().await;
        if let Some(request) = &mut control_request {
            let outcome = tokio::select! { outcome = request.as_mut() => Some(outcome), _ = tokio::time::sleep(Duration::from_millis(1)) => None };
            if let Some(outcome) = outcome {
                control_request = None;
                app.message = match outcome {
                    Ok(kvm_core::ControlOutcome::ObservedMatching) => {
                        "Request acknowledged; device state matches (causation not established)"
                            .into()
                    }
                    Ok(kvm_core::ControlOutcome::Unverified) => {
                        "Request acknowledged; outcome not verified".into()
                    }
                    Err(error) => error.to_string(),
                };
            }
        }
        if let Some(receiver) = &mut video_rx
            && receiver.has_changed().is_err()
        {
            let item = receiver.borrow_and_update().clone();
            app.video = None;
            app.control = false;
            app.clear_input_state();
            app.video_message = match item {
                Some(Err(error)) => error,
                _ => "Video stream closed".into(),
            };
            if let Err(error) = device.release_inputs().await {
                app.video_message =
                    format!("{}; input cleanup uncertain: {error}", app.video_message);
            }
            drop(stream.take());
            video_rx = None;
        }
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
                            let image = image.to_rgb8();
                            image_protocol = Some(picker.new_resize_protocol(
                                image::DynamicImage::ImageRgb8(image.clone()),
                            ));
                            app.video = Some(image);
                            app.video_arrived();
                        }
                        Err(error) => {
                            app.video = None;
                            app.video_message = format!("Invalid video frame: {error}");
                            drop(stream.take());
                        }
                    },
                    Err(error) => {
                        app.video = None;
                        app.video_message = error;
                        drop(stream.take());
                    }
                }
            }
        }
        terminal
            .draw(|frame| {
                draw(frame, &app);
                if let Some(warning) = tmux_passthrough_warning(picker.tmux, picker.protocol) {
                    let area = frame.area();
                    let row = ratatui::layout::Rect::new(
                        area.x,
                        area.y,
                        area.width,
                        u16::from(area.height > 0),
                    );
                    frame.render_widget(Paragraph::new(warning), row);
                }
                if let Some(image) = &app.video {
                    let areas = Layout::vertical([
                        Constraint::Length(3),
                        Constraint::Min(3),
                        Constraint::Length(4),
                    ])
                    .split(frame.area());
                    if let Some(area) =
                        video_viewport(areas[1], image.width(), image.height(), app.cell_size)
                    {
                        if let Some(protocol) = &mut image_protocol {
                            frame.render_stateful_widget(
                                ratatui_image::StatefulImage::default()
                                    .resize(ratatui_image::Resize::Scale(None)),
                                area.cells,
                                protocol,
                            );
                        }
                    }
                }
            })
            .map_err(io::Error::other)?;
        if let Some(event) = next_event(terminal)? {
            match event {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press
                        || (app.control
                            && (key.kind == KeyEventKind::Repeat
                                || (enhanced && key.kind == KeyEventKind::Release))) =>
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
                                app.video_message = "Waiting for video".into();
                            }
                            Err(error) => {
                                app.video = None;
                                app.video_message = error.to_string();
                            }
                        }
                    } else if !app.key_event(device, key).await {
                        if control_request.is_some() {
                            app.message = "Request still in flight; wait for outcome".into();
                        } else {
                            break;
                        }
                    }
                    if let Some(operation) = app.submitted.take() {
                        if control_request.is_none() {
                            control_request = Some(Box::pin(
                                device.control(operation, kvm_core::Confirmation::new()),
                            ));
                        } else {
                            app.message = "Another request is already in flight".into();
                        }
                    }
                }
                Event::FocusLost => {
                    app.control = false;
                    app.clear_input_state();
                    app.pending = None;
                    app.command = None;
                    app.message = match device.release_inputs().await {
                        Ok(()) if control_request.is_some() => {
                            "Focus lost; inputs released; submitted request remains in flight"
                                .into()
                        }
                        Ok(()) => "Focus lost; inputs released".into(),
                        Err(error) => format!("Focus lost; input cleanup uncertain: {error}"),
                    };
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
        fail_cleanup: bool,
        delayed_cleanup: bool,
        fail_mouse_release: bool,
        video_calls: std::sync::atomic::AtomicUsize,
        fail_restart: bool,
        video_frame: Option<Vec<u8>>,
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
        fail_video_after_frame: bool,
        block_control: bool,
        control_release: tokio::sync::Notify,
    }
    struct FakeStream {
        jpeg: Option<Vec<u8>>,
        fail_after_frame: bool,
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
            } else if self.fail_after_frame {
                Err(kvm_core::Error::Transport("stream failed".into()))
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
                    fail_after_frame: self.fail_video_after_frame,
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
                mouse_mode: kvm_core::MouseMode::Absolute,
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
            if self.fail_mouse_release && !pressed {
                Err(kvm_core::Error::AmbiguousInput("release failed".into()))
            } else {
                Ok(())
            }
        }
        async fn mouse_move(&self, x: i16, y: i16) -> kvm_core::Result<()> {
            self.calls.lock().unwrap().push(format!("move:{x}:{y}"));
            Ok(())
        }
        async fn release_inputs(&self) -> kvm_core::Result<()> {
            if self.delayed_cleanup {
                println!("SIGTERM_RELEASE_WAITING");
                use std::io::Write;
                std::io::stdout().flush().unwrap();
                self.control_release.notified().await;
            }
            self.calls.lock().unwrap().push("release".into());
            if self.fail_cleanup {
                Err(kvm_core::Error::AmbiguousInput("cleanup failed".into()))
            } else {
                Ok(())
            }
        }
        async fn control(
            &self,
            _: kvm_core::Control,
            _: kvm_core::Confirmation,
        ) -> kvm_core::Result<kvm_core::ControlOutcome> {
            self.calls.lock().unwrap().push("power".into());
            if self.block_control {
                self.control_release.notified().await;
            }
            Ok(kvm_core::ControlOutcome::Unverified)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_stream_reports_last_error_and_releases_inputs() {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&image::RgbImage::new(8, 8))
            .unwrap();
        let device = FakeDevice {
            video_frame: Some(jpeg),
            fail_video_after_frame: true,
            ..FakeDevice::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        let mut count = 0;
        run_loop(
            &mut terminal,
            &device,
            &ImagePicker::halfblocks(),
            false,
            |_| {
                count += 1;
                if count == 1 {
                    return Ok(Some(Event::Key(crossterm::event::KeyEvent::new(
                        KeyCode::Char('v'),
                        crossterm::event::KeyModifiers::NONE,
                    ))));
                }
                if count > 100 && device.dropped.load(std::sync::atomic::Ordering::SeqCst) {
                    return Ok(Some(Event::Key(crossterm::event::KeyEvent::new(
                        KeyCode::Char('q'),
                        crossterm::event::KeyModifiers::NONE,
                    ))));
                }
                std::thread::yield_now();
                Ok(None)
            },
        )
        .await
        .unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("stream failed"));
        assert!(!content.contains('▀'));
        assert_eq!(*device.calls.lock().unwrap(), ["release"]);
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
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_loop(
                &mut terminal,
                &device,
                &ImagePicker::halfblocks(),
                false,
                |terminal| {
                    rounds += 1;
                    if rounds == 1 {
                        Ok(Some(Event::Key(crossterm::event::KeyEvent::new(
                            KeyCode::Char('v'),
                            crossterm::event::KeyModifiers::NONE,
                        ))))
                    } else if terminal
                        .backend()
                        .buffer()
                        .content()
                        .iter()
                        .any(|cell| cell.symbol() == "▀")
                    {
                        Ok(Some(Event::Key(crossterm::event::KeyEvent::new(
                            KeyCode::Char('q'),
                            crossterm::event::KeyModifiers::NONE,
                        ))))
                    } else {
                        std::thread::yield_now();
                        Ok(None)
                    }
                },
            ),
        )
        .await
        .expect("video frame must render before exit");
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
    async fn focus_loss_is_processed_during_submitted_control() {
        let device = FakeDevice {
            block_control: true,
            ..FakeDevice::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        let mut events = ":power off\ny".chars();
        let mut focus_sent = false;
        run_loop(
            &mut terminal,
            &device,
            &ImagePicker::halfblocks(),
            false,
            |_| {
                if let Some(c) = events.next() {
                    return Ok(Some(Event::Key(crossterm::event::KeyEvent::new(
                        if c == '\n' {
                            KeyCode::Enter
                        } else {
                            KeyCode::Char(c)
                        },
                        crossterm::event::KeyModifiers::NONE,
                    ))));
                }
                let calls = device.calls.lock().unwrap().clone();
                if calls.iter().any(|x| x == "power") && !focus_sent {
                    focus_sent = true;
                    return Ok(Some(Event::FocusLost));
                }
                if calls.iter().any(|x| x == "release") {
                    device.control_release.notify_one();
                    return Ok(Some(Event::Key(crossterm::event::KeyEvent::new(
                        KeyCode::Char('q'),
                        crossterm::event::KeyModifiers::NONE,
                    ))));
                }
                std::thread::yield_now();
                Ok(None)
            },
        )
        .await
        .unwrap();
        assert_eq!(*device.calls.lock().unwrap(), ["power", "release"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn focus_loss_cancels_pending_confirmation() {
        let device = FakeDevice::default();
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        let sequence = [
            Some(':'),
            Some('p'),
            Some('o'),
            Some('w'),
            Some('e'),
            Some('r'),
            Some(' '),
            Some('o'),
            Some('f'),
            Some('f'),
            Some('\n'),
            None,
            Some('y'),
            Some('q'),
        ];
        let mut events = sequence.into_iter();
        run_loop(
            &mut terminal,
            &device,
            &ImagePicker::halfblocks(),
            false,
            |_| {
                Ok(Some(match events.next().expect("quit") {
                    None => Event::FocusLost,
                    Some(c) => Event::Key(crossterm::event::KeyEvent::new(
                        if c == '\n' {
                            KeyCode::Enter
                        } else {
                            KeyCode::Char(c)
                        },
                        crossterm::event::KeyModifiers::NONE,
                    )),
                }))
            },
        )
        .await
        .unwrap();
        assert_eq!(*device.calls.lock().unwrap(), ["release"]);
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
            &ImagePicker::halfblocks(),
            false,
            |_| {
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
            &ImagePicker::halfblocks(),
            false,
            |_| {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_repeat_events_reach_device() {
        let device = FakeDevice::default();
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        let mut step = 0;
        run_loop(
            &mut terminal,
            &device,
            &ImagePicker::halfblocks(),
            false,
            |_| {
                step += 1;
                let event = match step {
                    1 => crossterm::event::KeyEvent::new(
                        KeyCode::Char('i'),
                        crossterm::event::KeyModifiers::NONE,
                    ),
                    2 => crossterm::event::KeyEvent::new_with_kind(
                        KeyCode::Char('a'),
                        crossterm::event::KeyModifiers::NONE,
                        KeyEventKind::Repeat,
                    ),
                    3 => crossterm::event::KeyEvent::new(
                        KeyCode::F(12),
                        crossterm::event::KeyModifiers::NONE,
                    ),
                    _ => crossterm::event::KeyEvent::new(
                        KeyCode::Char('q'),
                        crossterm::event::KeyModifiers::NONE,
                    ),
                };
                Ok(Some(Event::Key(event)))
            },
        )
        .await
        .unwrap();
        assert_eq!(
            *device.calls.lock().unwrap(),
            ["key:KeyA:true", "key:KeyA:false", "release"]
        );
    }

    #[tokio::test]
    async fn failed_enhanced_key_clears_local_modifier_state() {
        let device = FakeDevice {
            fail_key: true,
            ..FakeDevice::default()
        };
        let mut app = App {
            enhanced_keyboard: true,
            ..App::default()
        };
        app.refresh(&device).await;
        app.control = true;
        app.held_modifiers.insert("ShiftLeft");
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new(
                KeyCode::Char('a'),
                crossterm::event::KeyModifiers::NONE,
            ),
        )
        .await;
        assert!(app.held_modifiers.is_empty());
        assert_eq!(*device.calls.lock().unwrap(), ["key:KeyA:true", "release"]);
    }

    #[tokio::test]
    async fn mouse_release_without_viewport_clears_local_state() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.control = true;
        app.drag_modifiers.push("ShiftLeft");
        app.held_modifiers.insert("ShiftLeft");
        app.mouse(
            &device,
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 0,
                row: 0,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            ratatui::layout::Rect::new(0, 0, 4, 4),
        )
        .await;
        assert!(app.drag_modifiers.is_empty());
        assert!(app.held_modifiers.is_empty());
        assert_eq!(*device.calls.lock().unwrap(), ["release"]);
    }

    #[tokio::test]
    async fn modifier_release_reconciles_other_preheld_modifier() {
        let device = FakeDevice::default();
        let mut app = App {
            enhanced_keyboard: true,
            ..App::default()
        };
        app.refresh(&device).await;
        app.control = true;
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftControl),
                crossterm::event::KeyModifiers::SHIFT,
                KeyEventKind::Release,
            ),
        )
        .await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            ["key:ShiftLeft:true", "key:ControlLeft:false"]
        );
    }

    #[tokio::test]
    async fn outside_modifier_failure_reports_failed_cleanup() {
        let device = FakeDevice {
            fail_key: true,
            fail_cleanup: true,
            ..FakeDevice::default()
        };
        let mut app = App::default();
        app.refresh(&device).await;
        app.control = true;
        app.video = Some(image::RgbImage::new(2, 4));
        app.mouse(
            &device,
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 10,
                row: 1,
                modifiers: crossterm::event::KeyModifiers::SHIFT,
            },
            ratatui::layout::Rect::new(0, 0, 4, 4),
        )
        .await;
        assert!(app.message.contains("failure"));
        assert!(app.message.contains("input cleanup uncertain"));
        assert!(app.message.contains("cleanup failed"));
        assert_eq!(
            *device.calls.lock().unwrap(),
            ["key:ShiftLeft:true", "release"]
        );
    }

    #[tokio::test]
    async fn outside_mouse_up_reconciles_new_modifier_in_both_modes() {
        for enhanced in [false, true] {
            let device = FakeDevice::default();
            let mut app = App {
                enhanced_keyboard: enhanced,
                ..App::default()
            };
            app.refresh(&device).await;
            app.control = true;
            app.video = Some(image::RgbImage::new(2, 4));
            for (kind, column, modifiers) in [
                (
                    MouseEventKind::Down(MouseButton::Left),
                    1,
                    crossterm::event::KeyModifiers::NONE,
                ),
                (
                    MouseEventKind::Up(MouseButton::Left),
                    10,
                    crossterm::event::KeyModifiers::SHIFT,
                ),
            ] {
                app.mouse(
                    &device,
                    MouseEvent {
                        kind,
                        column,
                        row: 1,
                        modifiers,
                    },
                    ratatui::layout::Rect::new(0, 0, 4, 4),
                )
                .await;
            }
            let mut expected = vec![
                "move:-32768:-32768",
                "button:left:true",
                "key:ShiftLeft:true",
                "button:left:false",
            ];
            if !enhanced {
                expected.push("key:ShiftLeft:false");
            }
            assert_eq!(*device.calls.lock().unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn outside_mouse_up_reconciles_released_drag_modifier_first() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.control = true;
        app.video = Some(image::RgbImage::new(2, 4));
        for (kind, column, modifiers) in [
            (
                MouseEventKind::Down(MouseButton::Left),
                1,
                crossterm::event::KeyModifiers::SHIFT,
            ),
            (
                MouseEventKind::Up(MouseButton::Left),
                10,
                crossterm::event::KeyModifiers::NONE,
            ),
        ] {
            app.mouse(
                &device,
                MouseEvent {
                    kind,
                    column,
                    row: 1,
                    modifiers,
                },
                ratatui::layout::Rect::new(0, 0, 4, 4),
            )
            .await;
        }
        assert_eq!(
            *device.calls.lock().unwrap(),
            [
                "key:ShiftLeft:true",
                "move:-32768:-32768",
                "button:left:true",
                "key:ShiftLeft:false",
                "button:left:false"
            ]
        );
    }

    #[tokio::test]
    async fn preheld_shift_is_reconciled_on_control_and_mouse_events() {
        let device = FakeDevice::default();
        let mut app = App {
            enhanced_keyboard: true,
            ..App::default()
        };
        app.refresh(&device).await;
        app.control = true;
        app.video = Some(image::RgbImage::new(2, 4));
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftControl),
                crossterm::event::KeyModifiers::SHIFT | crossterm::event::KeyModifiers::CONTROL,
                KeyEventKind::Press,
            ),
        )
        .await;
        app.mouse(
            &device,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: crossterm::event::KeyModifiers::SHIFT
                    | crossterm::event::KeyModifiers::CONTROL,
            },
            ratatui::layout::Rect::new(0, 0, 4, 4),
        )
        .await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            [
                "key:ShiftLeft:true",
                "key:ControlLeft:true",
                "move:-32768:-32768",
                "button:left:true"
            ]
        );
    }

    #[tokio::test]
    async fn fallback_drag_releases_modifier_when_flag_disappears() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.control = true;
        app.video = Some(image::RgbImage::new(2, 4));
        for (kind, modifiers) in [
            (
                MouseEventKind::Down(MouseButton::Left),
                crossterm::event::KeyModifiers::SHIFT,
            ),
            (
                MouseEventKind::Drag(MouseButton::Left),
                crossterm::event::KeyModifiers::NONE,
            ),
        ] {
            app.mouse(
                &device,
                MouseEvent {
                    kind,
                    column: 1,
                    row: 1,
                    modifiers,
                },
                ratatui::layout::Rect::new(0, 0, 4, 4),
            )
            .await;
        }
        assert_eq!(
            *device.calls.lock().unwrap(),
            [
                "key:ShiftLeft:true",
                "move:-32768:-32768",
                "button:left:true",
                "key:ShiftLeft:false",
                "move:-32768:-32768"
            ]
        );
    }

    #[tokio::test]
    async fn mixed_synthetic_and_tracked_modifiers_release_both_sides() {
        use crossterm::event::{KeyModifiers, ModifierKeyCode};
        for (flag, left, right, left_name, right_name) in [
            (
                KeyModifiers::SHIFT,
                ModifierKeyCode::LeftShift,
                ModifierKeyCode::RightShift,
                "ShiftLeft",
                "ShiftRight",
            ),
            (
                KeyModifiers::CONTROL,
                ModifierKeyCode::LeftControl,
                ModifierKeyCode::RightControl,
                "ControlLeft",
                "ControlRight",
            ),
            (
                KeyModifiers::ALT,
                ModifierKeyCode::LeftAlt,
                ModifierKeyCode::RightAlt,
                "AltLeft",
                "AltRight",
            ),
            (
                KeyModifiers::SUPER,
                ModifierKeyCode::LeftSuper,
                ModifierKeyCode::RightSuper,
                "MetaLeft",
                "MetaRight",
            ),
        ] {
            let device = FakeDevice::default();
            let mut app = App {
                enhanced_keyboard: true,
                ..App::default()
            };
            app.refresh(&device).await;
            app.control = true;
            for (code, modifiers, kind) in [
                (KeyCode::Char('a'), flag, KeyEventKind::Press),
                (KeyCode::Modifier(right), flag, KeyEventKind::Press),
                (KeyCode::Modifier(left), flag, KeyEventKind::Release),
                (
                    KeyCode::Modifier(right),
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                ),
            ] {
                app.key_event(
                    &device,
                    crossterm::event::KeyEvent::new_with_kind(code, modifiers, kind),
                )
                .await;
            }
            assert_eq!(
                *device.calls.lock().unwrap(),
                [
                    format!("key:{left_name}:true"),
                    "key:KeyA:true".into(),
                    format!("key:{right_name}:true"),
                    format!("key:{right_name}:false"),
                    format!("key:{left_name}:false")
                ]
            );
            assert!(app.held_modifiers.is_empty());
        }
    }

    #[tokio::test]
    async fn preheld_both_shifts_remain_active_until_last_release() {
        let device = FakeDevice::default();
        let mut app = App {
            enhanced_keyboard: true,
            ..App::default()
        };
        app.refresh(&device).await;
        app.control = true;
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                crossterm::event::KeyModifiers::SHIFT,
                KeyEventKind::Press,
            ),
        )
        .await;
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftShift),
                crossterm::event::KeyModifiers::SHIFT,
                KeyEventKind::Release,
            ),
        )
        .await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            ["key:ShiftLeft:true", "key:KeyA:true"]
        );
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Modifier(crossterm::event::ModifierKeyCode::RightShift),
                crossterm::event::KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
        )
        .await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            ["key:ShiftLeft:true", "key:KeyA:true", "key:ShiftLeft:false"]
        );
    }

    #[tokio::test]
    async fn untracked_right_release_does_not_release_physical_left() {
        let device = FakeDevice::default();
        let mut app = App {
            enhanced_keyboard: true,
            ..App::default()
        };
        app.refresh(&device).await;
        app.control = true;
        for (modifier, kind) in [
            (
                crossterm::event::ModifierKeyCode::LeftShift,
                KeyEventKind::Press,
            ),
            (
                crossterm::event::ModifierKeyCode::RightShift,
                KeyEventKind::Release,
            ),
        ] {
            app.key_event(
                &device,
                crossterm::event::KeyEvent::new_with_kind(
                    KeyCode::Modifier(modifier),
                    crossterm::event::KeyModifiers::NONE,
                    kind,
                ),
            )
            .await;
        }
        assert_eq!(
            *device.calls.lock().unwrap(),
            ["key:ShiftLeft:true", "key:ShiftRight:false"]
        );
        assert!(app.held_modifiers.contains("ShiftLeft"));
    }

    #[tokio::test]
    async fn preheld_right_shift_release_releases_synthesised_left() {
        let device = FakeDevice::default();
        let mut app = App {
            enhanced_keyboard: true,
            ..App::default()
        };
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                crossterm::event::KeyModifiers::SHIFT,
                KeyEventKind::Press,
            ),
        )
        .await;
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Modifier(crossterm::event::ModifierKeyCode::RightShift),
                crossterm::event::KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
        )
        .await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            ["key:ShiftLeft:true", "key:KeyA:true", "key:ShiftLeft:false"]
        );
        assert!(app.held_modifiers.is_empty());
    }

    #[tokio::test]
    async fn right_shift_is_not_duplicated_and_exit_clears_state() {
        let device = FakeDevice::default();
        let mut app = App {
            enhanced_keyboard: true,
            ..App::default()
        };
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        let shift = KeyCode::Modifier(crossterm::event::ModifierKeyCode::RightShift);
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new_with_kind(
                shift,
                crossterm::event::KeyModifiers::SHIFT,
                KeyEventKind::Press,
            ),
        )
        .await;
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                crossterm::event::KeyModifiers::SHIFT,
                KeyEventKind::Press,
            ),
        )
        .await;
        app.key(&device, KeyCode::F(12)).await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            ["key:ShiftRight:true", "key:KeyA:true", "release"]
        );
        assert!(app.held_modifiers.is_empty());
    }

    #[tokio::test]
    async fn fallback_keyboard_does_not_release_drag_modifier() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.control = true;
        app.video = Some(image::RgbImage::new(2, 4));
        app.mouse(
            &device,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: crossterm::event::KeyModifiers::SHIFT,
            },
            ratatui::layout::Rect::new(0, 0, 4, 4),
        )
        .await;
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new(
                KeyCode::Char('a'),
                crossterm::event::KeyModifiers::SHIFT,
            ),
        )
        .await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            [
                "key:ShiftLeft:true",
                "move:-32768:-32768",
                "button:left:true",
                "key:KeyA:true",
                "key:KeyA:false"
            ]
        );
    }

    #[tokio::test]
    async fn shift_started_during_drag_is_released_outside() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.control = true;
        app.video = Some(image::RgbImage::new(2, 4));
        for (kind, column, mods) in [
            (
                MouseEventKind::Down(MouseButton::Left),
                1,
                crossterm::event::KeyModifiers::NONE,
            ),
            (
                MouseEventKind::Drag(MouseButton::Left),
                1,
                crossterm::event::KeyModifiers::SHIFT,
            ),
            (
                MouseEventKind::Up(MouseButton::Left),
                10,
                crossterm::event::KeyModifiers::SHIFT,
            ),
        ] {
            app.mouse(
                &device,
                MouseEvent {
                    kind,
                    column,
                    row: 1,
                    modifiers: mods,
                },
                ratatui::layout::Rect::new(0, 0, 4, 4),
            )
            .await;
        }
        assert_eq!(
            *device.calls.lock().unwrap(),
            [
                "move:-32768:-32768",
                "button:left:true",
                "key:ShiftLeft:true",
                "move:-32768:-32768",
                "button:left:false",
                "key:ShiftLeft:false"
            ]
        );
    }

    #[tokio::test]
    async fn cancellation_guard_schedules_input_release() {
        let device = std::sync::Arc::new(FakeDevice::default());
        let guard = InputCleanup(Some(device.clone()));
        drop(guard);
        tokio::time::timeout(Duration::from_secs(2), async {
            while device.calls.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(*device.calls.lock().unwrap(), ["release"]);
    }

    #[tokio::test]
    async fn enhanced_input_reconciles_preheld_modifier() {
        let device = FakeDevice::default();
        let mut app = App {
            enhanced_keyboard: true,
            ..App::default()
        };
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                crossterm::event::KeyModifiers::SHIFT,
                KeyEventKind::Press,
            ),
        )
        .await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            ["key:ShiftLeft:true", "key:KeyA:true"]
        );
    }

    #[tokio::test]
    async fn enhanced_keys_hold_until_release_and_support_modifier_only() {
        let device = FakeDevice::default();
        let mut app = App {
            enhanced_keyboard: true,
            ..App::default()
        };
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        let shift = KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftShift);
        for (code, kind) in [
            (shift, KeyEventKind::Press),
            (KeyCode::Char('a'), KeyEventKind::Press),
            (KeyCode::Char('a'), KeyEventKind::Release),
            (shift, KeyEventKind::Release),
        ] {
            app.key_event(
                &device,
                crossterm::event::KeyEvent::new_with_kind(
                    code,
                    crossterm::event::KeyModifiers::NONE,
                    kind,
                ),
            )
            .await;
        }
        assert_eq!(
            *device.calls.lock().unwrap(),
            [
                "key:ShiftLeft:true",
                "key:KeyA:true",
                "key:KeyA:false",
                "key:ShiftLeft:false"
            ]
        );
    }

    #[tokio::test]
    async fn enhanced_modifier_stays_held_across_mouse_click() {
        let device = FakeDevice::default();
        let mut app = App {
            enhanced_keyboard: true,
            ..App::default()
        };
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        app.video = Some(image::RgbImage::new(2, 4));
        let shift = KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftShift);
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new_with_kind(
                shift,
                crossterm::event::KeyModifiers::SHIFT,
                KeyEventKind::Press,
            ),
        )
        .await;
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
                    modifiers: crossterm::event::KeyModifiers::SHIFT,
                },
                ratatui::layout::Rect::new(0, 0, 4, 4),
            )
            .await;
        }
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new_with_kind(
                shift,
                crossterm::event::KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
        )
        .await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            [
                "key:ShiftLeft:true",
                "move:-32768:-32768",
                "button:left:true",
                "move:-32768:-32768",
                "button:left:false",
                "key:ShiftLeft:false"
            ]
        );
    }

    #[tokio::test]
    async fn fallback_mouse_click_forwards_modifiers() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.control = true;
        app.video = Some(image::RgbImage::new(2, 4));
        app.mouse(
            &device,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: crossterm::event::KeyModifiers::SHIFT,
            },
            ratatui::layout::Rect::new(0, 0, 4, 4),
        )
        .await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            [
                "key:ShiftLeft:true",
                "move:-32768:-32768",
                "button:left:true"
            ]
        );
        for kind in [
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.mouse(
                &device,
                MouseEvent {
                    kind,
                    column: 1,
                    row: 1,
                    modifiers: crossterm::event::KeyModifiers::SHIFT,
                },
                ratatui::layout::Rect::new(0, 0, 4, 4),
            )
            .await;
        }
        assert_eq!(
            *device.calls.lock().unwrap(),
            [
                "key:ShiftLeft:true",
                "move:-32768:-32768",
                "button:left:true",
                "move:-32768:-32768",
                "move:-32768:-32768",
                "button:left:false",
                "key:ShiftLeft:false"
            ]
        );
    }

    #[tokio::test]
    async fn backtab_sends_shift_tab() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.control = true;
        app.key(&device, KeyCode::BackTab).await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            [
                "key:ShiftLeft:true",
                "key:Tab:true",
                "key:Tab:false",
                "key:ShiftLeft:false"
            ]
        );
    }

    #[test]
    fn viewport_accounts_for_two_fits_and_cell_padding() {
        let area = ratatui::layout::Rect::new(0, 0, 82, 42);
        let viewport = video_viewport(area, 640, 480, (8, 16)).unwrap();
        assert_eq!(viewport.cells, ratatui::layout::Rect::new(1, 6, 80, 30));
        assert_eq!(viewport.pixels, (640, 480));
        assert_eq!(
            video_mouse_coordinates(1, 6, &viewport),
            Some((-32461, -31811))
        );
        assert_eq!(
            video_mouse_coordinates(80, 35, &viewport),
            Some((32356, 31672))
        );
        let padded =
            video_viewport(ratatui::layout::Rect::new(0, 0, 12, 12), 100, 31, (8, 16)).unwrap();
        assert_eq!(padded.cells, ratatui::layout::Rect::new(1, 5, 10, 2));
        assert_eq!(padded.pixels, (80, 25));
        assert_eq!(
            video_mouse_coordinates(10, 6, &padded),
            Some((29448, 30036))
        );
        assert_eq!(video_mouse_coordinates(10, 7, &padded), None);
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
    async fn mouse_forwarding_requires_absolute_mode() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.control = true;
        app.video = Some(image::RgbImage::new(2, 4));
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        for mode in [kvm_core::MouseMode::Relative, kvm_core::MouseMode::Unknown] {
            app.capabilities.as_mut().unwrap().mouse_mode = mode;
            app.mouse(&device, event, ratatui::layout::Rect::new(0, 0, 4, 4))
                .await;
        }
        assert!(device.calls.lock().unwrap().is_empty());
        app.capabilities.as_mut().unwrap().mouse_mode = kvm_core::MouseMode::Absolute;
        app.mouse(&device, event, ratatui::layout::Rect::new(0, 0, 4, 4))
            .await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            ["move:-32768:-32768", "button:left:true"]
        );
    }

    #[tokio::test]
    async fn resize_and_failed_outside_release_clean_up_inputs() {
        let device = FakeDevice {
            fail_mouse_release: true,
            ..FakeDevice::default()
        };
        let mut app = App::default();
        app.refresh(&device).await;
        app.control = true;
        app.video = Some(image::RgbImage::new(2, 4));
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        app.mouse(
            &device,
            mouse(MouseEventKind::Down(MouseButton::Left), 1, 1),
            ratatui::layout::Rect::new(0, 0, 4, 4),
        )
        .await;
        app.mouse(
            &device,
            mouse(MouseEventKind::Up(MouseButton::Left), 10, 10),
            ratatui::layout::Rect::new(0, 0, 4, 4),
        )
        .await;
        assert_eq!(
            *device.calls.lock().unwrap(),
            [
                "move:-32768:-32768",
                "button:left:true",
                "button:left:false",
                "release"
            ]
        );
        assert!(!app.control);
        device.calls.lock().unwrap().clear();
        app.control = true;
        app.mouse(
            &device,
            mouse(MouseEventKind::Up(MouseButton::Left), 0, 0),
            ratatui::layout::Rect::new(0, 0, 1, 1),
        )
        .await;
        assert_eq!(*device.calls.lock().unwrap(), ["release"]);
    }

    #[tokio::test]
    async fn mouse_only_device_can_enter_input_mode() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.capabilities.as_mut().unwrap().keyboard = Capability::Unsupported;
        app.key(&device, KeyCode::Char('i')).await;
        assert!(app.control);
        app.key(&device, KeyCode::Char('a')).await;
        assert!(device.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn video_frames_preserve_confirmation_and_outcome() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char(':')).await;
        for c in "power off".chars() {
            app.key(&device, KeyCode::Char(c)).await;
        }
        app.key(&device, KeyCode::Enter).await;
        app.video_arrived();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("Confirm Power(Off)"));
        assert!(app.pending.is_some());
        app.key(&device, KeyCode::Char('y')).await;
        app.message = "Request acknowledged; outcome not verified".into();
        app.video_arrived();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("Request acknowledged; outcome not verified"));
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
        assert!(device.calls.lock().unwrap().is_empty());
        assert!(matches!(
            app.submitted,
            Some(kvm_core::Control::Power(kvm_core::PowerAction::Off))
        ));
        assert!(app.message.contains("outcome pending"));
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
        app.key(&device, KeyCode::F(12)).await;
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
    fn tmux_native_protocols_warn_about_passthrough() {
        use ratatui_image::picker::ProtocolType;
        for protocol in [
            ProtocolType::Kitty,
            ProtocolType::Sixel,
            ProtocolType::Iterm2,
        ] {
            assert!(
                tmux_passthrough_warning(true, protocol)
                    .unwrap()
                    .contains("allow-passthrough")
            );
            assert!(tmux_passthrough_warning(false, protocol).is_none());
        }
        assert!(tmux_passthrough_warning(true, ProtocolType::Halfblocks).is_none());
    }

    #[test]
    fn kitty_protocol_transmits_once_and_reencodes_on_resize() {
        use ratatui::widgets::StatefulWidget;
        let picker = ImagePicker {
            cell_size: (10, 20),
            protocol: ratatui_image::picker::ProtocolType::Kitty,
            tmux: false,
        };
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            40,
            40,
            image::Rgb([2, 3, 4]),
        ));
        let mut state = picker.new_resize_protocol(image);
        let area = ratatui::layout::Rect::new(0, 0, 4, 2);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        ratatui_image::StatefulImage::default().render(area, &mut buffer, &mut state);
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| cell.symbol().contains("\x1b_G"))
        );
        let mut second = ratatui::buffer::Buffer::empty(area);
        ratatui_image::StatefulImage::default().render(area, &mut second, &mut state);
        assert!(
            !second
                .content()
                .iter()
                .any(|cell| cell.symbol().contains("\x1b_G"))
        );
        let resized = ratatui::layout::Rect::new(0, 0, 2, 2);
        let mut third = ratatui::buffer::Buffer::empty(resized);
        ratatui_image::StatefulImage::default().render(resized, &mut third, &mut state);
        assert!(
            third
                .content()
                .iter()
                .any(|cell| cell.symbol().contains("\x1b_G"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn startup_process_restores_pty_on_sigterm() {
        const CHILD: &str = "KVM_STARTUP_PTY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = runtime.block_on(run(std::sync::Arc::new(FakeDevice::default())));
            assert!(result.is_err());
            return;
        }
        let script = r#"
import os, pty, select, signal, subprocess, sys, termios, time, tempfile
sandbox = tempfile.TemporaryDirectory()
marker = os.path.join(sandbox.name, 'called')
with open(os.path.join(sandbox.name, 'tmux'), 'w') as f:
    f.write('#!/bin/sh\nprintf called > ' + marker + '\n')
os.chmod(os.path.join(sandbox.name, 'tmux'), 0o700)
master, slave = pty.openpty()
before = termios.tcgetattr(slave)
env = dict(os.environ, KVM_STARTUP_PTY_CHILD='1', TERM='tmux-256color', TERM_PROGRAM='tmux', TMUX='/nonexistent', PATH=sandbox.name, KVM_TUI_IMAGE_PROTOCOL='halfblocks')
proc = subprocess.Popen([sys.argv[1], '--exact', 'tests::startup_process_restores_pty_on_sigterm', '--nocapture'], stdin=slave, stdout=slave, stderr=slave, env=env)
data = b''
deadline = time.monotonic() + 10
try:
    while b'\x1b[?1049h' not in data:
        if time.monotonic() >= deadline or proc.poll() is not None:
            raise AssertionError('startup did not enter alternate screen: ' + repr(data))
        if select.select([master], [], [], 0.1)[0]:
            data += os.read(master, 65536)
    os.kill(proc.pid, signal.SIGTERM)
    while proc.poll() is None:
        if time.monotonic() >= deadline:
            raise AssertionError('SIGTERM did not terminate startup')
        if select.select([master], [], [], 0.1)[0]:
            data += os.read(master, 65536)
    while select.select([master], [], [], 0)[0]:
        data += os.read(master, 65536)
    assert proc.returncode == 0, (proc.returncode, data)
    assert not os.path.exists(marker), 'startup invoked tmux'
    assert b'\x1b[?1049l' in data, data
    after = termios.tcgetattr(slave)
    # Darwin may set PENDIN transiently when canonical mode is restored.
    if hasattr(termios, 'PENDIN'):
        before[3] &= ~termios.PENDIN
        after[3] &= ~termios.PENDIN
    assert after == before, ('terminal attributes not restored', before, after, data)
finally:
    if proc.poll() is None:
        proc.kill()
        proc.wait()
    os.close(master)
    os.close(slave)
"#;
        let output = std::process::Command::new("python3")
            .args(["-I", "-c", script])
            .arg(std::env::current_exe().unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn sigterm_process_releases_input_and_restores_terminal() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};
        const CHILD: &str = "KVM_SIGTERM_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let mut signal =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .unwrap();
                let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                let mut guard = TerminalGuard::enter(FakeModes {
                    calls: calls.clone(),
                    fail_alternate: false,
                    fail_restore: false,
                })
                .unwrap();
                let device = FakeDevice {
                    delayed_cleanup: true,
                    ..FakeDevice::default()
                };
                let _ = terminal_capabilities();
                println!("SIGTERM_READY");
                std::io::stdout().flush().unwrap();
                let result = wait_for_termination(std::future::pending(), &mut signal).await;
                let cleanup = cleanup_terminal(result, &device, &mut guard);
                tokio::pin!(cleanup);
                tokio::select! {
                    _ = &mut cleanup => panic!("cleanup returned before delayed release"),
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                }
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
                device.control_release.notify_one();
                assert!(cleanup.await.is_err());
                assert_eq!(*device.calls.lock().unwrap(), ["release"]);
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
                println!("SIGTERM_CLEANED");
            });
            return;
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::sigterm_process_releases_input_and_restores_terminal",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(
                output.read_line(&mut line).unwrap(),
                0,
                "child exited before readiness"
            );
            if line.contains("SIGTERM_READY") {
                break;
            }
        }
        assert!(
            Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let mut rest = String::new();
        std::io::Read::read_to_string(&mut output, &mut rest).unwrap();
        assert!(child.wait().unwrap().success());
        assert!(rest.contains("SIGTERM_RELEASE_WAITING"));
        assert!(rest.contains("SIGTERM_CLEANED"));
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
        drop(guard);
        assert_eq!(
            *calls.borrow(),
            [
                "raw",
                "alternate",
                "mouse_on",
                "mouse_off",
                "leave",
                "cooked",
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
