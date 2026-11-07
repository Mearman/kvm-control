//! In-process terminal interface for an already connected `kvm_core::Device`.
//!
//! Integration assumptions: connection, authentication and device selection belong to the caller.
//! Video frames are rendered with a native terminal image protocol when one is detected or selected, and with coloured character cells otherwise; input is never retried after an error.

mod capabilities;

use std::{io, time::Duration};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
        MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use kvm_core::{Capabilities, Capability, Device, DeviceStatus, Reported};
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
    /// Set by the refresh key; the event loop starts the read in the background so input keeps being handled while it runs.
    refresh_requested: bool,
    /// Whether a refresh read is in flight.
    refreshing: bool,
    /// The outcome of the last refresh, shown on its own line so it never replaces the message, which carries the command being typed, a confirmation prompt or a control's outcome.
    refresh_note: String,
    /// Control outcomes received so far, so a refresh started before the latest one is shown as predating it.
    controls_completed: u64,
    /// The message shown when input mode was entered, so leaving input mode can replace it while it is still shown, but not a message written since (a control's outcome, a failed key).
    input_entry_message: Option<String>,
    /// Why a refresh ended input mode, and whether the release it requires is still running; shown on the device-state line after any uncertain cleanup. A pending release keeps it until the release finishes, since `i` waits on it; the next refresh outcome replaces a finished one.
    input_ended: Option<InputEnded>,
    /// The error of the last release of inputs that failed, from any source (a refresh ending input mode, `F12`, focus loss, a closing stream, a failed input), until a later release succeeds. A short fixed warning ([`CLEANUP_UNCERTAIN`]) leads the device-state line, since a key or button may still be held on the remote host; the error itself goes in the message, which wraps, because an adapter's error can be long enough to push every later refresh outcome off the row. The device's contract (`Device::release_inputs`) is that a release succeeds only once no earlier input is unconfirmed; with `kvm-kvmd` that is never, once the session is indeterminate, so the warning stays for the run.
    cleanup_uncertain: Option<String>,
    /// Releases started so far, and the newest whose result has been recorded, so a release that finishes after a newer one (the background release a refresh requires, overtaken by focus loss's inline release) cannot overwrite the newer result.
    releases_started: u64,
    releases_recorded: u64,
    /// The number of the background release a refresh requires, while it runs.
    background_release: Option<u64>,
}

/// The device-state line's warning while cleanup is uncertain; short and fixed, so the refresh outcome after it stays visible.
const CLEANUP_UNCERTAIN: &str = "Cleanup uncertain";

/// A refresh that ended input mode: the cause and the state of the release it requires.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InputEnded {
    cause: &'static str,
    release: Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Release {
    Pending,
    /// Finished; whether it succeeded is recorded in `App::cleanup_uncertain`.
    Finished,
}

impl InputEnded {
    /// The release state comes first, since the line is cut off at the width; a failed release is shown by the uncertain-cleanup warning before this.
    fn line(&self, cleanup_uncertain: bool) -> String {
        match (self.release, cleanup_uncertain) {
            (Release::Pending, _) => format!("Releasing inputs; input mode ended ({})", self.cause),
            (Release::Finished, false) => {
                format!("Inputs released; input mode ended ({})", self.cause)
            }
            (Release::Finished, true) => format!("Input mode ended ({})", self.cause),
        }
    }
}

/// Shown while `i` waits for a pending release; replaced when the release finishes if it is still showing.
const RELEASE_WAIT: &str = "Inputs are still being released; try again shortly";

/// The message that replaces input mode's entry message when input mode ends.
const INPUT_MODE_ENDED: &str = "Input mode ended; keys are client commands again";

/// The outcome of reading capabilities and status, applied to an [`App`] by [`App::apply_refresh`] (or [`App::finish_refresh`], which also waits for any release it requires).
pub struct Refresh {
    capabilities: kvm_core::Result<Capabilities>,
    status: kvm_core::Result<DeviceStatus>,
    /// The app's count of control outcomes when the read started.
    controls_before: u64,
}

impl Refresh {
    /// Reads capabilities and then status. Status is read whatever the status capability says, since each subsystem reports its own support. It changes no interface state, so it can run while input is handled.
    pub async fn read(device: &dyn Device, controls_before: u64) -> Self {
        let capabilities = device.capabilities().await;
        let status = device.status().await;
        Self {
            capabilities,
            status,
            controls_before,
        }
    }
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
            refresh_requested: false,
            refreshing: false,
            refresh_note: "Device state not read yet".into(),
            controls_completed: 0,
            input_entry_message: None,
            input_ended: None,
            cleanup_uncertain: None,
            releases_started: 0,
            releases_recorded: 0,
            background_release: None,
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

    /// Refreshes capability and status information through the shared device API, waiting for the read; the event loop instead runs [`Refresh::read`] as its own task, applies it with [`App::apply_refresh`] and runs any release it requires as another task.
    pub async fn refresh(&mut self, device: &dyn Device) {
        let refresh = Refresh::read(device, self.controls_completed).await;
        self.finish_refresh(device, refresh).await;
    }

    /// Applies a completed read and, when it ends input mode, releases every tracked input, waiting for the release. The event loop uses [`App::apply_refresh`] instead and releases in the background.
    pub async fn finish_refresh(&mut self, device: &dyn Device, refresh: Refresh) {
        if self.apply_refresh(refresh) {
            let number = self.begin_release();
            let release = device.release_inputs().await;
            self.input_release_finished(number, release);
        }
    }

    /// Applies a completed read, returning whether it ended input mode, in which case the caller must release every tracked input and report it with [`App::input_release_finished`].
    ///
    /// The read's outcome goes on the device-state line, so a command being typed, a confirmation prompt or a control's outcome stays the message; the message is replaced only while it is still input mode's own entry text, which would otherwise go on claiming input mode is on.
    ///
    /// Input mode's key and mouse handling (releases included) is gated on the keyboard and mouse capabilities and the mouse mode it was entered under, so those must not change under it: when the read reports different values for them, or fails to read capabilities, input mode ends and every tracked input is to be released, as `F12` does. Other capabilities, such as live video becoming proven once frames arrive, are applied without interrupting input mode. Capabilities are written nowhere else, so while input mode lasts its gates stay those it was entered under.
    pub fn apply_refresh(&mut self, refresh: Refresh) -> bool {
        self.refreshing = false;
        let mut errors = Vec::new();
        let capabilities = refresh
            .capabilities
            .map_err(|error| errors.push(error.to_string()))
            .ok();
        let mut ended = false;
        self.clear_released_notice();
        if self.control
            && input_gates(capabilities.as_ref()) != input_gates(self.capabilities.as_ref())
        {
            ended = true;
            self.leave_input_mode();
            self.input_ended = Some(InputEnded {
                cause: if capabilities.is_none() {
                    "capabilities could not be read"
                } else {
                    "keyboard or mouse support changed"
                },
                release: Release::Pending,
            });
        }
        self.capabilities = capabilities;
        self.status = refresh
            .status
            .map_err(|error| errors.push(error.to_string()))
            .ok();
        let predates = if refresh.controls_before < self.controls_completed {
            " (read before the last control; r refreshes)"
        } else {
            ""
        };
        self.refresh_note = if errors.is_empty() {
            format!("Device state read{predates}")
        } else {
            format!("Device state read failed: {}{predates}", errors.join("; "))
        };
        ended
    }

    /// Drops the notice once its release has finished, so it does not sit in front of every later outcome. A pending release keeps it, since `i` waits on it; an uncertain cleanup is kept separately, in `cleanup_uncertain`.
    fn clear_released_notice(&mut self) {
        if self
            .input_ended
            .as_ref()
            .is_some_and(|ended| ended.release == Release::Finished)
        {
            self.input_ended = None;
        }
    }

    /// Records a release's result: a failure sets the uncertain-cleanup warning and only a success clears it, as `Device::release_inputs` reports a success only once no earlier input is unconfirmed.
    ///
    /// `number` is the release's number from `begin_release`; a result older than one already recorded is ignored, since the newer release saw the device later.
    fn record_release(&mut self, number: u64, release: &kvm_core::Result<()>) {
        if number < self.releases_recorded {
            return;
        }
        self.releases_recorded = number;
        match release {
            Ok(()) => self.cleanup_uncertain = None,
            Err(error) => self.cleanup_uncertain = Some(error.to_string()),
        }
    }

    /// Numbers a release as it starts, so its result can be ordered against others.
    fn begin_release(&mut self) -> u64 {
        self.releases_started += 1;
        self.releases_started
    }

    /// Releases every tracked input and records the result; every release the client makes goes through here, or through `input_release_finished` for the one a refresh requires.
    async fn release_tracked(&mut self, device: &dyn Device) -> kvm_core::Result<()> {
        let number = self.begin_release();
        let release = device.release_inputs().await;
        self.record_release(number, &release);
        release
    }

    /// Whether `i` would enter input mode with the capabilities shown.
    fn input_available(&self) -> bool {
        self.capabilities.as_ref().is_some_and(|c| {
            c.keyboard == Capability::Supported || c.mouse == Capability::Supported
        })
    }

    /// Records how the release after a refresh ended input mode went. A failed release also goes at the start of the message, which wraps, since a key or button may still be held on the remote host; a waiting `i`'s message is replaced either way.
    pub fn input_release_finished(&mut self, number: u64, release: kvm_core::Result<()>) {
        self.record_release(number, &release);
        let waiting = self.message == RELEASE_WAIT;
        let Some(ended) = &mut self.input_ended else {
            return;
        };
        ended.release = Release::Finished;
        match release {
            Ok(()) => {
                if waiting {
                    self.message = if self.input_available() {
                        "Inputs released; i enters input mode".into()
                    } else {
                        "Inputs released".into()
                    };
                }
            }
            Err(error) => {
                // Short, so a message written earlier (a control's outcome) still fits beside it; the device-state line carries it durably.
                let warning = format!("Cleanup uncertain: {error}");
                self.message = if waiting || self.message.is_empty() {
                    warning
                } else {
                    format!("{warning}. {}", self.message)
                };
            }
        }
    }

    /// Whether a release requested when a refresh ended input mode is still running; input mode is not entered meanwhile, since the release would also release anything pressed after it.
    fn input_release_pending(&self) -> bool {
        self.input_ended
            .as_ref()
            .is_some_and(|ended| ended.release == Release::Pending)
    }

    /// Leaves input mode, replacing the message if it is still the entry text, so the screen never says input mode is on after it has ended.
    fn leave_input_mode(&mut self) {
        self.control = false;
        self.clear_input_state();
        if self.input_entry_message.take().as_ref() == Some(&self.message) {
            self.message = INPUT_MODE_ENDED.into();
        }
    }

    /// The device-state line, cut off at the width, so it is ordered by what the user must see: an uncertain cleanup first, then an end of input mode, then a read in flight or the outcome of the last one.
    fn device_state_line(&self) -> String {
        let mut parts = Vec::new();
        if self.cleanup_uncertain.is_some() {
            parts.push(CLEANUP_UNCERTAIN.to_owned());
        }
        if let Some(ended) = &self.input_ended {
            parts.push(ended.line(self.cleanup_uncertain.is_some()));
        }
        parts.push(if self.refreshing {
            "Refreshing device state".to_owned()
        } else {
            self.refresh_note.clone()
        });
        parts.join("; ")
    }

    /// Records a control's outcome as the message, so it is not replaced by a refresh, and marks the device state shown as predating it.
    fn control_finished(&mut self, outcome: kvm_core::Result<kvm_core::ControlOutcome>) {
        self.controls_completed += 1;
        self.message = match outcome {
            Ok(kvm_core::ControlOutcome::ObservedMatching) => {
                "Request acknowledged; device state matches (causation not established)".into()
            }
            Ok(kvm_core::ControlOutcome::Unverified) => {
                "Request acknowledged; outcome not verified".into()
            }
            Err(error) => error.to_string(),
        };
        if !self.refreshing && !self.refresh_note.contains("read before the last control") {
            self.refresh_note
                .push_str(" (read before the last control; r refreshes)");
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
                if let Err(error) = self.release_tracked(device).await {
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
                if let Err(error) = self.release_tracked(device).await {
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
                                let _ = self.release_tracked(device).await;
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
                            let cleanup = self.release_tracked(device).await;
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
                    let cleanup = self.release_tracked(device).await;
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
                            let _ = self.release_tracked(device).await;
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
            let cleanup = self.release_tracked(device).await;
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
                self.leave_input_mode();
                if let Err(error) = self.release_tracked(device).await {
                    self.message = error.to_string();
                }
            }
            KeyCode::Char('q') if !self.control => return false,
            KeyCode::Char('r') if !self.control => self.refresh_requested = true,
            KeyCode::Char('i') if !self.control && self.input_release_pending() => {
                self.message = RELEASE_WAIT.into();
            }
            KeyCode::Char('i') if !self.control => {
                if self.input_available() {
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
                    // Entering input mode settles nothing: the uncertain-cleanup warning stays, and the message repeats it.
                    if self.cleanup_uncertain.is_some() {
                        self.message
                            .push_str("; cleanup is uncertain, earlier inputs may still be held");
                    }
                    self.input_entry_message = Some(self.message.clone());
                    self.input_ended = None;
                } else if self.capabilities.is_none() && self.refreshing {
                    self.message = "Capabilities are still being read".into();
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
                                        let _ = self.release_tracked(device).await;
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
                            let cleanup = self.release_tracked(device).await;
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
                        let cleanup = self.release_tracked(device).await;
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

/// Device-reported text with control and bidirectional formatting characters escaped, so a device cannot send terminal control sequences or reorder the status display.
fn untrusted(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if kvm_core::is_terminal_unsafe(c) {
                c.escape_unicode().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

fn shown<T>(value: &Reported<T>, show: impl FnOnce(&T) -> String) -> String {
    match value {
        Reported::Supported(value) => show(value),
        Reported::Unsupported => "unsupported".into(),
        Reported::Unknown => "unknown".into(),
    }
}

fn shown_flag(value: &Reported<bool>, on: &str, off: &str) -> String {
    shown(value, |value| if *value { on } else { off }.to_owned())
}

fn shown_text(value: &Reported<String>) -> String {
    shown(value, |text| untrusted(text))
}

/// One line per subsystem of a typed status, each value shown as reported, `unsupported` or `unknown`.
fn status_summary(status: &DeviceStatus) -> String {
    let system = shown(&status.info, |info| {
        format!(
            "KVMD {}, streamer {}, host {}",
            shown_text(&info.kvmd_version),
            shown_text(&info.streamer_version),
            shown_text(&info.host)
        )
    });
    let hid = shown(&status.hid, |hid| {
        format!(
            "{}, keyboard {}, mouse {}",
            shown_flag(&hid.online, "online", "offline"),
            shown(&hid.keyboard, |keyboard| shown_flag(
                &keyboard.online,
                "online",
                "offline"
            )),
            shown(&hid.mouse, |mouse| format!(
                "{} ({:?})",
                shown_flag(&mouse.online, "online", "offline"),
                mouse.mode
            ))
        )
    });
    let video = shown(&status.streamer, |streamer| {
        format!(
            "source {}, clients {}",
            shown(&streamer.source, |source| format!(
                "{} {}",
                shown_flag(&source.online, "online", "without signal"),
                shown(&source.resolution, |resolution| format!(
                    "{}x{}",
                    resolution.width, resolution.height
                ))
            )),
            shown(&streamer.clients, u32::to_string)
        )
    });
    let power = shown(&status.atx, |atx| {
        format!(
            "power LED {}, HDD LED {}, {}",
            shown_flag(&atx.power_led, "on", "off"),
            shown_flag(&atx.hdd_led, "on", "off"),
            shown_flag(&atx.busy, "button press in progress", "idle")
        )
    });
    let switch = shown(&status.switch, |switch| {
        format!(
            "active port {}, ports {}, {}",
            shown(&switch.active_port, |port| port
                .map_or_else(|| "none".into(), |port| port.to_string())),
            shown(&switch.ports, |ports| ports
                .iter()
                .map(|port| port.port.to_string())
                .collect::<Vec<_>>()
                .join(" ")),
            shown_flag(&switch.synced, "synced", "not synced")
        )
    });
    let gpio = shown(&status.gpio, |gpio| {
        shown(&gpio.outputs, |outputs| {
            if outputs.is_empty() {
                return "no outputs".into();
            }
            outputs
                .iter()
                .map(|output| {
                    format!(
                        "{} {}",
                        untrusted(&output.channel),
                        shown_flag(&output.state, "on", "off")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
    });
    let media = shown(&status.media, |media| {
        format!(
            "drive {}, images {}",
            shown(&media.drive, |drive| format!(
                "{}{}",
                shown_flag(&drive.connected, "connected", "disconnected"),
                shown(&drive.image, |image| image.as_ref().map_or_else(
                    || ", no image".into(),
                    |image| format!(", image {}", untrusted(&image.name))
                ))
            )),
            shown(&media.images, |images| images.len().to_string())
        )
    });
    format!(
        "System: {system}\nHID: {hid}\nVideo: {video}\nPower: {power}\nSwitch: {switch}\nGPIO: {gpio}\nMedia: {media}"
    )
}

/// The capabilities input mode's key and mouse handling is gated on; `None` when capabilities are unread.
fn input_gates(
    capabilities: Option<&Capabilities>,
) -> Option<(Capability, Capability, kvm_core::MouseMode)> {
    capabilities.map(|c| (c.keyboard, c.mouse, c.mouse_mode))
}

/// Rows inside the controls footer's border given to the key help and the message, which wrap together; the device-state line has a row of its own below them.
const FOOTER_TEXT_ROWS: u16 = 3;
/// The controls footer: its text rows, the device-state row and the top and bottom border.
const FOOTER_HEIGHT: u16 = FOOTER_TEXT_ROWS + 1 + 2;

/// Splits the screen into the capability header, the video or status area and the controls footer.
fn screen_areas(area: ratatui::layout::Rect) -> std::rc::Rc<[ratatui::layout::Rect]> {
    Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(FOOTER_HEIGHT),
    ])
    .split(area)
}

/// Draws the current state using any ratatui backend, including its test backend.
pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let areas = screen_areas(frame.area());
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
            Some(status) => status_summary(status),
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
    let block = Block::default().title("Controls").borders(Borders::ALL);
    let inner = block.inner(areas[2]);
    frame.render_widget(block, areas[2]);
    // The help and the message wrap into the rows above; the device-state line keeps its own last row however long they are, and is cut off at the width rather than wrapped out of view.
    let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
    frame.render_widget(
        Paragraph::new(format!("{help}\n{}", app.message)).wrap(Wrap { trim: false }),
        rows[0],
    );
    frame.render_widget(Paragraph::new(app.device_state_line()), rows[1]);
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
            cell_size: capabilities::DEFAULT_CELL_SIZE,
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

/// Cell size from the window size ioctl, which may omit pixel dimensions.
fn window_cell_size() -> Option<(u16, u16)> {
    match crossterm::terminal::window_size() {
        Ok(size)
            if size.columns > 0
                && size.rows > 0
                && size.width >= size.columns
                && size.height >= size.rows =>
        {
            Some((size.width / size.columns, size.height / size.rows))
        }
        _ => None,
    }
}

/// Decides the image protocol and keyboard enhancement, querying the terminal when both standard input and standard output are terminals. Must run after entering the alternate screen and raw mode, so that replies are not echoed and the query is not left on the user's screen.
async fn detect_capabilities(
    environment: &capabilities::TerminalEnvironment,
    probe_deadline: Duration,
) -> io::Result<(ImagePicker, bool)> {
    #[cfg(unix)]
    let report = {
        use std::io::IsTerminal;
        if io::stdin().is_terminal() && io::stdout().is_terminal() {
            let query = capabilities::query(environment.tmux, environment.image_override.is_none());
            Some(capabilities::probe(&query, probe_deadline).await?)
        } else {
            None
        }
    };
    #[cfg(not(unix))]
    let report = {
        let _ = probe_deadline;
        None
    };
    let decision = capabilities::decide(environment, report.as_ref(), window_cell_size());
    Ok((
        ImagePicker {
            cell_size: decision.cell_size,
            protocol: decision.protocol,
            tmux: decision.tmux,
        },
        decision.enhanced_keyboard,
    ))
}

/// Runs the TUI for an already authenticated device. Cancellation schedules best-effort input release while the runtime remains active.
///
/// SIGTERM and SIGINT end the session at any point after the terminal is set up, capability detection included, and the terminal is restored before this returns; the runtime keeps handling both signals for the rest of the process.
///
/// # Errors
/// Returns an error when `KVM_TUI_IMAGE_PROTOCOL` is invalid, the terminal cannot be set up or restored, a termination signal arrives, or input release fails.
pub async fn run(device: std::sync::Arc<dyn Device>) -> io::Result<()> {
    run_with_probe_deadline(device, capabilities::PROBE_DEADLINE).await
}

async fn run_with_probe_deadline(
    device: std::sync::Arc<dyn Device>,
    probe_deadline: Duration,
) -> io::Result<()> {
    let environment = capabilities::TerminalEnvironment::from_env()?;
    let mut input_cleanup = InputCleanup(Some(device.clone()));
    #[cfg(unix)]
    let mut termination = Termination::new()?;
    let mut guard = TerminalGuard::enter(SystemTerminal)?;
    let session = async {
        let (picker, enhanced) = detect_capabilities(&environment, probe_deadline).await?;
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
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        run_loop(&mut terminal, device.clone(), &picker, enhanced, |_| {
            if event::poll(Duration::from_millis(25))? {
                Ok(Some(event::read()?))
            } else {
                Ok(None)
            }
        })
        .await
    };
    #[cfg(unix)]
    let result = termination.wait(session).await;
    #[cfg(not(unix))]
    let result = session.await;
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

/// SIGTERM and SIGINT handlers registered before the terminal is set up, so that a signal at any later point ends the session through terminal restoration rather than the default disposition.
#[cfg(unix)]
struct Termination {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Termination {
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
        })
    }

    /// Runs `run` until it completes or a termination signal arrives, dropping it in the latter case.
    async fn wait<F: std::future::Future<Output = io::Result<()>>>(
        &mut self,
        run: F,
    ) -> io::Result<()> {
        tokio::select! {
            result = run => result,
            _ = self.terminate.recv() => Err(io::Error::other("terminated by SIGTERM")),
            _ = self.interrupt.recv() => Err(io::Error::other("interrupted by SIGINT")),
        }
    }
}

async fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    shared: std::sync::Arc<dyn Device>,
    picker: &ImagePicker,
    enhanced: bool,
    mut next_event: impl FnMut(&Terminal<B>) -> io::Result<Option<Event>>,
) -> io::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let device = shared.as_ref();
    let mut app = App {
        enhanced_keyboard: enhanced,
        cell_size: picker.cell_size,
        message: "Press v to open live video".into(),
        ..App::default()
    };
    /// A refresh read running as its own task, aborted when dropped (on quit, on a termination signal or when the loop ends otherwise).
    struct RefreshTask(tokio::task::JoinHandle<Refresh>);
    impl Drop for RefreshTask {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    // A device read can take up to its routes' deadlines, so it runs as its own task: it keeps making progress while the loop awaits video, input or cleanup calls (which would otherwise stall it and could turn a reply that arrived into a timeout), keys and `q` are handled meanwhile, and at most one read is in flight. Its outcome goes on the device-state line, and keyboard or mouse capabilities that change (or cannot be read) under input mode end it (`App::apply_refresh`), with the release of tracked inputs run as its own task too, since it can take as long as a request to an unreachable device.
    let start_refresh = |app: &mut App| {
        app.refreshing = true;
        let device = shared.clone();
        let controls_before = app.controls_completed;
        RefreshTask(tokio::spawn(async move {
            Refresh::read(device.as_ref(), controls_before).await
        }))
    };
    let mut refresh_task = Some(start_refresh(&mut app));
    /// The release that a refresh ending input mode requires, aborted when dropped like the refresh read.
    struct ReleaseTask(tokio::task::JoinHandle<kvm_core::Result<()>>);
    impl Drop for ReleaseTask {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let mut release_task: Option<ReleaseTask> = None;
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
        if refresh_task
            .as_ref()
            .is_some_and(|task| task.0.is_finished())
            && let Some(mut task) = refresh_task.take()
        {
            match (&mut task.0).await {
                Ok(refresh) => {
                    // Releasing can take as long as a request to an unreachable device, so it runs as its own task too; the release that ends the run still releases if quitting aborts this one.
                    if app.apply_refresh(refresh) {
                        let device = shared.clone();
                        app.background_release = Some(app.begin_release());
                        release_task = Some(ReleaseTask(tokio::spawn(async move {
                            device.release_inputs().await
                        })));
                    }
                }
                Err(error) => {
                    app.refreshing = false;
                    app.clear_released_notice();
                    app.refresh_note = format!("Device state read failed: {error}");
                }
            }
        }
        if release_task
            .as_ref()
            .is_some_and(|task| task.0.is_finished())
            && let Some(mut task) = release_task.take()
        {
            let number = app
                .background_release
                .take()
                .expect("a background release is numbered when it starts");
            app.input_release_finished(
                number,
                match (&mut task.0).await {
                    Ok(release) => release,
                    Err(error) => Err(kvm_core::Error::Transport(format!(
                        "release task failed: {error}"
                    ))),
                },
            );
        }
        if std::mem::take(&mut app.refresh_requested) {
            if refresh_task.is_some() {
                app.message = "A refresh is already in progress".into();
            } else {
                refresh_task = Some(start_refresh(&mut app));
            }
        }
        if let Some(request) = &mut control_request {
            let outcome = tokio::select! { outcome = request.as_mut() => Some(outcome), _ = tokio::time::sleep(Duration::from_millis(1)) => None };
            if let Some(outcome) = outcome {
                control_request = None;
                app.control_finished(outcome);
            }
        }
        if let Some(receiver) = &mut video_rx
            && receiver.has_changed().is_err()
        {
            let item = receiver.borrow_and_update().clone();
            app.video = None;
            app.leave_input_mode();
            app.video_message = match item {
                Some(Err(error)) => error,
                _ => "Video stream closed".into(),
            };
            if let Err(error) = app.release_tracked(device).await {
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
                    let areas = screen_areas(frame.area());
                    if let Some(area) =
                        video_viewport(areas[1], image.width(), image.height(), app.cell_size)
                        && let Some(protocol) = &mut image_protocol
                    {
                        frame.render_stateful_widget(
                            ratatui_image::StatefulImage::default()
                                .resize(ratatui_image::Resize::Scale(None)),
                            area.cells,
                            protocol,
                        );
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
                    app.message = match app.release_tracked(device).await {
                        Ok(()) if control_request.is_some() => {
                            "Focus lost; inputs released; submitted request remains in flight"
                                .into()
                        }
                        Ok(()) => "Focus lost; inputs released".into(),
                        Err(error) => format!("Focus lost; input cleanup uncertain: {error}"),
                    };
                }
                Event::Mouse(mouse) => {
                    let areas = screen_areas(terminal.size().map_err(io::Error::other)?.into());
                    app.mouse(device, mouse, areas[1]).await;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod live_view_e2e;

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// The screen's text, for event closures that wait on what is drawn.
    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    /// Gives `next` no events until the start-up refresh, which runs as its own task, has shown the capabilities it read, so a test's first key sees them. It watches the capability header, which every terminal size used here shows in full, because the device-state row is cut off at narrow widths and reads the same before a read has started as while it runs; the fake devices it is used with always report capabilities.
    fn after_start_up_refresh(
        mut next: impl FnMut(&Terminal<TestBackend>) -> io::Result<Option<Event>>,
    ) -> impl FnMut(&Terminal<TestBackend>) -> io::Result<Option<Event>> {
        let started = std::time::Instant::now();
        // Only the start-up read is waited for; a later read that loses the capabilities is the test's to observe.
        let mut shown = false;
        move |terminal| {
            if !shown && screen_text(terminal).contains("Capabilities unknown") {
                // The fake devices answer at once, so this bound only turns a regression into a failure instead of a hang.
                assert!(
                    started.elapsed() < START_UP_READ_BOUND,
                    "the start-up refresh was never applied"
                );
                std::thread::yield_now();
                return Ok(None);
            }
            shown = true;
            next(terminal)
        }
    }

    /// The bound on waiting for the start-up refresh in tests. The fake devices answer at once, so the wait is a few scheduler turns; the bound is far above any scheduling delay on a loaded CI runner and only turns a regression into a failure instead of a hang. A test that puts a shorter timeout on its whole run fails at that timeout first, which also fails the test.
    const START_UP_READ_BOUND: Duration = Duration::from_secs(10);

    /// A scripted device. Its `fail_key` and `fail_mouse_release` return `AmbiguousInput` while `release_inputs` still succeeds unless `fail_cleanup` is set, which `Device::release_inputs` does not allow an adapter (after an ambiguous input it must keep failing); tests use that to check how the client handles each result on its own, and `fail_cleanup` models a device that keeps the rule.
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
        /// Reports the status capability unknown, as the adapter does when the info or streamer route is denied.
        status_capability_unknown: bool,
        /// Never answers a status read, as a device whose route never answers holds a read until its timeout.
        status_hangs: bool,
        /// While set, the capability read waits for `capabilities_gate` and then fails, as a read of a device that dropped off does.
        fail_capabilities: std::sync::atomic::AtomicBool,
        capabilities_gate: tokio::sync::Notify,
        /// Panics in `release_inputs`, so the task running it ends with a join error.
        panic_release: bool,
        /// Holds the stream's failure after its frame until notified.
        stream_close: Option<std::sync::Arc<tokio::sync::Notify>>,
        status_reads: std::sync::atomic::AtomicUsize,
        /// Makes the status read wait until video is requested, and video wait until the status read has finished, so the two can only both finish if the read progresses while the loop awaits video.
        video_waits_for_status: bool,
        video_requested: tokio::sync::Notify,
        status_finished: tokio::sync::Notify,
    }
    struct FakeStream {
        jpeg: Option<Vec<u8>>,
        fail_after_frame: bool,
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
        /// When set, the failure after the frame waits for this, so a test can act while the stream is open.
        close_gate: Option<std::sync::Arc<tokio::sync::Notify>>,
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
                if let Some(gate) = &self.close_gate {
                    gate.notified().await;
                }
                Err(kvm_core::Error::Transport("stream failed".into()))
            } else {
                std::future::pending().await
            }
        }
    }
    #[async_trait::async_trait]
    impl Device for FakeDevice {
        async fn video(&self) -> kvm_core::Result<Box<dyn kvm_core::VideoStream>> {
            if self.video_waits_for_status {
                self.video_requested.notify_one();
                self.status_finished.notified().await;
                return Err(kvm_core::Error::Unsupported("video".into()));
            }
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
                    close_gate: self.stream_close.clone(),
                })),
                None => Err(kvm_core::Error::Unsupported("video".into())),
            }
        }
        async fn status(&self) -> kvm_core::Result<DeviceStatus> {
            self.status_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.status_hangs {
                std::future::pending::<()>().await;
            }
            if self.video_waits_for_status {
                self.video_requested.notified().await;
                self.status_finished.notify_one();
            }
            Ok(DeviceStatus {
                info: Reported::Unknown,
                hid: Reported::Unknown,
                streamer: Reported::Unknown,
                atx: Reported::Unsupported,
                switch: Reported::Unknown,
                gpio: Reported::Unknown,
                media: Reported::Unknown,
            })
        }
        async fn capabilities(&self) -> kvm_core::Result<Capabilities> {
            if self
                .fail_capabilities
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                self.capabilities_gate.notified().await;
                return Err(kvm_core::Error::Transport("device request: timeout".into()));
            }
            Ok(Capabilities {
                status: if self.status_capability_unknown {
                    Capability::Unknown
                } else {
                    Capability::Supported
                },
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
            assert!(!self.panic_release, "synthetic release failure");
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
        let device = std::sync::Arc::new(FakeDevice {
            video_frame: Some(jpeg),
            fail_video_after_frame: true,
            ..FakeDevice::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        let mut count = 0;
        run_loop(
            &mut terminal,
            device.clone(),
            &ImagePicker::halfblocks(),
            false,
            after_start_up_refresh(|_| {
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
            }),
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
        let decoded = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
            .unwrap()
            .to_rgb8();
        // A uniform halfblock is encoded as a coloured space, not necessarily a block glyph.
        let [red, green, blue] = decoded.get_pixel(0, 0).0;
        let frame_colour = ratatui::style::Color::Rgb(red, green, blue);
        let device = std::sync::Arc::new(FakeDevice {
            video_frame: Some(jpeg),
            ..FakeDevice::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        let mut rounds = 0;
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_loop(
                &mut terminal,
                device.clone(),
                &ImagePicker::halfblocks(),
                false,
                after_start_up_refresh(|terminal| {
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
                        .any(|cell| cell.bg == frame_colour)
                    {
                        Ok(Some(Event::Key(crossterm::event::KeyEvent::new(
                            KeyCode::Char('q'),
                            crossterm::event::KeyModifiers::NONE,
                        ))))
                    } else {
                        std::thread::yield_now();
                        Ok(None)
                    }
                }),
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
                .any(|cell| cell.bg == frame_colour)
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
        let device = std::sync::Arc::new(FakeDevice {
            block_control: true,
            ..FakeDevice::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        let mut events = ":power off\ny".chars();
        let mut focus_sent = false;
        run_loop(
            &mut terminal,
            device.clone(),
            &ImagePicker::halfblocks(),
            false,
            after_start_up_refresh(|_| {
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
            }),
        )
        .await
        .unwrap();
        assert_eq!(*device.calls.lock().unwrap(), ["power", "release"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn focus_loss_cancels_pending_confirmation() {
        let device = std::sync::Arc::new(FakeDevice::default());
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
            device.clone(),
            &ImagePicker::halfblocks(),
            false,
            after_start_up_refresh(|_| {
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
            }),
        )
        .await
        .unwrap();
        assert_eq!(*device.calls.lock().unwrap(), ["release"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn video_key_is_routed_to_command_and_confirmation() {
        let device = std::sync::Arc::new(FakeDevice::default());
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        let sequence = [
            ':', 'p', 'o', 'w', 'e', 'r', ' ', 'o', 'f', 'f', '\n', 'v', 'q',
        ];
        let mut events = sequence.into_iter();
        run_loop(
            &mut terminal,
            device.clone(),
            &ImagePicker::halfblocks(),
            false,
            after_start_up_refresh(|_| {
                let c = events.next().expect("quit event");
                Ok(Some(Event::Key(crossterm::event::KeyEvent::new(
                    if c == '\n' {
                        KeyCode::Enter
                    } else {
                        KeyCode::Char(c)
                    },
                    crossterm::event::KeyModifiers::NONE,
                ))))
            }),
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
        let device = std::sync::Arc::new(FakeDevice {
            video_frame: Some(jpeg),
            fail_restart: true,
            ..FakeDevice::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        let mut count = 0;
        run_loop(
            &mut terminal,
            device.clone(),
            &ImagePicker::halfblocks(),
            false,
            after_start_up_refresh(|_| {
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
            }),
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
        let device = std::sync::Arc::new(FakeDevice::default());
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        let mut step = 0;
        run_loop(
            &mut terminal,
            device.clone(),
            &ImagePicker::halfblocks(),
            false,
            after_start_up_refresh(|_| {
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
            }),
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

    // Breaks if a refresh blocks input handling: with a status read that never finishes, 'q' would never be read.
    #[tokio::test]
    async fn quit_works_while_a_refresh_is_in_flight() {
        let device = std::sync::Arc::new(FakeDevice {
            status_hangs: true,
            ..FakeDevice::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let mut events = 0;
        let key = |code| {
            Some(Event::Key(crossterm::event::KeyEvent::new(
                code,
                crossterm::event::KeyModifiers::NONE,
            )))
        };
        // The status read never answers, so finishing at all shows the keys were handled during it; the timeout only keeps a regression from hanging the suite.
        tokio::time::timeout(
            Duration::from_secs(10),
            run_loop(
                &mut terminal,
                device.clone(),
                &ImagePicker::halfblocks(),
                false,
                |terminal| {
                    events += 1;
                    Ok(match events {
                        1 => {
                            let content: String = terminal
                                .backend()
                                .buffer()
                                .content()
                                .iter()
                                .map(|cell| cell.symbol())
                                .collect();
                            assert!(content.contains("Refreshing device state"));
                            key(KeyCode::Char('r'))
                        }
                        2 => {
                            let content: String = terminal
                                .backend()
                                .buffer()
                                .content()
                                .iter()
                                .map(|cell| cell.symbol())
                                .collect();
                            assert!(content.contains("A refresh is already in progress"));
                            key(KeyCode::Char('q'))
                        }
                        _ => None,
                    })
                },
            ),
        )
        .await
        .expect("quit must not wait for the refresh")
        .unwrap();
        // The second 'r' did not start a second read while the first was in flight.
        assert_eq!(
            device
                .status_reads
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    // Breaks if the refresh only progresses while the loop polls it: the loop then awaits video, which waits for the read, and neither finishes.
    #[tokio::test]
    async fn the_refresh_progresses_while_the_loop_awaits_a_device_call() {
        let device = std::sync::Arc::new(FakeDevice {
            video_waits_for_status: true,
            ..FakeDevice::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let mut events = 0;
        let key = |code| {
            Some(Event::Key(crossterm::event::KeyEvent::new(
                code,
                crossterm::event::KeyModifiers::NONE,
            )))
        };
        // The timeout only keeps a regression from hanging the suite.
        tokio::time::timeout(
            Duration::from_secs(10),
            run_loop(
                &mut terminal,
                device.clone(),
                &ImagePicker::halfblocks(),
                false,
                |terminal| {
                    events += 1;
                    Ok(if events == 1 {
                        key(KeyCode::Char('v'))
                    } else if screen_text(terminal).contains("Device state read") {
                        key(KeyCode::Char('q'))
                    } else {
                        std::thread::yield_now();
                        None
                    })
                },
            ),
        )
        .await
        .expect("the read and the video request must both finish")
        .unwrap();
    }

    fn fake_capabilities(keyboard: Capability) -> Capabilities {
        Capabilities {
            status: Capability::Supported,
            snapshot: Capability::Unknown,
            keyboard,
            mouse: Capability::Supported,
            mouse_mode: kvm_core::MouseMode::Absolute,
            live_video: Capability::Unknown,
            power: Capability::Supported,
            switch: Capability::Unknown,
            gpio: Capability::Unknown,
            media: Capability::Unknown,
        }
    }

    async fn read_with(
        device: &FakeDevice,
        capabilities: kvm_core::Result<Capabilities>,
        controls_before: u64,
    ) -> Refresh {
        Refresh {
            capabilities,
            status: device.status().await,
            controls_before,
        }
    }

    // Breaks if a refresh that fails to read capabilities during input mode leaves the held button pressed: input mode must end and the inputs be released.
    #[tokio::test]
    async fn a_refresh_that_loses_capabilities_ends_input_mode_and_releases_a_held_button() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        assert!(app.control);
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
        let refresh = read_with(
            &device,
            Err(kvm_core::Error::Transport("timeout".into())),
            0,
        )
        .await;
        app.finish_refresh(&device, refresh).await;
        assert!(!app.control);
        assert!(app.capabilities.is_none());
        assert_eq!(
            device.calls.lock().unwrap().last().map(String::as_str),
            Some("release")
        );
        assert!(
            app.device_state_line()
                .starts_with("Inputs released; input mode ended (capabilities could not be read)")
        );
    }

    // Breaks if a capability that input mode is not gated on ends it: live video becomes proven once frames arrive, and ending input mode then would turn the user's next keystrokes into client commands.
    #[tokio::test]
    async fn a_refresh_changing_only_other_capabilities_keeps_input_mode() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        let mut proven = fake_capabilities(Capability::Supported);
        proven.live_video = Capability::Supported;
        proven.snapshot = Capability::Supported;
        let refresh = read_with(&device, Ok(proven.clone()), 0).await;
        app.finish_refresh(&device, refresh).await;
        assert!(app.control);
        assert!(device.calls.lock().unwrap().is_empty());
        // The other fields are still applied.
        assert_eq!(app.capabilities, Some(proven));
    }

    // Breaks if ending input mode replaces a control's outcome that arrived during the read.
    #[tokio::test]
    async fn ending_input_mode_keeps_a_control_outcome_in_the_message() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        app.control_finished(Ok(kvm_core::ControlOutcome::Unverified));
        let outcome = app.message.clone();
        let refresh = read_with(&device, Ok(fake_capabilities(Capability::Unsupported)), 0).await;
        app.finish_refresh(&device, refresh).await;
        assert!(!app.control);
        assert_eq!(app.message, outcome);
        assert!(
            app.device_state_line().starts_with(
                "Inputs released; input mode ended (keyboard or mouse support changed)"
            )
        );
    }

    fn with_mode(mode: kvm_core::MouseMode, mouse: Capability) -> Capabilities {
        Capabilities {
            mouse_mode: mode,
            mouse,
            ..fake_capabilities(Capability::Supported)
        }
    }

    // Breaks if input mode is gated on the keyboard alone: a change of only the mouse mode, or only the mouse capability, must also end input mode and release a held button.
    #[tokio::test]
    async fn a_mouse_or_mouse_mode_change_alone_ends_input_mode() {
        for changed in [
            with_mode(kvm_core::MouseMode::Relative, Capability::Supported),
            with_mode(kvm_core::MouseMode::Absolute, Capability::Unknown),
        ] {
            let device = FakeDevice::default();
            let mut app = App::default();
            app.refresh(&device).await;
            app.key(&device, KeyCode::Char('i')).await;
            app.video = Some(image::RgbImage::new(2, 4));
            app.mouse(
                &device,
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 1,
                    row: 1,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
                ratatui::layout::Rect::new(0, 0, 4, 4),
            )
            .await;
            // The button press reached the device, so the release that follows is what lifts it.
            assert!(
                device
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|call| call == "button:left:true")
            );
            let refresh = read_with(&device, Ok(changed.clone()), 0).await;
            app.finish_refresh(&device, refresh).await;
            assert!(!app.control, "{changed:?}");
            assert_eq!(
                device.calls.lock().unwrap().last().map(String::as_str),
                Some("release")
            );
        }
    }

    // Breaks if, once a failed read ends input mode, the rendered screen does not lead the device-state row with the notice or still says input mode is on.
    #[tokio::test]
    async fn an_ended_input_mode_is_visible_on_screen() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        let entry = app.message.clone();
        assert!(entry.contains("Input mode"));
        let failed = Refresh {
            capabilities: Err(kvm_core::Error::Transport("device request: timeout".into())),
            status: Err(kvm_core::Error::Transport("device request: timeout".into())),
            controls_before: 0,
        };
        app.finish_refresh(&device, failed).await;
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (1..buffer.area.width - 1)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        };
        let state_row = row(buffer.area.height - 2);
        assert!(
            state_row
                .starts_with("Inputs released; input mode ended (capabilities could not be read)"),
            "{state_row:?}"
        );
        let footer: String = (buffer.area.height - FOOTER_HEIGHT..buffer.area.height)
            .map(row)
            .collect();
        assert!(!footer.contains(&entry), "{footer:?}");
        assert!(footer.contains(INPUT_MODE_ENDED), "{footer:?}");
        // F12 leaves input mode the same way, replacing only the entry message.
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        assert_eq!(app.device_state_line(), "Device state read");
        app.key(&device, KeyCode::F(12)).await;
        assert_eq!(app.message, INPUT_MODE_ENDED);
        app.key(&device, KeyCode::Char('i')).await;
        app.control_finished(Ok(kvm_core::ControlOutcome::Unverified));
        let outcome = app.message.clone();
        app.key(&device, KeyCode::F(12)).await;
        assert_eq!(app.message, outcome);
    }

    // Breaks if input mode can be entered while the release a refresh requested is still running, which would release what is pressed next.
    #[test]
    fn input_mode_waits_for_a_pending_release() {
        let mut app = App {
            capabilities: Some(fake_capabilities(Capability::Supported)),
            input_ended: Some(InputEnded {
                cause: "capabilities could not be read",
                release: Release::Pending,
            }),
            ..App::default()
        };
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(app.key(&FakeDevice::default(), KeyCode::Char('i')));
        assert!(!app.control);
        assert!(app.message.contains("still being released"));
        let number = app.begin_release();
        app.input_release_finished(number, Ok(()));
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(app.key(&FakeDevice::default(), KeyCode::Char('i')));
        assert!(app.control);
        assert!(app.input_ended.is_none());
    }

    /// Drives `run_loop` until a refresh ends input mode, as a user does: waits for the start-up refresh, presses `r` with the capability read held at the fake's gate, presses `i` while that read is in flight (input mode opens under the start-up capabilities), then lets the read fail, which ends input mode; the remaining events come from `then`.
    fn end_input_mode_by_refresh(
        device: std::sync::Arc<FakeDevice>,
        mut then: impl FnMut(&Terminal<TestBackend>) -> Option<Event>,
    ) -> impl FnMut(&Terminal<TestBackend>) -> io::Result<Option<Event>> {
        let mut step = 0;
        let key = |code| {
            Some(Event::Key(crossterm::event::KeyEvent::new(
                code,
                crossterm::event::KeyModifiers::NONE,
            )))
        };
        after_start_up_refresh(move |terminal| {
            step += 1;
            Ok(match step {
                1 => {
                    device
                        .fail_capabilities
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    key(KeyCode::Char('r'))
                }
                2 => key(KeyCode::Char('i')),
                3 => {
                    // `i` was handled in the previous iteration, so input mode is on before the read fails.
                    device.capabilities_gate.notify_one();
                    None
                }
                _ => then(terminal),
            })
        })
    }

    // Breaks if the release a refresh requires is not run in the background, if `i` is accepted while it is pending, or if its completion is not shown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_release_a_refresh_requires_runs_through_the_loop() {
        let device = std::sync::Arc::new(FakeDevice {
            delayed_cleanup: true,
            ..FakeDevice::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let mut phase = 0;
        let release = device.clone();
        let key = |code| {
            Some(Event::Key(crossterm::event::KeyEvent::new(
                code,
                crossterm::event::KeyModifiers::NONE,
            )))
        };
        tokio::time::timeout(
            Duration::from_secs(10),
            run_loop(
                &mut terminal,
                device.clone(),
                &ImagePicker::halfblocks(),
                false,
                end_input_mode_by_refresh(device.clone(), move |terminal| {
                    let text = screen_text(terminal);
                    match phase {
                        // The release is waiting on the fake, so the loop is still drawing: try to re-enter input mode.
                        0 if text.contains("Releasing inputs; input mode ended") => {
                            phase = 1;
                            key(KeyCode::Char('i'))
                        }
                        1 if text.contains(RELEASE_WAIT) => {
                            phase = 2;
                            release.control_release.notify_one();
                            None
                        }
                        // The read that ended input mode lost the capabilities, so the message must not offer `i`.
                        2 if text.contains("Inputs released; input mode ended")
                            && !text.contains(RELEASE_WAIT)
                            && !text.contains("i enters input mode") =>
                        {
                            phase = 3;
                            key(KeyCode::Char('q'))
                        }
                        _ => {
                            std::thread::yield_now();
                            None
                        }
                    }
                }),
            ),
        )
        .await
        .expect("the loop must finish")
        .unwrap();
        assert_eq!(
            device
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| *call == "release")
                .count(),
            1
        );
    }

    // Breaks if quitting waits for a release that never finishes, or if a release task that fails is not shown as an uncertain cleanup.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pending_release_does_not_hold_quit_and_a_failed_one_is_shown() {
        let device = std::sync::Arc::new(FakeDevice {
            delayed_cleanup: true,
            ..FakeDevice::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            run_loop(
                &mut terminal,
                device.clone(),
                &ImagePicker::halfblocks(),
                false,
                end_input_mode_by_refresh(device.clone(), |terminal| {
                    if screen_text(terminal).contains("Releasing inputs") {
                        Some(Event::Key(crossterm::event::KeyEvent::new(
                            KeyCode::Char('q'),
                            crossterm::event::KeyModifiers::NONE,
                        )))
                    } else {
                        std::thread::yield_now();
                        None
                    }
                }),
            ),
        )
        .await
        .expect("quit must not wait for the release")
        .unwrap();
        // The release was still waiting at the fake when the loop ended.
        assert!(
            !device
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call == "release")
        );

        let device = std::sync::Arc::new(FakeDevice {
            panic_release: true,
            ..FakeDevice::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(30, 16)).unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            run_loop(
                &mut terminal,
                device.clone(),
                &ImagePicker::halfblocks(),
                false,
                end_input_mode_by_refresh(device.clone(), |terminal| {
                    if screen_text(terminal).contains("Cleanup uncertain") {
                        Some(Event::Key(crossterm::event::KeyEvent::new(
                            KeyCode::Char('q'),
                            crossterm::event::KeyModifiers::NONE,
                        )))
                    } else {
                        std::thread::yield_now();
                        None
                    }
                }),
            ),
        )
        .await
        .expect("a failed release must be shown")
        .unwrap();
    }

    fn state_row(app: &App, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 16)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (1..buffer.area.width - 1)
            .map(|x| buffer[(x, buffer.area.height - 2)].symbol())
            .collect()
    }

    fn failed_read() -> Refresh {
        Refresh {
            capabilities: Err(kvm_core::Error::Transport("device request: timeout".into())),
            status: Err(kvm_core::Error::Transport("device request: timeout".into())),
            controls_before: 0,
        }
    }

    // Breaks if an uncertain cleanup is cut off on narrow screens, or if a later refresh outcome removes it although nothing has been released.
    #[tokio::test]
    async fn an_uncertain_cleanup_stays_visible_until_a_release_succeeds() {
        let device = FakeDevice {
            fail_cleanup: true,
            ..FakeDevice::default()
        };
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        app.finish_refresh(&device, failed_read()).await;
        for width in [20, 30, 52, 80] {
            let row = state_row(&app, width);
            assert!(
                row.starts_with("Cleanup uncertain"),
                "width {width}: {row:?}"
            );
        }
        assert!(app.message.starts_with("Cleanup uncertain: "));
        // A later refresh outcome, failed or successful, leaves the warning in front.
        app.finish_refresh(&device, failed_read()).await;
        let later = read_with(&device, Ok(fake_capabilities(Capability::Unsupported)), 0).await;
        app.finish_refresh(&device, later).await;
        assert!(state_row(&app, 20).starts_with("Cleanup uncertain"));
    }

    // Breaks if a released notice keeps hiding later refresh outcomes, or if a later successful release does not settle an uncertain cleanup.
    #[tokio::test]
    async fn a_notice_gives_way_once_a_release_has_succeeded() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        app.finish_refresh(&device, failed_read()).await;
        assert!(app.device_state_line().starts_with("Inputs released"));
        let later = read_with(&device, Ok(fake_capabilities(Capability::Unsupported)), 0).await;
        app.finish_refresh(&device, later).await;
        assert_eq!(app.device_state_line(), "Device state read");

        // An uncertain cleanup is settled only by a later successful release, through the helper every release site uses.
        app.cleanup_uncertain = Some("cleanup failed".into());
        app.release_tracked(&device).await.unwrap();
        assert_eq!(app.device_state_line(), "Device state read");
    }

    fn failing_cleanup() -> FakeDevice {
        FakeDevice {
            fail_cleanup: true,
            ..FakeDevice::default()
        }
    }

    // Scenario A: a key request is ambiguous and the release after it fails; a control's outcome then replaces the message, and the warning must stay on the row.
    #[tokio::test]
    async fn a_failed_release_after_an_ambiguous_key_stays_on_screen() {
        let device = FakeDevice {
            fail_key: true,
            fail_cleanup: true,
            ..FakeDevice::default()
        };
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        app.key(&device, KeyCode::Char('a')).await;
        assert!(!app.control);
        app.control_finished(Ok(kvm_core::ControlOutcome::Unverified));
        assert!(!app.message.contains("cleanup"));
        for width in [20, 30, 52, 80] {
            assert!(
                state_row(&app, width).starts_with("Cleanup uncertain"),
                "width {width}"
            );
        }
    }

    // Scenario B, and `i` not clearing the warning: a refresh's release fails, `i` re-enters input mode, F12's release fails too and a control's outcome replaces the message.
    #[tokio::test]
    async fn entering_input_mode_keeps_an_uncertain_cleanup() {
        let device = failing_cleanup();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        let changed = read_with(
            &device,
            Ok(with_mode(
                kvm_core::MouseMode::Relative,
                Capability::Supported,
            )),
            0,
        )
        .await;
        app.finish_refresh(&device, changed).await;
        assert!(app.device_state_line().starts_with("Cleanup uncertain"));
        app.key(&device, KeyCode::Char('i')).await;
        assert!(app.control);
        assert!(app.message.contains("cleanup is uncertain"));
        assert!(state_row(&app, 20).starts_with("Cleanup uncertain"));
        app.key(&device, KeyCode::F(12)).await;
        app.control_finished(Ok(kvm_core::ControlOutcome::Unverified));
        assert!(state_row(&app, 20).starts_with("Cleanup uncertain"));
    }

    // Breaks if a release site stops recording its result: F12, a failed input's cleanup and a mouse release outside the video each set the warning on failure, and F12 and the input cleanup clear it on success. Focus loss and a closing stream are covered through the event loop by `loop_releases_record_their_result`.
    #[tokio::test]
    async fn every_release_site_records_its_result() {
        // F12.
        let mut app = App::default();
        app.refresh(&failing_cleanup()).await;
        app.key(&failing_cleanup(), KeyCode::Char('i')).await;
        app.key(&failing_cleanup(), KeyCode::F(12)).await;
        assert!(app.cleanup_uncertain.is_some());
        app.key(&FakeDevice::default(), KeyCode::Char('i')).await;
        app.key(&FakeDevice::default(), KeyCode::F(12)).await;
        assert!(app.cleanup_uncertain.is_none());
        // A failed key's cleanup.
        let failing_key = FakeDevice {
            fail_key: true,
            fail_cleanup: true,
            ..FakeDevice::default()
        };
        app.key(&failing_key, KeyCode::Char('i')).await;
        app.key(&failing_key, KeyCode::Char('a')).await;
        assert!(app.cleanup_uncertain.is_some());
        let clean_key = FakeDevice {
            fail_key: true,
            ..FakeDevice::default()
        };
        app.key(&clean_key, KeyCode::Char('i')).await;
        app.key(&clean_key, KeyCode::Char('a')).await;
        assert!(app.cleanup_uncertain.is_none());
        // A mouse release outside the video, with no viewport to map it to.
        app.key(&failing_cleanup(), KeyCode::Char('i')).await;
        app.mouse(
            &failing_cleanup(),
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 0,
                row: 0,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            ratatui::layout::Rect::new(0, 0, 4, 4),
        )
        .await;
        assert!(app.cleanup_uncertain.is_some());
    }

    // Breaks if focus loss or a closing video stream stops recording a failed release: each runs inside the event loop, and the rendered device-state row must then lead with the warning.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loop_releases_record_their_result() {
        // Focus loss.
        let device = std::sync::Arc::new(failing_cleanup());
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let mut sent = false;
        tokio::time::timeout(
            Duration::from_secs(10),
            run_loop(
                &mut terminal,
                device.clone(),
                &ImagePicker::halfblocks(),
                false,
                after_start_up_refresh(move |terminal| {
                    Ok(if !sent {
                        sent = true;
                        Some(Event::FocusLost)
                    } else if state_row_of(terminal).starts_with(CLEANUP_UNCERTAIN) {
                        Some(Event::Key(crossterm::event::KeyEvent::new(
                            KeyCode::Char('q'),
                            crossterm::event::KeyModifiers::NONE,
                        )))
                    } else {
                        std::thread::yield_now();
                        None
                    })
                }),
            ),
        )
        .await
        .expect("focus loss's failed release must reach the row")
        .unwrap();

        // A closing video stream.
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&image::RgbImage::new(8, 8))
            .unwrap();
        let device = std::sync::Arc::new(FakeDevice {
            video_frame: Some(jpeg),
            fail_video_after_frame: true,
            fail_cleanup: true,
            ..FakeDevice::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let mut opened = false;
        tokio::time::timeout(
            Duration::from_secs(10),
            run_loop(
                &mut terminal,
                device.clone(),
                &ImagePicker::halfblocks(),
                false,
                after_start_up_refresh(move |terminal| {
                    Ok(if !opened {
                        opened = true;
                        Some(Event::Key(crossterm::event::KeyEvent::new(
                            KeyCode::Char('v'),
                            crossterm::event::KeyModifiers::NONE,
                        )))
                    } else if state_row_of(terminal).starts_with(CLEANUP_UNCERTAIN) {
                        Some(Event::Key(crossterm::event::KeyEvent::new(
                            KeyCode::Char('q'),
                            crossterm::event::KeyModifiers::NONE,
                        )))
                    } else {
                        std::thread::yield_now();
                        None
                    })
                }),
            ),
        )
        .await
        .expect("a closing stream's failed release must reach the row")
        .unwrap();
    }

    /// The device-state row of a drawn screen: the last row inside the footer's bottom border.
    fn state_row_of(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        (1..buffer.area.width - 1)
            .map(|x| buffer[(x, buffer.area.height - 2)].symbol())
            .collect()
    }

    // A tripwire, not a proof: breaks if non-test code names `release_inputs` anywhere other than the allowed sites (the recording helper, the waited and background refresh releases recorded by `input_release_finished`, the exit cleanup and the drop guard), however the call is written or awaited; comment lines are skipped.
    #[test]
    fn releases_go_through_the_recording_helper() {
        let source = include_str!("lib.rs");
        let code = &source[..source.find("#[cfg(test)]\nmod tests").unwrap()];
        let direct: usize = code
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .map(|line| line.matches("release_inputs").count())
            .sum();
        let allowed = [
            "let release = device.release_inputs().await;\n        self.record_release",
            "let release = device.release_inputs().await;\n            self.input_release_finished",
            "device.release_inputs().await\n                        })));",
            "let cleanup = device.release_inputs().await.map_err(io::Error::other);",
            "if let Err(error) = device.release_inputs().await {\n                    eprintln!(\"Input cleanup uncertain after cancellation",
        ];
        for site in allowed {
            assert_eq!(code.matches(site).count(), 1, "{site}");
        }
        assert_eq!(direct, allowed.len());
    }

    // Breaks if the background release a refresh requires overwrites a newer inline release's result: focus loss releases while the background release is pending, fails, and the background one then finishes successfully.
    #[test]
    fn an_older_release_result_does_not_overwrite_a_newer_one() {
        let mut app = App {
            input_ended: Some(InputEnded {
                cause: "capabilities could not be read",
                release: Release::Pending,
            }),
            ..App::default()
        };
        let background = app.begin_release();
        let inline = app.begin_release();
        app.record_release(inline, &Err(kvm_core::Error::Http(401)));
        app.input_release_finished(background, Ok(()));
        assert!(app.cleanup_uncertain.is_some());
        assert!(app.device_state_line().starts_with(CLEANUP_UNCERTAIN));
        // The notice still finishes, so `i` no longer waits.
        assert!(!app.input_release_pending());
        // The reverse order records the newer success.
        let failed_first = app.begin_release();
        let succeeded_later = app.begin_release();
        app.record_release(failed_first, &Err(kvm_core::Error::Http(401)));
        app.record_release(succeeded_later, &Ok(()));
        assert!(app.cleanup_uncertain.is_none());
    }

    // With `kvm-kvmd`'s error text, which is long, the row must show the short warning and then the refresh outcome, and the message must carry the error.
    #[tokio::test]
    async fn a_long_adapter_error_leaves_the_refresh_outcome_on_the_row() {
        const KVMD_ERROR: &str = "input session remains indeterminate after an ambiguous operation";
        let mut app = App::default();
        app.refresh(&FakeDevice::default()).await;
        let number = app.begin_release();
        app.record_release(
            number,
            &Err(kvm_core::Error::AmbiguousInput(KVMD_ERROR.into())),
        );
        assert_eq!(
            app.device_state_line(),
            format!("{CLEANUP_UNCERTAIN}; Device state read")
        );
        for width in [52, 80, 120] {
            let row = state_row(&app, width);
            assert!(
                row.starts_with(&format!("{CLEANUP_UNCERTAIN}; Device state read")),
                "width {width}: {row:?}"
            );
        }
        app.refreshing = true;
        assert!(
            state_row(&app, 80)
                .starts_with(&format!("{CLEANUP_UNCERTAIN}; Refreshing device state"))
        );
    }

    // T1: after a change of only the mouse mode, input support remains, so the finished release offers `i` again; after a failed read it does not.
    #[tokio::test]
    async fn a_finished_release_offers_i_only_when_it_would_enter_input_mode() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        assert!(
            app.apply_refresh(
                read_with(
                    &device,
                    Ok(with_mode(
                        kvm_core::MouseMode::Relative,
                        Capability::Supported
                    )),
                    0
                )
                .await
            )
        );
        app.key(&device, KeyCode::Char('i')).await;
        assert_eq!(app.message, RELEASE_WAIT);
        let number = app.begin_release();
        app.input_release_finished(number, Ok(()));
        assert_eq!(app.message, "Inputs released; i enters input mode");

        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        assert!(app.apply_refresh(failed_read()));
        app.key(&device, KeyCode::Char('i')).await;
        let number = app.begin_release();
        app.input_release_finished(number, Ok(()));
        assert_eq!(app.message, "Inputs released");
    }

    // Breaks if a read applied while the release is pending drops the notice, which would let `i` re-enter input mode and the release lift what is pressed next.
    #[tokio::test]
    async fn a_pending_release_survives_a_later_read() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        assert!(app.apply_refresh(failed_read()));
        let later = read_with(&device, Ok(fake_capabilities(Capability::Supported)), 0).await;
        assert!(!app.apply_refresh(later));
        assert!(app.device_state_line().starts_with("Releasing inputs"));
        app.key(&device, KeyCode::Char('i')).await;
        assert!(!app.control);
        assert_eq!(app.message, RELEASE_WAIT);
    }

    // Scenario 1: a second refresh requested before the release fails completes after it; the warning must stay on the row, visible at 20 columns.
    #[tokio::test]
    async fn a_failed_release_survives_a_read_requested_before_it_failed() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        assert!(app.apply_refresh(failed_read()));
        let second = read_with(&device, Ok(fake_capabilities(Capability::Supported)), 0).await;
        let number = app.begin_release();
        app.input_release_finished(
            number,
            Err(kvm_core::Error::AmbiguousInput("cleanup failed".into())),
        );
        app.apply_refresh(second);
        assert!(state_row(&app, 20).starts_with("Cleanup uncertain"));
    }

    // Scenario 2: a control's outcome replaces the message and a refresh follows; the warning must still be on screen at every width.
    #[tokio::test]
    async fn a_failed_release_survives_a_control_outcome_and_a_refresh() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        assert!(app.apply_refresh(failed_read()));
        let number = app.begin_release();
        app.input_release_finished(
            number,
            Err(kvm_core::Error::AmbiguousInput("cleanup failed".into())),
        );
        app.control_finished(Ok(kvm_core::ControlOutcome::Unverified));
        assert!(!app.message.contains("Cleanup uncertain"));
        let later = read_with(
            &device,
            Ok(fake_capabilities(Capability::Supported)),
            app.controls_completed,
        )
        .await;
        app.apply_refresh(later);
        for width in [20, 30, 52, 80] {
            let row = state_row(&app, width);
            assert!(
                row.starts_with("Cleanup uncertain"),
                "width {width}: {row:?}"
            );
        }
    }

    // Breaks if the stream closing leaves input mode's entry text as the message after input mode has ended.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_closing_stream_replaces_the_input_mode_entry_message() {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&image::RgbImage::new(8, 8))
            .unwrap();
        let close = std::sync::Arc::new(tokio::sync::Notify::new());
        let device = std::sync::Arc::new(FakeDevice {
            video_frame: Some(jpeg),
            fail_video_after_frame: true,
            stream_close: Some(close.clone()),
            ..FakeDevice::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let mut phase = 0;
        let key = |code| {
            Some(Event::Key(crossterm::event::KeyEvent::new(
                code,
                crossterm::event::KeyModifiers::NONE,
            )))
        };
        tokio::time::timeout(
            Duration::from_secs(10),
            run_loop(
                &mut terminal,
                device.clone(),
                &ImagePicker::halfblocks(),
                false,
                after_start_up_refresh(move |terminal| {
                    let text = screen_text(terminal);
                    Ok(match phase {
                        0 => {
                            phase = 1;
                            key(KeyCode::Char('v'))
                        }
                        1 if text.contains("Live video") && !text.contains("Waiting for video") => {
                            phase = 2;
                            key(KeyCode::Char('i'))
                        }
                        2 if text.contains("Input mode:") => {
                            phase = 3;
                            close.notify_one();
                            None
                        }
                        3 if text.contains("stream failed") => {
                            assert!(!text.contains("Input mode:"), "{text}");
                            assert!(text.contains(INPUT_MODE_ENDED), "{text}");
                            phase = 4;
                            key(KeyCode::Char('q'))
                        }
                        _ => {
                            std::thread::yield_now();
                            None
                        }
                    })
                }),
            ),
        )
        .await
        .expect("the loop must finish")
        .unwrap();
    }

    // Breaks if a long help, message or command wraps the device-state line out of view.
    #[test]
    fn the_device_state_line_stays_visible_under_long_text() {
        let long_message = "Input mode: F12 exits; Escape is remote; terminal lacks key releases, keys are tapped; and a long transport error from the device that goes on";
        for width in [20, 30, 52, 80, 120] {
            for control in [false, true] {
                let mut terminal = Terminal::new(TestBackend::new(width, 16)).unwrap();
                let app = App {
                    control,
                    refreshing: true,
                    message: long_message.into(),
                    ..App::default()
                };
                terminal.draw(|frame| draw(frame, &app)).unwrap();
                let buffer = terminal.backend().buffer();
                // The device-state row is the last row inside the footer's bottom border.
                let row = buffer.area.height - 2;
                let line: String = (1..buffer.area.width - 1)
                    .map(|x| buffer[(x, row)].symbol())
                    .collect();
                let expected = "Refreshing device state";
                // At 20 columns the inner width is narrower than the text, which is cut off at it.
                let shown = &expected[..expected.len().min(usize::from(width) - 2)];
                assert!(line.starts_with(shown), "width {width}: {line:?}");
            }
        }
    }

    // Breaks if a refresh reporting the keyboard unsupported during input mode leaves a held modifier pressed or still tracked.
    #[tokio::test]
    async fn a_refresh_that_changes_capabilities_releases_a_held_modifier() {
        let device = FakeDevice::default();
        let mut app = App {
            enhanced_keyboard: true,
            ..App::default()
        };
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        app.key_event(
            &device,
            crossterm::event::KeyEvent::new(
                KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftShift),
                crossterm::event::KeyModifiers::SHIFT,
            ),
        )
        .await;
        assert!(!app.held_modifiers.is_empty());
        let refresh = read_with(&device, Ok(fake_capabilities(Capability::Unsupported)), 0).await;
        app.finish_refresh(&device, refresh).await;
        assert!(!app.control);
        assert!(app.held_modifiers.is_empty());
        assert_eq!(
            device.calls.lock().unwrap().last().map(String::as_str),
            Some("release")
        );
    }

    // Breaks if an unchanged refresh interrupts input mode.
    #[tokio::test]
    async fn an_unchanged_refresh_keeps_input_mode() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        app.key(&device, KeyCode::Char('i')).await;
        let refresh = read_with(&device, Ok(fake_capabilities(Capability::Supported)), 0).await;
        app.finish_refresh(&device, refresh).await;
        assert!(app.control);
        assert!(device.calls.lock().unwrap().is_empty());
    }

    // Breaks if a refresh replaces the command being typed, a confirmation prompt or a control's outcome.
    #[tokio::test]
    async fn a_refresh_never_replaces_the_message() {
        let device = FakeDevice::default();
        let mut app = App::default();
        app.refresh(&device).await;
        for c in ":power off-hard".chars() {
            app.key(&device, KeyCode::Char(c)).await;
        }
        let typed = app.message.clone();
        let failed = read_with(&device, Err(kvm_core::Error::Http(502)), 0).await;
        app.finish_refresh(&device, failed).await;
        assert_eq!(app.message, typed);
        assert!(app.device_state_line().contains("read failed"));
        app.key(&device, KeyCode::Enter).await;
        // The refresh failed to read capabilities, so the command is refused; read them again and retry.
        app.refresh(&device).await;
        for c in ":power off-hard".chars() {
            app.key(&device, KeyCode::Char(c)).await;
        }
        app.key(&device, KeyCode::Enter).await;
        let prompt = app.message.clone();
        assert!(prompt.starts_with("Confirm"));
        let refresh = read_with(&device, Ok(fake_capabilities(Capability::Supported)), 0).await;
        app.finish_refresh(&device, refresh).await;
        assert_eq!(app.message, prompt);
        // A control's error stays the message, and a read that started before it is marked as predating it.
        app.control_finished(Err(kvm_core::Error::Http(500)));
        let error = app.message.clone();
        let earlier = read_with(&device, Ok(fake_capabilities(Capability::Supported)), 0).await;
        app.finish_refresh(&device, earlier).await;
        assert_eq!(app.message, error);
        assert!(
            app.device_state_line()
                .contains("read before the last control")
        );
        let later = read_with(
            &device,
            Ok(fake_capabilities(Capability::Supported)),
            app.controls_completed,
        )
        .await;
        app.finish_refresh(&device, later).await;
        assert_eq!(app.device_state_line(), "Device state read");
    }

    #[tokio::test]
    async fn input_mode_during_the_first_read_says_capabilities_are_still_being_read() {
        let mut app = App {
            refreshing: true,
            ..App::default()
        };
        app.key(&FakeDevice::default(), KeyCode::Char('i')).await;
        assert_eq!(app.message, "Capabilities are still being read");
    }

    #[tokio::test]
    async fn refresh_reads_status_when_its_capability_is_unknown() {
        let device = FakeDevice {
            status_capability_unknown: true,
            ..FakeDevice::default()
        };
        let mut app = App::default();
        app.refresh(&device).await;
        assert_eq!(
            app.capabilities.as_ref().unwrap().status,
            Capability::Unknown
        );
        assert_eq!(app.status.as_ref().unwrap().atx, Reported::Unsupported);
    }

    #[test]
    fn renders_typed_status_and_escapes_device_text() {
        use kvm_core::{AtxStatus, GpioOutput, GpioStatus, SwitchPort, SwitchStatus};
        let port = |port| SwitchPort {
            port,
            unit: Reported::Supported(0),
            channel: Reported::Supported(port),
            id: Reported::Unknown,
            name: Reported::Unknown,
        };
        let status = DeviceStatus {
            info: Reported::Unknown,
            hid: Reported::Unsupported,
            streamer: Reported::Unknown,
            atx: Reported::Supported(AtxStatus {
                busy: Reported::Supported(false),
                power_led: Reported::Supported(true),
                hdd_led: Reported::Unknown,
                power_busy: Reported::Unsupported,
                reset_busy: Reported::Unsupported,
            }),
            switch: Reported::Supported(SwitchStatus {
                ports: Reported::Supported(vec![port(0), port(1)]),
                active_port: Reported::Supported(Some(1)),
                synced: Reported::Unknown,
            }),
            gpio: Reported::Supported(GpioStatus {
                inputs: Reported::Supported(Vec::new()),
                outputs: Reported::Supported(vec![GpioOutput {
                    channel: "lamp\u{1b}[2J\u{9b}\u{202e}".into(),
                    online: Reported::Supported(true),
                    busy: Reported::Supported(false),
                    state: Reported::Supported(true),
                    switchable: Reported::Supported(true),
                }]),
            }),
            media: Reported::Unsupported,
        };
        let summary = status_summary(&status);
        assert!(summary.contains("System: unknown"));
        assert!(summary.contains("HID: unsupported"));
        assert!(summary.contains("Power: power LED on, HDD LED unknown, idle"));
        assert!(summary.contains("Switch: active port 1, ports 0 1, unknown"));
        assert!(summary.contains("GPIO: lamp\\u{1b}[2J\\u{9b}\\u{202e} on"));
        assert!(!summary.contains(['\u{1b}', '\u{9b}', '\u{202e}']));
        assert!(summary.contains("Media: unsupported"));
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        let app = App {
            status: Some(status),
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
        assert!(content.contains("Power: power LED on"));
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

    /// Runs the real `run` in a child process on a pseudo-terminal and signals it at each startup stage: under tmux with the image protocol overridden, during a capability probe the terminal never answers (by SIGTERM and by SIGINT), and after an answered probe has enabled keyboard enhancement. Each run must restore the screen, the keyboard mode and the terminal attributes, and must leave standard input to the process afterwards.
    #[cfg(unix)]
    #[test]
    fn startup_process_restores_pty_on_termination_signals() {
        const CHILD: &str = "KVM_STARTUP_PTY_CHILD";
        const NAME: &str = "tests::startup_process_restores_pty_on_termination_signals";
        if let Some(scenario) = std::env::var_os(CHILD) {
            let probe_deadline = if scenario == "unanswered" {
                // Longer than the harness allows for the whole run, so only the signal can end the probe.
                Duration::from_secs(3600)
            } else {
                capabilities::PROBE_DEADLINE
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = runtime.block_on(run_with_probe_deadline(
                std::sync::Arc::new(FakeDevice::default()),
                probe_deadline,
            ));
            assert!(result.is_err());
            println!("RUN_RETURNED");
            // A reader left blocked on standard input by the probe would take this line first.
            let mut line = String::new();
            io::stdin().read_line(&mut line).unwrap();
            println!("STDIN_OWNED {}", line.trim());
            return;
        }
        let script = r#"
import os, pty, select, signal, subprocess, sys, termios, time, tempfile
binary, name, scenario, signame = sys.argv[1:5]
sandbox = tempfile.TemporaryDirectory()
marker = os.path.join(sandbox.name, 'called')
with open(os.path.join(sandbox.name, 'tmux'), 'w') as f:
    f.write('#!/bin/sh\nprintf called > ' + marker + '\n')
os.chmod(os.path.join(sandbox.name, 'tmux'), 0o700)
master, slave = pty.openpty()
before = termios.tcgetattr(slave)
env = {key: value for key, value in os.environ.items() if key not in ('TMUX', 'TERM_PROGRAM', 'KONSOLE_VERSION', 'KVM_TUI_IMAGE_PROTOCOL')}
env.update(KVM_STARTUP_PTY_CHILD=scenario, PATH=sandbox.name)
if scenario == 'tmux':
    env.update(TERM='tmux-256color', TERM_PROGRAM='tmux', TMUX='/nonexistent', KVM_TUI_IMAGE_PROTOCOL='halfblocks')
else:
    env.update(TERM='xterm-256color')
proc = subprocess.Popen([binary, '--exact', name, '--nocapture'], stdin=slave, stdout=slave, stderr=slave, env=env)
data = b''
deadline = time.monotonic() + 10
def read_until(needle, why):
    global data
    while needle not in data:
        if time.monotonic() >= deadline or proc.poll() is not None:
            raise AssertionError(why + ': ' + repr(data))
        if select.select([master], [], [], 0.1)[0]:
            data += os.read(master, 65536)
def normalised(attributes):
    # Darwin may set PENDIN transiently when canonical mode is restored.
    if hasattr(termios, 'PENDIN'):
        attributes[3] &= ~termios.PENDIN
    return attributes
try:
    read_until(b'\x1b[?1049h', 'startup did not enter alternate screen')
    if scenario != 'tmux':
        read_until(b'\x1b[c', 'startup did not send the capability probe')
        assert data.index(b'\x1b[?1049h') < data.index(b'\x1b[?u'), ('probe sent before the alternate screen', data)
    if scenario == 'answered':
        os.write(master, b'\x1b[?0u\x1b_Gi=31;OK\x1b\\\x1b[6;20;10t\x1b[?62;4c')
        read_until(b'\x1b[>11u', 'answered probe did not enable keyboard enhancement')
    os.kill(proc.pid, getattr(signal, 'SIG' + signame))
    read_until(b'RUN_RETURNED', 'signal did not end the session')
    assert b'\x1b[?1049l' in data, data
    if scenario == 'answered':
        assert b'\x1b[<1u' in data[data.index(b'\x1b[>11u'):], ('keyboard enhancement not popped', data)
    else:
        assert b'\x1b[>' not in data, ('keyboard enhancement pushed without an answer', data)
    assert not os.path.exists(marker), 'startup invoked tmux'
    after = termios.tcgetattr(slave)
    assert normalised(after) == normalised(before), ('terminal attributes not restored', before, after, data)
    os.write(master, b'stdin-check\n')
    read_until(b'STDIN_OWNED stdin-check', 'standard input was not left to the process')
    while proc.poll() is None:
        if time.monotonic() >= deadline:
            raise AssertionError('process did not exit')
        if select.select([master], [], [], 0.1)[0]:
            data += os.read(master, 65536)
    assert proc.returncode == 0, (proc.returncode, data)
finally:
    if proc.poll() is None:
        proc.kill()
        proc.wait()
    os.close(master)
    os.close(slave)
"#;
        for (scenario, signal) in [
            ("tmux", "TERM"),
            ("unanswered", "TERM"),
            ("unanswered", "INT"),
            ("answered", "TERM"),
        ] {
            let output = std::process::Command::new("python3")
                .args(["-I", "-c", script])
                .arg(std::env::current_exe().unwrap())
                .args([NAME, scenario, signal])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{scenario} with SIG{signal}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
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
                let mut termination = Termination::new().unwrap();
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
                let environment = capabilities::TerminalEnvironment::from_env().unwrap();
                let _ = detect_capabilities(&environment, capabilities::PROBE_DEADLINE).await;
                println!("SIGTERM_READY");
                std::io::stdout().flush().unwrap();
                let result = termination.wait(std::future::pending()).await;
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
    fn maps_only_known_keys() {
        assert_eq!(key_name(KeyCode::Char('a')).as_deref(), Some("KeyA"));
        assert_eq!(key_name(KeyCode::Char('!')).as_deref(), Some("Digit1"));
        assert_eq!(key_name(KeyCode::F(12)).as_deref(), Some("F12"));
        assert_eq!(key_name(KeyCode::Delete).as_deref(), Some("Delete"));
        assert_eq!(key_name(KeyCode::Char('é')), None);
    }
}
