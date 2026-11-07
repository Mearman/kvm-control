use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Parser, Subcommand, ValueEnum};
use kvm_core::{Confirmation, Control, ControlOutcome, Device, DeviceProfile, PowerAction};
use kvm_kvmd::{Config, Kvmd, LoginState};
use serde_json::{Value, json};
use url::Url;
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(name = "kvm", version, about = "Control a KVMD-compatible device")]
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
    device: &impl Device,
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

async fn termination_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

async fn press_and_release<F, S>(device: &impl Device, press: F, interrupt: S) -> Result<(), String>
where
    F: std::future::Future<Output = kvm_core::Result<()>>,
    S: std::future::Future<Output = io::Result<()>>,
{
    let result = tokio::select! {
        result = press => result.map_err(|e| e.to_string()),
        signal = interrupt => Err(match signal { Ok(()) => "interrupted; input outcome uncertain".into(), Err(e) => format!("signal handler failed: {e}; input outcome uncertain") }),
    };
    let cleanup = device.release_inputs().await;
    result.map_err(|e| {
        format!(
            "{e}; cleanup: {}",
            cleanup
                .as_ref()
                .map_or_else(ToString::to_string, |()| "confirmed".into())
        )
    })?;
    cleanup.map_err(|e| e.to_string())
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

async fn execute(device: &impl Device, command: Command) -> Result<Value, String> {
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
            press_and_release(device, device.key(&key, true), termination_signal()).await?;
            Ok(json!({"key": key, "released": true}))
        }
        Command::Click { button } => {
            press_and_release(
                device,
                device.mouse_button(&button, true),
                termination_signal(),
            )
            .await?;
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
        .map(std::fs::read)
        .transpose()
        .map_err(|e| format!("cannot read CA certificate: {e}"))?;
    if cli.insecure_tls {
        eprintln!("warning: TLS certificate verification is disabled");
    }
    let config = Config {
        endpoint: cli.endpoint,
        profile: match cli.profile {
            Profile::Pikvm => DeviceProfile::PiKvm,
            Profile::Glkvm => DeviceProfile::GlKvm,
        },
        trusted_ca_pem: ca,
        insecure_tls: cli.insecure_tls,
        timeout: Duration::from_secs(30),
    };
    if cli.totp && matches!(cli.auth, Auth::Header) {
        return Err("TOTP requires cookie authentication; header authentication would reuse an expiring code".into());
    }
    let secret = Zeroizing::new(password(cli.password_stdin)?);
    let totp = if cli.totp {
        Some(Zeroizing::new(
            rpassword::prompt_password("TOTP code: ")
                .map_err(|_| "could not read TOTP from terminal")?,
        ))
    } else {
        None
    };
    let device = match cli.auth {
        Auth::Cookie => {
            let device = Kvmd::new(config).map_err(|e| e.to_string())?;
            let login = device
                .login_with_totp(&cli.username, &secret, totp.as_deref().map(String::as_str))
                .await;
            drop(totp);
            drop(secret);
            let mut state = login.map_err(|e| e.to_string())?;
            if let LoginState::ApprovalPending { expires_in } = state {
                eprintln!(
                    "{}",
                    json!({"event": "approval_pending", "expires_in": expires_in, "message": "Approve this login on the device"})
                );
                let expiry = expires_in.ok_or("approval has no expiry")?;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(expiry);
                while matches!(state, LoginState::ApprovalPending { .. }) {
                    if tokio::time::Instant::now() >= deadline {
                        return Err("device approval expired".into());
                    }
                    tokio::time::timeout_at(deadline, async {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        device.complete_approval().await
                    })
                    .await
                    .map_err(|_| "device approval expired")?
                    .map(|next| state = next)
                    .map_err(|e| e.to_string())?;
                }
            }
            device
        }
        Auth::Header => {
            let device = Kvmd::with_header_auth(config, &cli.username, &secret);
            drop(secret);
            let device = device.map_err(|e| e.to_string())?;
            device.check_auth().await.map_err(|e| e.to_string())?;
            device
        }
    };
    execute(&device, cli.command).await
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
                    json!({"ok": true, "result": value})
                )
            {
                eprintln!("cannot write output: {error}");
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("{}", json!({"ok": false, "error": error}));
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

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
        let error = press_and_release(&device, press, std::future::ready(Ok(())))
            .await
            .unwrap_err();
        assert!(error.contains("interrupted"));
        assert_eq!(*device.0.lock().unwrap(), ["release"]);
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
                "kvm",
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
            "kvm",
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
            "kvm",
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
                "kvm",
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
