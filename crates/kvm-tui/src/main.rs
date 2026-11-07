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
#[command(
    about = "Interactive KVMD terminal client",
    after_help = "Set KVM_TUI_IMAGE_PROTOCOL to halfblocks, kitty, sixel or iterm2 to select an image protocol explicitly. Native protocols in tmux require allow-passthrough to be enabled externally; the application does not change tmux settings."
)]
struct Args {
    /// Device base HTTPS URL, without credentials
    #[arg(long)]
    endpoint: Option<Url>,
    /// Device profile
    #[arg(long, value_enum)]
    profile: Option<Profile>,
    #[arg(long)]
    username: Option<String>,
    /// Prompt for a current TOTP code
    #[arg(long)]
    totp: bool,
    /// PEM certificate authority to trust
    #[arg(long)]
    ca: Option<PathBuf>,
    /// Disable TLS verification for this connection
    #[arg(long)]
    insecure_tls: bool,
}

#[cfg(test)]
mod tests {
    use super::Args;
    use clap::Parser;

    #[test]
    fn accepts_totp_flag_without_a_code_argument() {
        assert!(Args::try_parse_from(["kvm-tui", "--totp"]).unwrap().totp);
        assert!(Args::try_parse_from(["kvm-tui", "--totp", "012345"]).is_err());
    }
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
    let password = zeroize::Zeroizing::new(rpassword::prompt_password("Password: ")?);
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
    let totp = if args.totp {
        Some(zeroize::Zeroizing::new(rpassword::prompt_password(
            "TOTP code: ",
        )?))
    } else {
        None
    };
    let mut state = device
        .login_with_totp(&username, &password, totp.as_deref().map(String::as_str))
        .await?;
    drop(totp);
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
