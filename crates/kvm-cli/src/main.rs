use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Parser, Subcommand, ValueEnum};
use kvm_core::{Device, DeviceProfile};
use kvm_kvmd::{Config, Kvmd, LoginState};
use serde_json::{Value, json};
use url::Url;

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
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output)
                .map_err(|e| format!("cannot create snapshot: {e}"))?;
            file.write_all(&snapshot.jpeg)
                .map_err(|e| format!("cannot write snapshot: {e}"))?;
            Ok(json!({"output": output, "bytes": snapshot.jpeg.len()}))
        }
        Command::Key { key } => {
            let result = device.key(&key, true).await;
            let cleanup = device.release_inputs().await;
            result.map_err(|e| {
                format!(
                    "{e}; cleanup: {}",
                    cleanup
                        .as_ref()
                        .map_or_else(ToString::to_string, |()| "confirmed".into())
                )
            })?;
            cleanup.map_err(|e| e.to_string())?;
            Ok(json!({"key": key, "released": true}))
        }
        Command::Click { button } => {
            let result = device.mouse_button(&button, true).await;
            let cleanup = device.release_inputs().await;
            result.map_err(|e| {
                format!(
                    "{e}; cleanup: {}",
                    cleanup
                        .as_ref()
                        .map_or_else(ToString::to_string, |()| "confirmed".into())
                )
            })?;
            cleanup.map_err(|e| e.to_string())?;
            Ok(json!({"button": button, "released": true}))
        }
        Command::Move { x, y } => {
            device.mouse_move(x, y).await.map_err(|e| e.to_string())?;
            Ok(json!({"x": x, "y": y}))
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
    let secret = password(cli.password_stdin)?;
    let device = match cli.auth {
        Auth::Cookie => {
            let device = Kvmd::new(config).map_err(|e| e.to_string())?;
            let mut state = device
                .login(&cli.username, &secret)
                .await
                .map_err(|e| e.to_string())?;
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
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    state = device
                        .complete_approval()
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
            device
        }
        Auth::Header => {
            let device = Kvmd::with_header_auth(config, &cli.username, &secret)
                .map_err(|e| e.to_string())?;
            device.check_auth().await.map_err(|e| e.to_string())?;
            device
        }
    };
    drop(secret);
    execute(&device, cli.command).await
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(value) => println!("{}", json!({"ok": true, "result": value})),
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
        async fn release_inputs(&self) -> kvm_core::Result<()> {
            self.0.lock().unwrap().push("release".into());
            Ok(())
        }
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
