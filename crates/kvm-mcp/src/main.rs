use std::{env, error::Error, fs, sync::Arc, time::Duration};

use kvm_core::{Device, DeviceProfile};
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
async fn shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    tokio::select! { _ = terminate.recv() => (), _ = interrupt.recv() => () }
    Ok(())
}

#[cfg(not(unix))]
async fn shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let endpoint = Url::parse(&env::var("KVM_ENDPOINT")?)?;
    let profile = match env::var("KVM_PROFILE")?.as_str() {
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
    let password = read_password(&env::var("KVM_PASSWORD_FD")?)?;
    let login = device.login(&env::var("KVM_USERNAME")?, &password).await?;
    if let LoginState::ApprovalPending { .. } = login {
        return Err("device approval is pending; MCP session was not started".into());
    }
    drop(password);
    let allow_control = env::var("KVM_ALLOW_CONTROL").as_deref() == Ok("1");
    #[cfg(unix)]
    let transport = (
        tokio::net::unix::pipe::Receiver::from_file(fs::File::open("/dev/stdin")?)?,
        tokio::io::stdout(),
    );
    #[cfg(not(unix))]
    let transport = rmcp::transport::stdio();
    let allow_consequential = env::var("KVM_ALLOW_CONSEQUENTIAL").as_deref() == Ok("1");
    let service = Server::new(device.clone(), allow_control, allow_consequential)
        .serve(transport)
        .await?;
    let cancel = service.cancellation_token();
    let signal_task = tokio::spawn(async move {
        if let Err(error) = shutdown_signal().await {
            eprintln!("Could not listen for shutdown signal: {error}");
        }
        cancel.cancel();
    });
    let outcome = service.waiting().await;
    signal_task.abort();
    let _ = signal_task.await;
    if allow_control && let Err(error) = device.release_inputs().await {
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
