//! Terminal capability detection.
//!
//! The terminal is queried directly for keyboard enhancement, the kitty graphics protocol, sixel
//! support and its cell size. The query ends with primary device attributes, which every
//! VT100-compatible terminal answers and which terminals answer in order, so its reply marks the
//! other replies as complete. The environment heuristics apply when the terminal does not answer,
//! and `KVM_TUI_IMAGE_PROTOCOL` always decides the image protocol when it is set.
//!
//! Probing never blocks on standard input: input is read only after `poll(2)` reports it ready,
//! and the wait between polls is an asynchronous sleep, so dropping the probe future (for example
//! when a termination signal wins a `select!`) leaves nothing reading the terminal.

use ratatui_image::picker::ProtocolType;
use std::{ffi::OsString, io, time::Duration};

/// Cell size in pixels assumed when neither the window size ioctl nor the terminal reports one; this is ratatui-image's own default.
pub(crate) const DEFAULT_CELL_SIZE: (u16, u16) = (10, 20);

/// Longest a terminal may take to answer the probe; the same bound crossterm applies to its keyboard enhancement query. A terminal that answers ends the probe at its device attributes reply, so only a terminal that never answers waits this long.
pub(crate) const PROBE_DEADLINE: Duration = Duration::from_secs(2);

/// Interval between readiness checks while waiting for replies; it bounds the latency added to an answer that arrives between checks and has no effect on what is detected.
#[cfg(unix)]
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Image identifier carried by the kitty graphics query so that its reply can be told apart from replies to other graphics commands.
const KITTY_QUERY_ID: &[u8] = b"i=31";

/// Longest control sequence body retained while parsing; probe replies are far shorter, so a longer sequence is not a reply and is discarded.
const MAX_SEQUENCE: usize = 256;

const ESC: u8 = 0x1b;

/// What the process environment says about the terminal.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct TerminalEnvironment {
    pub(crate) term: String,
    pub(crate) program: String,
    pub(crate) tmux: bool,
    pub(crate) konsole: bool,
    /// Image protocol named by `KVM_TUI_IMAGE_PROTOCOL`; when present it is authoritative.
    pub(crate) image_override: Option<ProtocolType>,
}

impl TerminalEnvironment {
    /// Reads the environment of this process.
    ///
    /// # Errors
    /// Returns an error when `KVM_TUI_IMAGE_PROTOCOL` is set to an unknown protocol or is not valid Unicode.
    pub(crate) fn from_env() -> io::Result<Self> {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> io::Result<Self> {
        let text = |name| {
            lookup(name)
                .and_then(|value: OsString| value.into_string().ok())
                .unwrap_or_default()
        };
        let term = text("TERM");
        let program = text("TERM_PROGRAM");
        let tmux = lookup("TMUX").is_some() || term.starts_with("tmux") || program == "tmux";
        let konsole = lookup("KONSOLE_VERSION").is_some_and(|value| !value.is_empty());
        let image_override = match lookup("KVM_TUI_IMAGE_PROTOCOL").map(OsString::into_string) {
            None => None,
            Some(Ok(value)) => Some(match value.as_str() {
                "halfblocks" => ProtocolType::Halfblocks,
                "kitty" => ProtocolType::Kitty,
                "sixel" => ProtocolType::Sixel,
                "iterm2" => ProtocolType::Iterm2,
                _ => return Err(io::Error::other("invalid KVM_TUI_IMAGE_PROTOCOL")),
            }),
            Some(Err(_)) => {
                return Err(io::Error::other("invalid KVM_TUI_IMAGE_PROTOCOL encoding"));
            }
        };
        Ok(Self {
            term,
            program,
            tmux,
            konsole,
            image_override,
        })
    }

    /// Terminals whose own image protocol is preferred: iTerm2, and WezTerm, which answers the kitty graphics query without supporting the placeholders the kitty renderer relies on.
    fn iterm2_family(&self) -> bool {
        matches!(self.program.as_str(), "iTerm.app" | "WezTerm") || self.term == "wezterm"
    }

    fn heuristic_protocol(&self) -> ProtocolType {
        if self.tmux {
            ProtocolType::Halfblocks
        } else if matches!(self.term.as_str(), "xterm-kitty" | "xterm-ghostty") {
            ProtocolType::Kitty
        } else if self.iterm2_family() {
            ProtocolType::Iterm2
        } else {
            ProtocolType::Halfblocks
        }
    }

    fn heuristic_keyboard(&self) -> bool {
        !self.tmux
            && matches!(
                self.term.as_str(),
                "xterm-kitty" | "xterm-ghostty" | "wezterm"
            )
    }
}

/// Replies collected from the terminal.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProbeReport {
    /// The terminal answered primary device attributes, so every other field is final.
    pub(crate) answered: bool,
    /// The terminal reported its keyboard enhancement flags, so it implements the kitty keyboard protocol.
    pub(crate) keyboard_enhancement: bool,
    pub(crate) kitty_graphics: bool,
    /// Primary device attributes include sixel graphics (attribute 4).
    pub(crate) sixel: bool,
    /// Cell size in pixels as (width, height).
    pub(crate) cell_size: Option<(u16, u16)>,
}

/// The capabilities the client uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Decision {
    pub(crate) protocol: ProtocolType,
    pub(crate) tmux: bool,
    pub(crate) enhanced_keyboard: bool,
    pub(crate) cell_size: (u16, u16),
}

/// Combines the environment, the probe and the window size into the capabilities to use.
///
/// The override decides the image protocol. Otherwise an answered probe decides both the image protocol and keyboard enhancement, and the environment heuristics decide them when the terminal was not probed or did not answer. Under tmux the sixel attribute describes tmux rather than the outer terminal, so it is ignored there; a kitty graphics reply under tmux arrived through passthrough and is used. Konsole answers the kitty graphics query and reports sixel, but renders neither correctly with this client, so it gets the character-cell fallback. The window size ioctl takes precedence over the reported cell size because it describes the same window without a round trip.
pub(crate) fn decide(
    environment: &TerminalEnvironment,
    report: Option<&ProbeReport>,
    window_cell_size: Option<(u16, u16)>,
) -> Decision {
    let answered = report.filter(|report| report.answered);
    let protocol = match (environment.image_override, answered) {
        (Some(protocol), _) => protocol,
        (None, None) => environment.heuristic_protocol(),
        (None, Some(_)) if environment.konsole => ProtocolType::Halfblocks,
        (None, Some(_)) if !environment.tmux && environment.iterm2_family() => ProtocolType::Iterm2,
        (None, Some(report)) if report.kitty_graphics => ProtocolType::Kitty,
        (None, Some(report)) if report.sixel && !environment.tmux => ProtocolType::Sixel,
        (None, Some(_)) => ProtocolType::Halfblocks,
    };
    let enhanced_keyboard = match answered {
        Some(report) => report.keyboard_enhancement,
        None => environment.heuristic_keyboard(),
    };
    let cell_size = window_cell_size
        .or_else(|| report.and_then(|report| report.cell_size))
        .unwrap_or(DEFAULT_CELL_SIZE);
    Decision {
        protocol,
        tmux: environment.tmux,
        enhanced_keyboard,
        cell_size,
    }
}

/// Builds the probe: keyboard enhancement flags, then (when `images` is set) the kitty graphics query and the cell size, then primary device attributes last as the sentinel. Under tmux the kitty graphics query is wrapped for passthrough so that it reaches the outer terminal; the other queries are answered by tmux itself.
pub(crate) fn query(tmux: bool, images: bool) -> Vec<u8> {
    let mut query = b"\x1b[?u".to_vec();
    if images {
        let kitty: &[u8] = b"_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA";
        if tmux {
            query.extend_from_slice(b"\x1bPtmux;\x1b\x1b");
            query.extend_from_slice(kitty);
            query.extend_from_slice(b"\x1b\x1b\\\x1b\\");
        } else {
            query.push(ESC);
            query.extend_from_slice(kitty);
            query.extend_from_slice(b"\x1b\\");
        }
        query.extend_from_slice(b"\x1b[16t");
    }
    query.extend_from_slice(b"\x1b[c");
    query
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    Csi,
    Apc,
    ApcEscape,
}

/// Incremental parser for probe replies; bytes that are not replies (such as keys typed during the probe) are ignored.
#[derive(Debug, Default)]
pub(crate) struct ResponseParser {
    state: State,
    sequence: Vec<u8>,
    report: ProbeReport,
}

impl ResponseParser {
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.push(byte);
        }
    }

    pub(crate) fn report(&self) -> ProbeReport {
        self.report
    }

    fn push(&mut self, byte: u8) {
        self.state = match (self.state, byte) {
            (State::Apc, ESC) => State::ApcEscape,
            (State::ApcEscape, b'\\') => {
                self.apc();
                State::Ground
            }
            (_, ESC) => State::Escape,
            (State::Escape, b'[') => {
                self.sequence.clear();
                State::Csi
            }
            (State::Escape, b'_') => {
                self.sequence.clear();
                State::Apc
            }
            (State::Csi, 0x20..=0x3f) | (State::Apc, _) if self.sequence.len() < MAX_SEQUENCE => {
                self.sequence.push(byte);
                self.state
            }
            (State::Csi, 0x40..=0x7e) => {
                self.csi(byte);
                State::Ground
            }
            _ => State::Ground,
        };
    }

    fn csi(&mut self, last: u8) {
        let parameters = self.sequence.as_slice();
        match last {
            b'u' => {
                if let Some(flags) = parameters.strip_prefix(b"?")
                    && !flags.is_empty()
                    && flags.iter().all(u8::is_ascii_digit)
                {
                    self.report.keyboard_enhancement = true;
                }
            }
            b'c' => {
                if let Some(attributes) = parameters.strip_prefix(b"?") {
                    self.report.sixel |= attributes.split(|&b| b == b';').any(|a| a == b"4");
                    self.report.answered = true;
                }
            }
            b't' => {
                let fields: Vec<&[u8]> = parameters.split(|&b| b == b';').collect();
                if let [b"6", height, width] = fields.as_slice()
                    && let (Some(height), Some(width)) = (number(height), number(width))
                    && height > 0
                    && width > 0
                {
                    self.report.cell_size = Some((width, height));
                }
            }
            _ => {}
        }
    }

    fn apc(&mut self) {
        let Some(body) = self.sequence.strip_prefix(b"G") else {
            return;
        };
        let mut parts = body.splitn(2, |&b| b == b';');
        let keys = parts.next().unwrap_or_default();
        let message = parts.next().unwrap_or_default();
        if keys.split(|&b| b == b',').any(|key| key == KITTY_QUERY_ID) && message.starts_with(b"OK")
        {
            self.report.kitty_graphics = true;
        }
    }
}

fn number(digits: &[u8]) -> Option<u16> {
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// Writes `query` to standard output and collects replies from standard input until the terminal answers primary device attributes, standard input ends, or `deadline` passes.
///
/// Standard input is read only once `poll(2)` reports it ready, and the wait between checks is an asynchronous sleep, so the future can be dropped at any await point without leaving a read in progress. Input that is not a reply, such as keys typed during the probe, is discarded.
///
/// # Errors
/// Returns an error when writing the query or polling or reading standard input fails.
#[cfg(unix)]
pub(crate) async fn probe(query: &[u8], deadline: Duration) -> io::Result<ProbeReport> {
    use std::io::Write;
    let mut stdout = io::stdout();
    stdout.write_all(query)?;
    stdout.flush()?;
    let deadline = tokio::time::Instant::now() + deadline;
    let mut parser = ResponseParser::default();
    let mut buffer = [0_u8; 1024];
    loop {
        while stdin_ready()? {
            match read_stdin(&mut buffer)? {
                Some(0) => return Ok(parser.report()),
                Some(read) => parser.feed(&buffer[..read]),
                None => break,
            }
        }
        if parser.report().answered || tokio::time::Instant::now() >= deadline {
            return Ok(parser.report());
        }
        tokio::time::sleep(PROBE_POLL_INTERVAL).await;
    }
}

/// Whether a read of standard input would return without blocking; hang-up and invalid descriptors count as ready so that the following read reports them.
#[cfg(unix)]
fn stdin_ready() -> io::Result<bool> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let stdin = rustix::stdio::stdin();
    let mut descriptors = [PollFd::from_borrowed_fd(stdin, PollFlags::IN)];
    let immediately = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // poll(2) counts the descriptors whose returned events are non-zero, so with one descriptor a positive count means it reported an event, hang-up and invalid descriptors included; `PollFd::revents` is not read because it panics on an event bit rustix does not define.
    match poll(&mut descriptors, Some(&immediately)) {
        Ok(ready) => Ok(ready > 0),
        Err(rustix::io::Errno::INTR) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Reads what standard input holds; `None` means the read was interrupted or would block and should be retried after the next readiness check.
///
/// Reads the descriptor directly: a buffered `std::io::Stdin` could hold back bytes the readiness check cannot see.
#[cfg(unix)]
fn read_stdin(buffer: &mut [u8]) -> io::Result<Option<usize>> {
    use rustix::io::Errno;
    match rustix::io::read(rustix::stdio::stdin(), buffer) {
        Ok(read) => Ok(Some(read)),
        Err(Errno::INTR | Errno::AGAIN) => Ok(None),
        // A descriptor that is no longer open has nothing more to say.
        Err(Errno::BADF) => Ok(Some(0)),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(pairs: &[(&str, &str)]) -> io::Result<TerminalEnvironment> {
        let pairs: Vec<(String, OsString)> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        TerminalEnvironment::from_lookup(|name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
    }

    fn parse(bytes: &[u8]) -> ProbeReport {
        let mut parser = ResponseParser::default();
        parser.feed(bytes);
        parser.report()
    }

    fn answered(report: ProbeReport) -> ProbeReport {
        ProbeReport {
            answered: true,
            ..report
        }
    }

    const FULL_REPLY: &[u8] = b"\x1b[?0u\x1b_Gi=31;OK\x1b\\\x1b[6;20;10t\x1b[?62;4;22c";

    #[test]
    fn parses_every_reply_even_one_byte_at_a_time() {
        let expected = ProbeReport {
            answered: true,
            keyboard_enhancement: true,
            kitty_graphics: true,
            sixel: true,
            cell_size: Some((10, 20)),
        };
        assert_eq!(parse(FULL_REPLY), expected);
        let mut parser = ResponseParser::default();
        for byte in FULL_REPLY {
            parser.feed(std::slice::from_ref(byte));
        }
        assert_eq!(parser.report(), expected);
    }

    #[test]
    fn device_attributes_alone_answer_without_capabilities() {
        assert_eq!(
            parse(b"\x1b[?1;2c"),
            ProbeReport {
                answered: true,
                ..ProbeReport::default()
            }
        );
        assert!(!parse(b"\x1b[?u\x1b[?0u").answered);
    }

    #[test]
    fn rejects_kitty_errors_and_other_image_ids() {
        assert!(!parse(b"\x1b_Gi=31;ENOTSUPPORTED:no\x1b\\").kitty_graphics);
        assert!(!parse(b"\x1b_Gi=7;OK\x1b\\").kitty_graphics);
        assert!(!parse(b"\x1b_Gi=310;OK\x1b\\").kitty_graphics);
        assert!(parse(b"\x1b_Gi=31,p=1;OK\x1b\\").kitty_graphics);
    }

    #[test]
    fn ignores_keys_and_malformed_sequences_around_replies() {
        let report = parse(b"ab\x1b[?xu\x1b[6;0;9t\x1b[?62;14cq\x1bO\x1b[?3u");
        assert!(report.answered);
        assert!(report.keyboard_enhancement);
        assert!(!report.sixel, "attribute 14 is not attribute 4");
        assert_eq!(report.cell_size, None);
    }

    #[test]
    fn discards_overlong_sequences() {
        let mut bytes = b"\x1b_Gi=31;OK".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', MAX_SEQUENCE));
        bytes.extend_from_slice(b"\x1b\\\x1b[?1c");
        let report = parse(&bytes);
        assert!(!report.kitty_graphics);
        assert!(report.answered);
    }

    #[test]
    fn query_ends_with_device_attributes_and_wraps_kitty_for_tmux() {
        let plain = query(false, true);
        assert!(plain.starts_with(b"\x1b[?u"));
        assert!(plain.ends_with(b"\x1b[c"));
        assert!(plain.windows(5).any(|window| window == b"\x1b_Gi="));
        let wrapped = query(true, true);
        assert!(
            wrapped
                .windows(9)
                .any(|window| window == b"\x1bPtmux;\x1b\x1b")
        );
        assert!(wrapped.ends_with(b"\x1b[c"));
        assert_eq!(query(false, false), b"\x1b[?u\x1b[c");
    }

    #[test]
    fn reads_override_and_tmux_from_environment() {
        let env = environment(&[("TERM", "screen"), ("TMUX", "/sock")]).unwrap();
        assert!(env.tmux);
        assert_eq!(env.image_override, None);
        let env = environment(&[("KVM_TUI_IMAGE_PROTOCOL", "sixel")]).unwrap();
        assert_eq!(env.image_override, Some(ProtocolType::Sixel));
        assert!(environment(&[("KVM_TUI_IMAGE_PROTOCOL", "png")]).is_err());
        assert!(environment(&[("TERM_PROGRAM", "tmux")]).unwrap().tmux);
        assert!(
            environment(&[("KONSOLE_VERSION", "250400")])
                .unwrap()
                .konsole
        );
    }

    #[test]
    fn override_is_authoritative_over_the_probe() {
        let env = environment(&[("KVM_TUI_IMAGE_PROTOCOL", "halfblocks")]).unwrap();
        let report = parse(FULL_REPLY);
        let decision = decide(&env, Some(&report), None);
        assert_eq!(decision.protocol, ProtocolType::Halfblocks);
        assert!(decision.enhanced_keyboard);
        let env = environment(&[("KVM_TUI_IMAGE_PROTOCOL", "kitty")]).unwrap();
        assert_eq!(
            decide(&env, Some(&ProbeReport::default()), None).protocol,
            ProtocolType::Kitty
        );
    }

    #[test]
    fn answered_probe_is_authoritative_over_the_environment() {
        let kitty = environment(&[("TERM", "xterm-kitty")]).unwrap();
        let bare = answered(ProbeReport::default());
        let decision = decide(&kitty, Some(&bare), None);
        assert_eq!(decision.protocol, ProtocolType::Halfblocks);
        assert!(!decision.enhanced_keyboard);
        let xterm = environment(&[("TERM", "xterm-256color")]).unwrap();
        let graphics = answered(ProbeReport {
            kitty_graphics: true,
            keyboard_enhancement: true,
            ..ProbeReport::default()
        });
        let decision = decide(&xterm, Some(&graphics), None);
        assert_eq!(decision.protocol, ProtocolType::Kitty);
        assert!(decision.enhanced_keyboard);
        let sixel = answered(ProbeReport {
            sixel: true,
            ..ProbeReport::default()
        });
        assert_eq!(
            decide(&xterm, Some(&sixel), None).protocol,
            ProtocolType::Sixel
        );
    }

    #[test]
    fn unanswered_probe_falls_back_to_environment_heuristics() {
        let silent = ProbeReport::default();
        for (pairs, protocol, keyboard) in [
            (vec![("TERM", "xterm-kitty")], ProtocolType::Kitty, true),
            (vec![("TERM", "xterm-ghostty")], ProtocolType::Kitty, true),
            (
                vec![("TERM_PROGRAM", "iTerm.app")],
                ProtocolType::Iterm2,
                false,
            ),
            (vec![("TERM", "wezterm")], ProtocolType::Iterm2, true),
            (
                vec![("TERM", "xterm-kitty"), ("TMUX", "/sock")],
                ProtocolType::Halfblocks,
                false,
            ),
            (vec![("TERM", "vt100")], ProtocolType::Halfblocks, false),
        ] {
            let env = environment(&pairs).unwrap();
            for report in [None, Some(&silent)] {
                let decision = decide(&env, report, None);
                assert_eq!(decision.protocol, protocol, "{pairs:?}");
                assert_eq!(decision.enhanced_keyboard, keyboard, "{pairs:?}");
            }
        }
    }

    #[test]
    fn terminal_specific_exceptions_apply_to_answered_probes() {
        let full = parse(FULL_REPLY);
        let iterm = environment(&[("TERM_PROGRAM", "WezTerm")]).unwrap();
        assert_eq!(
            decide(&iterm, Some(&full), None).protocol,
            ProtocolType::Iterm2
        );
        let konsole = environment(&[("KONSOLE_VERSION", "250400")]).unwrap();
        assert_eq!(
            decide(&konsole, Some(&full), None).protocol,
            ProtocolType::Halfblocks
        );
        let tmux = environment(&[("TMUX", "/sock"), ("TERM_PROGRAM", "iTerm.app")]).unwrap();
        let sixel_only = answered(ProbeReport {
            sixel: true,
            ..ProbeReport::default()
        });
        let decision = decide(&tmux, Some(&sixel_only), None);
        assert_eq!(decision.protocol, ProtocolType::Halfblocks);
        assert!(decision.tmux);
        assert_eq!(
            decide(&tmux, Some(&full), None).protocol,
            ProtocolType::Kitty
        );
    }

    #[test]
    fn cell_size_prefers_window_then_reply_then_default() {
        let env = TerminalEnvironment::default();
        let report = parse(FULL_REPLY);
        assert_eq!(
            decide(&env, Some(&report), Some((8, 16))).cell_size,
            (8, 16)
        );
        assert_eq!(decide(&env, Some(&report), None).cell_size, (10, 20));
        let without_size = answered(ProbeReport::default());
        assert_eq!(
            decide(&env, Some(&without_size), None).cell_size,
            DEFAULT_CELL_SIZE
        );
        assert_eq!(decide(&env, None, None).cell_size, DEFAULT_CELL_SIZE);
    }
}
