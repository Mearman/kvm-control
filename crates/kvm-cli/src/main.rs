use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Parser, Subcommand, ValueEnum};
use kvm_core::{
    AuthScheme, Confirmation, Connected, Connector, Control, ControlOutcome, Credentials, Device,
    DeviceProfile, Endpoint, LoginState, PowerAction, ServerTrust,
};
use kvm_kvmd::KvmdConnector;
use serde_json::{Value, json};
use url::Url;
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(version, about = "Control a KVMD-compatible device")]
struct Cli {
    /// Device base HTTPS URL without credentials (the adapter adds its API path)
    #[arg(long)]
    endpoint: Url,
    #[arg(long, value_enum)]
    profile: Profile,
    #[arg(long)]
    username: String,
    /// Read the password from standard input instead of a hidden terminal prompt
    #[arg(long)]
    password_stdin: bool,
    /// Prompt for a current TOTP code for cookie login
    #[arg(long, conflicts_with = "password_stdin")]
    totp: bool,
    /// Authentication method
    #[arg(long, value_enum, default_value = "cookie")]
    auth: Auth,
    /// PEM certificate authority to trust
    #[arg(long)]
    ca: Option<PathBuf>,
    /// Disable TLS certificate verification for this connection
    #[arg(long)]
    insecure_tls: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Profile {
    Pikvm,
    Glkvm,
}

#[derive(Clone, Copy, ValueEnum)]
enum Auth {
    Cookie,
    Header,
}

#[derive(Clone, Copy, ValueEnum)]
enum Power {
    On,
    Off,
    OffHard,
    ResetHard,
}

#[derive(Clone, Copy, ValueEnum)]
enum Toggle {
    On,
    Off,
}

#[derive(Subcommand)]
enum Command {
    /// Check authentication and return device status
    Diagnose,
    Status,
    Capabilities,
    /// Write a JPEG to a new file
    Snapshot {
        output: PathBuf,
    },
    /// Press and release a KVMD key name
    Key {
        key: String,
    },
    /// Press and release a KVMD mouse button name
    Click {
        button: String,
    },
    /// Move the mouse to signed absolute coordinates
    Move {
        #[arg(allow_hyphen_values = true)]
        x: i16,
        #[arg(allow_hyphen_values = true)]
        y: i16,
    },
    /// Execute a power operation after explicit confirmation
    Power {
        #[arg(value_enum)]
        action: Power,
        #[arg(long, required = true)]
        confirm: bool,
    },
    /// Select a KVM switch port after explicit confirmation
    Switch {
        port: u32,
        #[arg(long, required = true)]
        confirm: bool,
    },
    /// Set a GPIO channel after explicit confirmation
    Gpio {
        channel: String,
        #[arg(value_enum)]
        state: Toggle,
        #[arg(long, required = true)]
        confirm: bool,
    },
    /// Connect or disconnect virtual media after explicit confirmation
    MediaConnect {
        #[arg(value_enum)]
        state: Toggle,
        #[arg(long, required = true)]
        confirm: bool,
    },
    /// Select a virtual media image after explicit confirmation
    MediaSelect {
        image: String,
        #[arg(long, required = true)]
        confirm: bool,
    },
    /// Stream newline-delimited JSON objects containing base64 JPEG frames
    Video {
        /// Stop after this many frames
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        frames: Option<u64>,
    },
}

fn password(from_stdin: bool) -> Result<String, String> {
    if from_stdin {
        if io::stdin().is_terminal() {
            return Err("--password-stdin requires redirected input".into());
        }
        let mut value = String::new();
        io::stdin()
            .read_to_string(&mut value)
            .map_err(|_| "could not read password from stdin")?;
        if value.ends_with('\n') {
            value.pop();
            if value.ends_with('\r') {
                value.pop();
            }
        }
        if value.is_empty() {
            return Err("password is empty".into());
        }
        Ok(value)
    } else {
        rpassword::prompt_password("Password: ")
            .map_err(|_| "could not read password from terminal".into())
    }
}

async fn write_video(
    video: &mut dyn kvm_core::VideoStream,
    output: &mut impl Write,
    frames: Option<u64>,
) -> Result<u64, String> {
    let mut count = 0_u64;
    while let Some(frame) = video.next_frame().await.map_err(|e| e.to_string())? {
        let record = json!({"ok": true, "frame": {"encoding": "base64", "media_type": "image/jpeg", "data": STANDARD.encode(frame.jpeg)}});
        writeln!(output, "{record}")
            .and_then(|()| output.flush())
            .map_err(|e| format!("cannot write video frame: {e}"))?;
        count += 1;
        if frames == Some(count) {
            break;
        }
    }
    Ok(count)
}

async fn run_control(
    device: &(impl Device + ?Sized),
    operation: Control,
    confirm: bool,
) -> Result<Value, String> {
    if !confirm {
        return Err("explicit --confirm is required".into());
    }
    let outcome = device
        .control(operation, Confirmation::new())
        .await
        .map_err(|e| e.to_string())?;
    let post_state = match outcome {
        ControlOutcome::ObservedMatching => "observed_matching",
        ControlOutcome::Unverified => "unverified",
    };
    Ok(json!({"accepted": true, "post_state": post_state}))
}

/// Registers the SIGTERM and SIGINT handlers before returning, and returns a future that resolves when either signal arrives. A caller that calls this before submitting any input therefore handles a signal at every later point instead of taking the default disposition, which would exit with the input still held.
///
/// # Errors
/// Returns an error when a handler cannot be registered.
#[cfg(unix)]
fn termination_signal() -> io::Result<impl std::future::Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => (),
            _ = interrupt.recv() => (),
        }
    })
}

/// Registers the Ctrl-C handler before returning, and returns a future that resolves when Ctrl-C arrives; see the Unix variant for why registration is eager.
///
/// # Errors
/// Returns an error when the handler cannot be registered.
#[cfg(not(unix))]
fn termination_signal() -> io::Result<impl std::future::Future<Output = ()>> {
    let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
    Ok(async move {
        ctrl_c.recv().await;
    })
}

/// Holds `press` until it completes or `interrupt` resolves, then always releases inputs, and reports the outcome described by [`press_outcome`]. A press result already available when the signal is also ready is used rather than discarded. A signal the runtime has observed by the time the release completes is passed on; one observed later, or after this returns, is not.
async fn press_and_release<F, S>(
    device: &(impl Device + ?Sized),
    press: F,
    interrupt: S,
) -> Result<(), String>
where
    F: std::future::Future<Output = kvm_core::Result<()>>,
    S: std::future::Future<Output = ()>,
{
    let mut interrupt = std::pin::pin!(interrupt);
    // Biased towards the press so a result already available is kept; a pending press lets the signal be polled next, so the signal is never starved.
    let pressed = tokio::select! {
        biased;
        result = press => Some(result),
        () = &mut interrupt => None,
    };
    let cleanup = device.release_inputs().await;
    // A press that ended by the signal has already consumed it; otherwise one poll collects a signal observed during the release.
    let signalled = pressed.is_none()
        || tokio::select! {
            biased;
            () = &mut interrupt => true,
            () = std::future::ready(()) => false,
        };
    press_outcome(pressed, cleanup, signalled)
}

/// Reports a press that the signal ended (`pressed` is `None`) as an interruption with an uncertain outcome. A completed press is reported as it ended: a successful press with confirmed cleanup is a success even when a signal arrived, since nothing was interrupted and a failure would invite a retry that presses the input again; when the press or the cleanup failed, a signal is named first as `interrupted; press: <error>` or `interrupted; press completed`. Every failure after a press carries the cleanup result.
fn press_outcome(
    pressed: Option<kvm_core::Result<()>>,
    cleanup: kvm_core::Result<()>,
    signalled: bool,
) -> Result<(), String> {
    let failure = match (pressed, &cleanup) {
        (Some(Ok(())), Ok(())) => return Ok(()),
        (Some(Ok(())), Err(e)) if !signalled => return Err(e.to_string()),
        (Some(Ok(())), Err(_)) => "interrupted; press completed".to_owned(),
        (Some(Err(e)), _) if signalled => format!("interrupted; press: {e}"),
        (Some(Err(e)), _) => e.to_string(),
        (None, _) => "interrupted; input outcome uncertain".to_owned(),
    };
    let cleanup = match cleanup {
        Ok(()) => "confirmed".to_owned(),
        Err(e) => e.to_string(),
    };
    Err(format!("{failure}; cleanup: {cleanup}"))
}

fn write_snapshot(
    path: &std::path::Path,
    bytes: &[u8],
    write: impl FnOnce(&mut std::fs::File, &[u8]) -> io::Result<()>,
) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("cannot create snapshot: {e}"))?;
    if let Err(error) = write(&mut file, bytes) {
        drop(file);
        return match std::fs::remove_file(path) {
            Ok(()) => Err(format!("cannot write snapshot: {error}")),
            Err(cleanup) => Err(format!(
                "cannot write snapshot: {error}; cannot remove partial file: {cleanup}"
            )),
        };
    }
    Ok(())
}

async fn execute(device: &(impl Device + ?Sized), command: Command) -> Result<Value, String> {
    match command {
        Command::Diagnose | Command::Status => {
            Ok(json!({"status": device.status().await.map_err(|e| e.to_string())?}))
        }
        Command::Capabilities => {
            Ok(json!({"capabilities": device.capabilities().await.map_err(|e| e.to_string())?}))
        }
        Command::Snapshot { output } => {
            let snapshot = device.snapshot().await.map_err(|e| e.to_string())?;
            write_snapshot(&output, &snapshot.jpeg, |file, bytes| file.write_all(bytes))?;
            Ok(json!({"output": output, "bytes": snapshot.jpeg.len()}))
        }
        Command::Key { key } => {
            // Registered before the press future exists, so once the press has been sent no signal can reach the default disposition, and a registration failure is reported before anything is sent.
            let interrupt =
                termination_signal().map_err(|e| format!("signal handler failed: {e}"))?;
            press_and_release(device, device.key(&key, true), interrupt).await?;
            Ok(json!({"key": key, "released": true}))
        }
        Command::Click { button } => {
            let interrupt =
                termination_signal().map_err(|e| format!("signal handler failed: {e}"))?;
            press_and_release(device, device.mouse_button(&button, true), interrupt).await?;
            Ok(json!({"button": button, "released": true}))
        }
        Command::Move { x, y } => {
            device.mouse_move(x, y).await.map_err(|e| e.to_string())?;
            Ok(json!({"x": x, "y": y}))
        }
        Command::Power { action, confirm } => {
            run_control(
                device,
                Control::Power(match action {
                    Power::On => PowerAction::On,
                    Power::Off => PowerAction::Off,
                    Power::OffHard => PowerAction::OffHard,
                    Power::ResetHard => PowerAction::ResetHard,
                }),
                confirm,
            )
            .await
        }
        Command::Switch { port, confirm } => {
            run_control(device, Control::Switch { port }, confirm).await
        }
        Command::Gpio {
            channel,
            state,
            confirm,
        } => {
            run_control(
                device,
                Control::Gpio {
                    channel,
                    state: matches!(state, Toggle::On),
                },
                confirm,
            )
            .await
        }
        Command::MediaConnect { state, confirm } => {
            run_control(
                device,
                Control::MediaConnect {
                    connected: matches!(state, Toggle::On),
                },
                confirm,
            )
            .await
        }
        Command::MediaSelect { image, confirm } => {
            run_control(device, Control::MediaSelect { image }, confirm).await
        }
        Command::Video { frames } => {
            let mut video = device.video().await.map_err(|e| e.to_string())?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            let count = write_video(video.as_mut(), &mut output, frames).await?;
            Ok(json!({"frames": count}))
        }
    }
}

async fn run(cli: Cli) -> Result<Value, String> {
    let ca = cli
        .ca
        .map(|path| {
            std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read --ca certificate {}: {e}", path.display()))
        })
        .transpose()?;
    if cli.insecure_tls {
        eprintln!("warning: TLS certificate verification is disabled");
    }
    let endpoint = Endpoint {
        label: None,
        address: cli.endpoint,
        profile: match cli.profile {
            Profile::Pikvm => DeviceProfile::PiKvm,
            Profile::Glkvm => DeviceProfile::GlKvm,
        },
        auth: match cli.auth {
            Auth::Cookie => AuthScheme::Session,
            Auth::Header => AuthScheme::Header,
        },
        trust: if cli.insecure_tls {
            ServerTrust::Disabled
        } else {
            ServerTrust::Verified {
                additional_ca_pem: ca,
            }
        },
        timeout: Duration::from_secs(30),
    };
    if cli.totp && matches!(cli.auth, Auth::Header) {
        return Err("TOTP requires cookie authentication; header authentication would reuse an expiring code".into());
    }
    let mut credentials =
        Credentials::new(cli.username, Zeroizing::new(password(cli.password_stdin)?));
    if cli.totp {
        credentials = credentials.with_totp(Zeroizing::new(
            rpassword::prompt_password("TOTP code: ")
                .map_err(|_| "could not read TOTP from terminal")?,
        ));
    }
    // The credentials are consumed by the login, which drops and so clears them once it has sent them. Under header authentication the connector also checks them with the device.
    let connection = match KvmdConnector
        .connect(endpoint, credentials)
        .await
        .map_err(|e| e.to_string())?
    {
        Connected::Authenticated(connection) => connection,
        Connected::ApprovalPending {
            connection,
            expires_in,
        } => {
            eprintln!(
                "{}",
                json!({"event": "approval_pending", "expires_in": expires_in, "message": "Approve this login on the device"})
            );
            let expiry = expires_in.ok_or("approval has no expiry")?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(expiry);
            let mut state = LoginState::ApprovalPending { expires_in };
            while matches!(state, LoginState::ApprovalPending { .. }) {
                if tokio::time::Instant::now() >= deadline {
                    return Err("device approval expired".into());
                }
                tokio::time::timeout_at(deadline, async {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    connection.complete_approval().await
                })
                .await
                .map_err(|_| "device approval expired")?
                .map(|next| state = next)
                .map_err(|e| e.to_string())?;
            }
            connection
        }
    };
    execute(connection.as_ref(), cli.command).await
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let is_video = matches!(cli.command, Command::Video { .. });
    match run(cli).await {
        Ok(value) => {
            if !is_video
                && let Err(error) = writeln!(
                    io::stdout().lock(),
                    "{}",
                    kvm_core::escape_json_for_terminal(
                        &json!({"ok": true, "result": value}).to_string()
                    )
                )
            {
                eprintln!("cannot write output: {error}");
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!(
                "{}",
                kvm_core::escape_json_for_terminal(
                    &json!({"ok": false, "error": error}).to_string()
                )
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// A device whose key requests are ambiguous but whose releases succeed, so the CLI's press and cleanup reporting can be checked separately. It does not keep `release_inputs` failing after an ambiguous key as `Device::release_inputs` requires of an adapter, which the CLI does not rely on: it reports each call's result as it is.
    struct Fake(std::sync::Mutex<Vec<String>>);
    struct Frames(u8);

    #[async_trait::async_trait]
    impl kvm_core::VideoStream for Frames {
        async fn next_frame(&mut self) -> kvm_core::Result<Option<kvm_core::Snapshot>> {
            if self.0 == 0 {
                return Ok(None);
            }
            self.0 -= 1;
            Ok(Some(kvm_core::Snapshot {
                jpeg: vec![0xff, 0xd8, 0xff, 0xd9],
            }))
        }
    }

    struct BrokenOutput;
    impl Write for BrokenOutput {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn video_reports_broken_output() {
        let error = write_video(&mut Frames(1), &mut BrokenOutput, None)
            .await
            .unwrap_err();
        assert!(error.contains("cannot write video frame"));
    }

    #[tokio::test]
    async fn video_emits_bounded_individual_json_frames() {
        let mut stream = Frames(2);
        let mut bytes = Vec::new();
        assert_eq!(
            write_video(&mut stream, &mut bytes, Some(1)).await.unwrap(),
            1
        );
        assert_eq!(stream.0, 1);
        let record: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record["frame"]["data"], "/9j/2Q==");
        assert_eq!(record["frame"]["media_type"], "image/jpeg");
    }

    #[async_trait::async_trait]
    impl Device for Fake {
        async fn video(&self) -> kvm_core::Result<Box<dyn kvm_core::VideoStream>> {
            Err(kvm_core::Error::Unsupported("synthetic video".into()))
        }
        async fn status(&self) -> kvm_core::Result<kvm_core::DeviceStatus> {
            unreachable!()
        }
        async fn capabilities(&self) -> kvm_core::Result<kvm_core::Capabilities> {
            unreachable!()
        }
        async fn snapshot(&self) -> kvm_core::Result<kvm_core::Snapshot> {
            Ok(kvm_core::Snapshot {
                jpeg: vec![0xff, 0xd8, 0xff, 0xd9],
            })
        }
        async fn key(&self, key: &str, pressed: bool) -> kvm_core::Result<()> {
            self.0.lock().unwrap().push(format!("{key}:{pressed}"));
            Err(kvm_core::Error::AmbiguousInput("synthetic failure".into()))
        }
        async fn mouse_button(&self, _: &str, _: bool) -> kvm_core::Result<()> {
            unreachable!()
        }
        async fn mouse_move(&self, x: i16, y: i16) -> kvm_core::Result<()> {
            self.0.lock().unwrap().push(format!("{x},{y}"));
            Ok(())
        }
        async fn control(
            &self,
            operation: Control,
            _: Confirmation,
        ) -> kvm_core::Result<ControlOutcome> {
            self.0.lock().unwrap().push(format!("{operation:?}"));
            Ok(match operation {
                Control::Power(_) => ControlOutcome::Unverified,
                _ => ControlOutcome::ObservedMatching,
            })
        }
        async fn release_inputs(&self) -> kvm_core::Result<()> {
            self.0.lock().unwrap().push("release".into());
            Ok(())
        }
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_is_private_with_permissive_umask() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "KVM_SNAPSHOT_PERMISSION_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new("sh")
                .arg("-c")
                .arg("umask 000; exec \"$1\" --exact tests::snapshot_is_private_with_permissive_umask")
                .arg("sh")
                .arg(std::env::current_exe().unwrap())
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(result.success());
            return;
        }
        let directory =
            std::env::temp_dir().join(format!("kvm-cli-permissions-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("snapshot.jpg");
        write_snapshot(&path, b"first", |file, bytes| file.write_all(bytes)).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_snapshot(&path, b"second", |file, bytes| file.write_all(bytes)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn partial_snapshot_is_removed_after_write_failure() {
        let path =
            std::env::temp_dir().join(format!("kvm-cli-snapshot-test-{}", std::process::id()));
        assert!(!path.exists());
        let result = write_snapshot(&path, b"jpeg", |file, bytes| {
            file.write_all(&bytes[..2])?;
            Err(io::Error::other("synthetic write failure"))
        });
        assert!(result.unwrap_err().contains("synthetic write failure"));
        assert!(!path.exists(), "partial snapshot must be removed");
    }

    #[tokio::test]
    async fn interruption_releases_an_inflight_press() {
        let device = Fake(Default::default());
        let press = std::future::pending::<kvm_core::Result<()>>();
        let error = press_and_release(&device, press, std::future::ready(()))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "interrupted; input outcome uncertain; cleanup: confirmed"
        );
        assert_eq!(*device.0.lock().unwrap(), ["release"]);
    }

    fn synthetic(message: &str) -> kvm_core::Error {
        kvm_core::Error::AmbiguousInput(message.into())
    }

    #[tokio::test]
    async fn press_result_ready_with_the_signal_is_used() {
        let device = Fake(Default::default());
        press_and_release(&device, std::future::ready(Ok(())), std::future::ready(()))
            .await
            .unwrap();
        let error = press_and_release(
            &device,
            std::future::ready(Err(synthetic("press"))),
            std::future::ready(()),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            format!(
                "interrupted; press: {}; cleanup: confirmed",
                synthetic("press")
            )
        );
        assert_eq!(*device.0.lock().unwrap(), ["release", "release"]);
    }

    #[tokio::test]
    async fn signal_during_release_is_reported_only_after_a_failed_press() {
        for press in [Ok(()), Err(synthetic("press"))] {
            let device = Fake(Default::default());
            let failed = press.is_err();
            // Resolves only once the release has been requested, so the signal arrives after the press has finished.
            let interrupt = std::future::poll_fn(|_| {
                if device
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| event == "release")
                {
                    std::task::Poll::Ready(())
                } else {
                    std::task::Poll::Pending
                }
            });
            let result = press_and_release(&device, std::future::ready(press), interrupt).await;
            if failed {
                assert_eq!(
                    result.unwrap_err(),
                    format!(
                        "interrupted; press: {}; cleanup: confirmed",
                        synthetic("press")
                    )
                );
            } else {
                result.unwrap();
            }
            assert_eq!(*device.0.lock().unwrap(), ["release"]);
        }
    }

    #[test]
    fn press_outcome_covers_every_press_cleanup_and_signal_combination() {
        let (press, cleanup) = (synthetic("press"), synthetic("cleanup"));
        for (pressed, cleaned, signalled, expected) in [
            (Some(Ok(())), Ok(()), false, Ok(())),
            (Some(Ok(())), Ok(()), true, Ok(())),
            (Some(Ok(())), Err(()), false, Err(cleanup.to_string())),
            (
                Some(Ok(())),
                Err(()),
                true,
                Err(format!("interrupted; press completed; cleanup: {cleanup}")),
            ),
            (
                Some(Err(())),
                Ok(()),
                false,
                Err(format!("{press}; cleanup: confirmed")),
            ),
            (
                Some(Err(())),
                Ok(()),
                true,
                Err(format!("interrupted; press: {press}; cleanup: confirmed")),
            ),
            (
                Some(Err(())),
                Err(()),
                false,
                Err(format!("{press}; cleanup: {cleanup}")),
            ),
            (
                Some(Err(())),
                Err(()),
                true,
                Err(format!("interrupted; press: {press}; cleanup: {cleanup}")),
            ),
            // A press the signal ended is an interruption whatever `signalled` says, since the signal was consumed by ending it.
            (
                None,
                Ok(()),
                false,
                Err("interrupted; input outcome uncertain; cleanup: confirmed".into()),
            ),
            (
                None,
                Ok(()),
                true,
                Err("interrupted; input outcome uncertain; cleanup: confirmed".into()),
            ),
            (
                None,
                Err(()),
                false,
                Err(format!(
                    "interrupted; input outcome uncertain; cleanup: {cleanup}"
                )),
            ),
            (
                None,
                Err(()),
                true,
                Err(format!(
                    "interrupted; input outcome uncertain; cleanup: {cleanup}"
                )),
            ),
        ] {
            let pressed =
                pressed.map(|result: Result<(), ()>| result.map_err(|()| synthetic("press")));
            let cleaned = cleaned.map_err(|()| synthetic("cleanup"));
            assert_eq!(press_outcome(pressed, cleaned, signalled), expected);
        }
    }

    /// Signals this process after the handlers are registered but before their future is first polled. The child first restores the signal's default disposition and unblocks it, so a handler registered only on first poll lets the signal kill the child whatever the parent inherited, and the parent sees that in the child's exit status.
    #[cfg(unix)]
    #[test]
    // Restoring the signal disposition and mask and signalling this process have no safe wrapper in kvm-cli's dependencies.
    #[allow(unsafe_code)]
    fn termination_signals_are_handled_from_registration() {
        const CHILD: &str = "KVM_SIGNAL_REGISTRATION_TEST_CHILD";
        const NAME: &str = "tests::termination_signals_are_handled_from_registration";
        // Bounds only a signal that never reaches the handler, so a regression fails instead of hanging; a working handler resolves as soon as the runtime reads the signal.
        const OBSERVATION_BOUND: Duration = Duration::from_secs(10);
        let Ok(name) = std::env::var(CHILD) else {
            for name in ["TERM", "INT"] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", NAME])
                    .env(CHILD, name)
                    .output()
                    .unwrap();
                let stdout = String::from_utf8_lossy(&output.stdout);
                // A child that ran no test also exits successfully, so the count proves the signal case ran.
                assert!(
                    output.status.success() && stdout.contains("1 passed"),
                    "SIG{name}: {}\n{stdout}\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        };
        let signal = match name.as_str() {
            "TERM" => libc::SIGTERM,
            "INT" => libc::SIGINT,
            other => panic!("unknown signal {other}"),
        };
        // SAFETY: the calls receive a valid signal number and a set initialised by sigemptyset before use.
        unsafe {
            assert_ne!(libc::signal(signal, libc::SIG_DFL), libc::SIG_ERR);
            let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
            assert_eq!(libc::sigemptyset(set.as_mut_ptr()), 0);
            assert_eq!(libc::sigaddset(set.as_mut_ptr(), signal), 0);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_UNBLOCK, set.as_ptr(), std::ptr::null_mut()),
                0
            );
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let interrupt = termination_signal().unwrap();
            // SAFETY: kill(2) on this process's own id with a valid signal number.
            assert_eq!(unsafe { libc::kill(libc::getpid(), signal) }, 0);
            tokio::time::timeout(OBSERVATION_BOUND, interrupt)
                .await
                .expect("signal not observed");
        });
    }

    #[tokio::test]
    async fn ambiguous_key_is_not_replayed_and_cleanup_runs() {
        let device = Fake(Default::default());
        let error = execute(&device, Command::Key { key: "KeyA".into() })
            .await
            .unwrap_err();
        assert!(error.contains("cleanup: confirmed"));
        assert_eq!(*device.0.lock().unwrap(), ["KeyA:true", "release"]);
    }

    #[tokio::test]
    async fn move_reports_coordinates() {
        let device = Fake(Default::default());
        assert_eq!(
            execute(&device, Command::Move { x: -1, y: 2 })
                .await
                .unwrap(),
            json!({"x": -1, "y": 2})
        );
        assert_eq!(*device.0.lock().unwrap(), ["-1,2"]);
    }

    #[tokio::test]
    async fn control_requires_confirmation_and_reports_unverified() {
        let device = Fake(Default::default());
        assert!(
            run_control(&device, Control::Power(PowerAction::On), false)
                .await
                .is_err()
        );
        assert!(device.0.lock().unwrap().is_empty());
        assert_eq!(
            run_control(&device, Control::Power(PowerAction::On), true)
                .await
                .unwrap()["post_state"],
            "unverified"
        );
        assert_eq!(
            run_control(&device, Control::Switch { port: 1 }, true)
                .await
                .unwrap()["post_state"],
            "observed_matching"
        );
        assert_eq!(
            *device.0.lock().unwrap(),
            ["Power(On)", "Switch { port: 1 }"]
        );
    }

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn rejects_password_argument() {
        assert!(
            Cli::try_parse_from([
                "kvm-cli",
                "--endpoint",
                "https://example.com",
                "--profile",
                "pikvm",
                "--username",
                "user",
                "--password",
                "secret",
                "status"
            ])
            .is_err()
        );
    }

    #[test]
    fn totp_is_a_flag_and_cannot_share_password_stdin() {
        let args = [
            "kvm-cli",
            "--endpoint",
            "https://example.com",
            "--profile",
            "pikvm",
            "--username",
            "user",
            "--totp",
            "status",
        ];
        assert!(Cli::try_parse_from(args).unwrap().totp);
        let args = [
            "kvm-cli",
            "--endpoint",
            "https://example.com",
            "--profile",
            "pikvm",
            "--username",
            "user",
            "--totp",
            "--password-stdin",
            "status",
        ];
        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn accepts_negative_coordinates() {
        assert!(
            Cli::try_parse_from([
                "kvm-cli",
                "--endpoint",
                "https://example.com",
                "--profile",
                "pikvm",
                "--username",
                "user",
                "move",
                "-1",
                "2"
            ])
            .is_ok()
        );
    }
}
