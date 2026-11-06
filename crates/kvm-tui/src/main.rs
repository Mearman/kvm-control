use clap::{Parser, ValueEnum};
use kvm_core::DeviceProfile;
use kvm_kvmd::{Config, Kvmd, LoginState};
use std::{
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};
use url::Url;

#[derive(Parser)]
#[command(about = "Interactive KVMD terminal client")]
struct Args {
    /// Device base HTTPS URL, without credentials
    #[arg(long)]
    endpoint: Option<Url>,
    /// Device profile
    #[arg(long, value_enum)]
    profile: Option<Profile>,
    #[arg(long)]
    username: Option<String>,
    /// PEM certificate authority to trust
    #[arg(long)]
    ca: Option<PathBuf>,
    /// Disable TLS verification for this connection
    #[arg(long)]
    insecure_tls: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Profile {
    Pikvm,
    Glkvm,
}

fn prompt(label: &str) -> io::Result<String> {
    eprint!("{label}: ");
    io::stderr().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_owned())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let endpoint = match args.endpoint {
        Some(endpoint) => endpoint,
        None => prompt("Device HTTPS URL")?.parse()?,
    };
    let profile = match args.profile {
        Some(profile) => profile,
        None => match prompt("Profile (pikvm or glkvm)")?
            .to_ascii_lowercase()
            .as_str()
        {
            "pikvm" => Profile::Pikvm,
            "glkvm" => Profile::Glkvm,
            _ => return Err("unknown device profile".into()),
        },
    };
    let username = match args.username {
        Some(username) => username,
        None => prompt("Username")?,
    };
    let password = rpassword::prompt_password("Password: ")?;
    if args.insecure_tls {
        eprintln!("Warning: TLS certificate verification is disabled");
    }
    let device = Kvmd::new(Config {
        endpoint,
        profile: match profile {
            Profile::Pikvm => DeviceProfile::PiKvm,
            Profile::Glkvm => DeviceProfile::GlKvm,
        },
        trusted_ca_pem: args.ca.map(std::fs::read).transpose()?,
        insecure_tls: args.insecure_tls,
        timeout: Duration::from_secs(15),
    })?;
    let mut state = device.login(&username, &password).await?;
    drop(password);
    while let LoginState::ApprovalPending { expires_in } = state {
        eprintln!(
            "Device approval required (expiry: {expires_in:?} seconds). Approve on the device, then press Enter; type q to cancel."
        );
        if prompt("Continue")?.eq_ignore_ascii_case("q") {
            return Err("device approval cancelled".into());
        }
        state = device.complete_approval().await?;
    }
    kvm_tui::run(std::sync::Arc::new(device)).await?;
    Ok(())
}
