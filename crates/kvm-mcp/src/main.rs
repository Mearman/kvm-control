use std::{env, error::Error, fs, sync::Arc, time::Duration};

use kvm_core::{Device, DeviceProfile};
use kvm_kvmd::{Config, Kvmd};
use kvm_mcp::Server;
use rmcp::ServiceExt;
use url::Url;

#[cfg(unix)]
fn read_password(path: &str) -> Result<String, Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err("password file must be a regular file inaccessible to group and others".into());
    }
    use std::io::Read;
    let mut password = String::new();
    file.take(65536).read_to_string(&mut password)?;
    if password.ends_with('\n') {
        password.pop();
        if password.ends_with('\r') {
            password.pop();
        }
    }
    Ok(password)
}

#[cfg(not(unix))]
fn read_password(_: &str) -> Result<String, Box<dyn Error>> {
    Err("password files require Unix permission checks".into())
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
    let password = read_password(&env::var("KVM_PASSWORD_FILE")?)?;
    device.login(&env::var("KVM_USERNAME")?, &password).await?;
    drop(password);
    let allow_control = env::var("KVM_ALLOW_CONTROL").as_deref() == Ok("1");
    let service = Server::new(device.clone(), allow_control)
        .serve(rmcp::transport::stdio())
        .await?;
    let outcome = service.waiting().await;
    if allow_control && let Err(error) = device.release_inputs().await {
        eprintln!("Input cleanup uncertain: {error}");
    }
    outcome?;
    Ok(())
}
