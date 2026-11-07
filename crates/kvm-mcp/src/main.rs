use std::{env, error::Error, fs, process::ExitCode, sync::Arc, time::Duration};

use kvm_core::DeviceProfile;
use kvm_kvmd::{Config, Kvmd, LoginState};
use kvm_mcp::Server;
use rmcp::ServiceExt;
use url::Url;

#[cfg(unix)]
fn read_password(fd: &str) -> Result<zeroize::Zeroizing<String>, Box<dyn Error>> {
    use std::io::Read;
    use std::os::unix::fs::FileTypeExt;
    let fd: i32 = fd.parse()?;
    if fd < 3 {
        return Err("password descriptor must not be a standard stream".into());
    }
    let file = fs::File::open(format!("/dev/fd/{fd}"))?;
    if !file.metadata()?.file_type().is_fifo() {
        return Err("password descriptor must be an inherited pipe".into());
    }
    const MAX_PASSWORD_BYTES: u64 = 65536;
    let mut password = zeroize::Zeroizing::new(String::new());
    file.take(MAX_PASSWORD_BYTES + 1)
        .read_to_string(&mut password)?;
    if password.len() as u64 > MAX_PASSWORD_BYTES {
        return Err("password input exceeds the size limit".into());
    }
    if password.ends_with('\n') {
        password.pop();
        if password.ends_with('\r') {
            password.pop();
        }
    }
    Ok(password)
}

#[cfg(not(unix))]
fn read_password(_: &str) -> Result<zeroize::Zeroizing<String>, Box<dyn Error>> {
    Err("inherited password pipes are supported only on Unix".into())
}

#[cfg(unix)]
fn shutdown_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! { _ = terminate.recv() => (), _ = interrupt.recv() => () }
    })
}

/// Registers the Ctrl-C handler before returning, as the Unix variant registers its handlers, so the readiness message printed afterwards is true; `tokio::signal::ctrl_c` would register only when first polled.
#[cfg(not(unix))]
fn shutdown_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
    Ok(async move {
        ctrl_c.recv().await;
    })
}

/// The environment variables `kvm-mcp` cannot start without, each with what to set it to.
const REQUIRED_SETTINGS: [(&str, &str); 4] = [
    ("KVM_ENDPOINT", "the device HTTPS URL"),
    ("KVM_PROFILE", "pikvm or glkvm"),
    ("KVM_USERNAME", "the device account name"),
    (
        "KVM_PASSWORD_FD",
        "the number of an inherited pipe descriptor, at least 3, holding the password",
    ),
];

/// The values of [`REQUIRED_SETTINGS`].
struct RequiredSettings {
    endpoint: String,
    profile: String,
    username: String,
    password_fd: String,
}

/// Reads every variable in [`REQUIRED_SETTINGS`].
///
/// # Errors
///
/// Returns one message naming each variable that is unset, with what to set it to, or not valid Unicode; variable values never appear in it.
fn required_settings() -> Result<RequiredSettings, String> {
    let read = REQUIRED_SETTINGS.map(|(name, expected)| {
        env::var(name).map_err(|error| match error {
            env::VarError::NotPresent => format!("{name} is not set (set it to {expected})"),
            env::VarError::NotUnicode(_) => format!("{name} is not valid Unicode"),
        })
    });
    match read {
        [Ok(endpoint), Ok(profile), Ok(username), Ok(password_fd)] => Ok(RequiredSettings {
            endpoint,
            profile,
            username,
            password_fd,
        }),
        read => Err(read
            .into_iter()
            .filter_map(Result::err)
            .collect::<Vec<_>>()
            .join("; ")),
    }
}

/// Prints a startup or serving failure as `Error: ` and its message, rather than the quoted `Debug` form a `Result` returned from `main` prints, and exits with the same failure status.
#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let RequiredSettings {
        endpoint,
        profile,
        username,
        password_fd,
    } = required_settings()?;
    let endpoint = Url::parse(&endpoint)?;
    let profile = match profile.as_str() {
        "pikvm" => DeviceProfile::PiKvm,
        "glkvm" => DeviceProfile::GlKvm,
        _ => return Err("KVM_PROFILE must be pikvm or glkvm".into()),
    };
    let insecure_tls = env::var("KVM_INSECURE_TLS").as_deref() == Ok("1");
    if insecure_tls {
        eprintln!("Warning: TLS certificate verification is disabled");
    }
    let ca = env::var("KVM_CA_FILE").ok().map(fs::read).transpose()?;
    let device = Arc::new(Kvmd::new(Config {
        endpoint,
        profile,
        trusted_ca_pem: ca,
        insecure_tls,
        timeout: Duration::from_secs(30),
    })?);
    let password = read_password(&password_fd)?;
    let totp = env::var("KVM_TOTP_FD")
        .ok()
        .map(|fd| read_password(&fd))
        .transpose()?;
    let login = device
        .login_with_totp(&username, &password, totp.as_deref().map(String::as_str))
        .await?;
    drop(totp);
    drop(password);
    if let LoginState::ApprovalPending { expires_in } = login {
        let seconds = expires_in.ok_or("device approval expiry is missing")?;
        eprintln!("Device approval pending; approve the request on the device.");
        tokio::time::timeout(Duration::from_secs(seconds), async {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if matches!(device.complete_approval().await?, LoginState::Authenticated) {
                    return Ok::<(), kvm_core::Error>(());
                }
            }
        })
        .await
        .map_err(|_| "device approval expired")??;
    }
    let allow_control = env::var("KVM_ALLOW_CONTROL").as_deref() == Ok("1");
    #[cfg(unix)]
    let transport = (
        tokio::net::unix::pipe::Receiver::from_file(fs::File::open("/dev/stdin")?)?,
        tokio::io::stdout(),
    );
    #[cfg(not(unix))]
    let transport = rmcp::transport::stdio();
    let allow_consequential = env::var("KVM_ALLOW_CONSEQUENTIAL").as_deref() == Ok("1");
    let server = Server::new(device.clone(), allow_control, allow_consequential);
    let signal = shutdown_signal()?;
    eprintln!("MCP shutdown handlers registered");
    let service = server.clone().serve(transport).await?;
    let cancel = service.cancellation_token();
    let signal_task = tokio::spawn(async move {
        signal.await;
        cancel.cancel();
    });
    let outcome = service.waiting().await;
    signal_task.abort();
    let _ = signal_task.await;
    if let Err(error) = server.shutdown().await {
        eprintln!("Input cleanup uncertain: {error}");
    }
    outcome?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::read_password;
    use std::{fs::File, io::Write, os::fd::AsRawFd};

    #[test]
    fn reads_synthetic_totp_from_separate_pipe() {
        let (reader, writer) = rustix::pipe::pipe().unwrap();
        let mut writer = File::from(writer);
        writer.write_all(b"012345\n").unwrap();
        drop(writer);
        assert_eq!(
            &*read_password(&reader.as_raw_fd().to_string()).unwrap(),
            "012345"
        );
    }

    #[test]
    fn accepts_pipe_and_rejects_non_pipe() {
        let (reader, writer) = rustix::pipe::pipe().unwrap();
        let mut writer = File::from(writer);
        writer.write_all(b"synthetic-password\n").unwrap();
        drop(writer);
        assert_eq!(
            &*read_password(&reader.as_raw_fd().to_string()).unwrap(),
            "synthetic-password"
        );
        let file = File::open("/dev/null").unwrap();
        assert!(read_password(&file.as_raw_fd().to_string()).is_err());
        assert!(read_password("0").is_err());
    }
}
