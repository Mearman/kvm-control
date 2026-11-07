use clap::{Parser, ValueEnum};
use kvm_core::{
    AuthScheme, Connected, Connector, Credentials, Device, DeviceProfile, Endpoint, LoginState,
    ServerTrust,
};
use kvm_kvmd::KvmdConnector;
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
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
    use super::{Args, read_ca};
    use clap::Parser;

    #[test]
    fn accepts_totp_flag_without_a_code_argument() {
        assert!(Args::try_parse_from(["kvm-tui", "--totp"]).unwrap().totp);
        assert!(Args::try_parse_from(["kvm-tui", "--totp", "012345"]).is_err());
    }

    #[test]
    fn unreadable_ca_names_the_option_and_path() {
        let path =
            std::env::temp_dir().join(format!("kvm-tui-missing-ca-{}.pem", std::process::id()));
        let error = read_ca(&path).unwrap_err();
        assert!(
            error.starts_with(&format!(
                "cannot read --ca certificate {}: ",
                path.display()
            )),
            "{error}"
        );
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Profile {
    Pikvm,
    Glkvm,
}

/// Reads the `--ca` certificate file.
///
/// # Errors
///
/// Returns a message naming `--ca` and the path when the file cannot be read; the file's contents never appear in it.
fn read_ca(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read --ca certificate {}: {error}", path.display()))
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
    // Read before any prompt, so a wrong path fails before the password is asked for.
    let trusted_ca_pem = args.ca.as_deref().map(read_ca).transpose()?;
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
    let endpoint = Endpoint {
        label: None,
        address: endpoint,
        profile: match profile {
            Profile::Pikvm => DeviceProfile::PiKvm,
            Profile::Glkvm => DeviceProfile::GlKvm,
        },
        auth: AuthScheme::Session,
        trust: if args.insecure_tls {
            ServerTrust::Disabled
        } else {
            ServerTrust::Verified {
                additional_ca_pem: trusted_ca_pem,
            }
        },
        timeout: Duration::from_secs(15),
    };
    let mut credentials = Credentials::new(username, password);
    if args.totp {
        credentials = credentials.with_totp(zeroize::Zeroizing::new(rpassword::prompt_password(
            "TOTP code: ",
        )?));
    }
    let connection = match KvmdConnector.connect(endpoint, credentials).await? {
        Connected::Authenticated(connection) => connection,
        Connected::ApprovalPending {
            connection,
            expires_in,
        } => {
            let mut state = LoginState::ApprovalPending { expires_in };
            while let LoginState::ApprovalPending { expires_in } = state {
                eprintln!(
                    "Device approval required (expiry: {expires_in:?} seconds). Approve on the device, then press Enter; type q to cancel."
                );
                if prompt("Continue")?.eq_ignore_ascii_case("q") {
                    return Err("device approval cancelled".into());
                }
                state = connection.complete_approval().await?;
            }
            connection
        }
    };
    let device: std::sync::Arc<dyn Device> = connection;
    kvm_tui::run(device).await?;
    Ok(())
}
