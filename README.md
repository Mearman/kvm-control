# kvm-control

Rust libraries and local clients for controlling KVMD-compatible devices over HTTPS. The workspace contains a device abstraction, a KVMD adapter, a command-line client, a terminal client and an MCP server. The implemented PiKVM and GLKVM profiles are not a claim of verified compatibility with every device or firmware version. See [compatibility](COMPATIBILITY.md) and [security](SECURITY.md) before connecting a device.

## Build from source

Install a Rust toolchain supporting the 2024 edition, then from this checkout run `cargo build --workspace` or `cargo test --workspace`. These commands compile the project; they do not establish hardware compatibility. Pull requests and pushes to `main` are checked by `.github/workflows/ci.yml`; see [compatibility](COMPATIBILITY.md). The version in `Cargo.toml` is not the release version, which comes from the release tag, and no crate is published to a package registry. Neither is evidence of an API stability guarantee.

## Install from a release

Each [GitHub release](https://github.com/Mearman/kvm-control/releases) has one archive for each of `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin` and `aarch64-apple-darwin`, named `kvm-control-TAG-TARGET.tar.gz` and holding the `kvm-cli`, `kvm-tui` and `kvm-mcp` binaries, with a SHA-256 checksum file named after the archive plus `.sha256`. Download the archive for your platform and its checksum file, check the archive against it, then unpack it:

```sh
tag=$(curl -fsSL -o /dev/null -w '%{url_effective}' https://github.com/Mearman/kvm-control/releases/latest | sed 's|.*/||')
target=aarch64-apple-darwin
archive="kvm-control-$tag-$target.tar.gz"
curl -fsSLO "https://github.com/Mearman/kvm-control/releases/download/$tag/$archive"
curl -fsSLO "https://github.com/Mearman/kvm-control/releases/download/$tag/$archive.sha256"
shasum -a 256 -c "$archive.sha256"
mkdir kvm-control && tar -xzf "$archive" -C kvm-control
kvm-control/kvm-cli --version
```

`sha256sum -c` reads the same checksum file on Linux. Put the unpacked binaries on your `PATH` to run them as `kvm-cli`, `kvm-tui` and `kvm-mcp`. The binaries are not signed or notarised; a checksum shows the download matches what the release published, not who built it. Building from source remains the way to run a commit or target that has no release.

## Direct connection

Connect to the device's HTTPS origin, or an HTTPS reverse-proxy prefix exposing its KVMD routes. The endpoint must not contain credentials, a query or a fragment. For example, `https://kvm.example.com` is an origin, not a cloud account or relay. The adapter appends `/api/` to the supplied prefix. Live video also needs the prefix's `/streamer/stream` route and a WebSocket upgrade at `/api/ws?stream=true`. Network access to the device or proxy and working device credentials are required.

Use a certificate trusted by the client or supply the device CA with `--ca`. `--insecure-tls` disables certificate verification and should not be used on an untrusted network.

## CLI

Run `cargo run -p kvm-cli -- --help` for the complete options. For example, a read-only status request is:

```sh
cargo run -p kvm-cli -- --endpoint https://kvm.example.com --profile pikvm --username operator status
```

The CLI prompts for the password without echoing it. `--password-stdin` reads redirected standard input instead, not a terminal. Cookie authentication is the default; `--auth header` uses KVMD authentication headers. Device approval may be required during cookie login. Commands include `diagnose`, `status`, `capabilities`, `snapshot`, `key`, `click`, `move`, `power`, `switch`, `gpio`, `media-connect`, `media-select` and `video`. Consequential commands require `--confirm`; acknowledgement is not necessarily proof that the resulting state was observed. Snapshots are written to a new file and video emits newline-delimited JSON containing base64 JPEG frames; use `video --frames N` to bound the stream.

`key` and `click` press and then release the input, releasing it whatever happens to the press. Ctrl-C, or SIGTERM or SIGINT on Unix, that arrives before the press result is available ends the wait, and the CLI fails with an `interrupted` error whose input outcome is uncertain. A signal observed once the press result is available, or during the release, is named in the error only when the press or the release failed (`interrupted; press: <error>` or `interrupted; press completed`); a completed press with a confirmed release still succeeds. Every failure after a press carries the cleanup result.

## Terminal client

Run `cargo run -p kvm-tui -- --endpoint https://kvm.example.com --profile pikvm --username operator`. Missing connection fields are prompted for, as is the password. An interactive terminal with a usable video stream is needed for the native terminal experience. Images use halfblocks or a terminal image protocol. After entering the alternate screen the client asks the terminal which image protocols and keyboard enhancements it supports, waiting a bounded time for the answer; a terminal that does not answer is judged from `TERM`, `TERM_PROGRAM` and tmux detection, and halfblocks are used when nothing better is known. Set `KVM_TUI_IMAGE_PROTOCOL` to `halfblocks`, `kitty`, `sixel` or `iterm2` to choose the image protocol explicitly; the setting overrides detection. Native image protocols inside tmux require `allow-passthrough` to be enabled externally; the client does not change tmux configuration. Terminal keyboard release support affects whether held keys can be represented; otherwise keys are tapped. SIGTERM or SIGINT at any point, detection included, restores the terminal before the client exits.

In viewing mode, `i` enters input mode, `F12` leaves it, `r` refreshes, and `q` quits. `:` opens the control command prompt; commands include `power on`, `switch 1`, `gpio CHANNEL on`, `media connect`, `media disconnect` and `media select IMAGE`. The client asks for confirmation before consequential operations. Mouse forwarding requires a device reporting supported absolute mouse mode.

## MCP server

`kvm-mcp` is a local stdio MCP server, not a network listener. It reads `KVM_ENDPOINT`, `KVM_PROFILE` (`pikvm` or `glkvm`), `KVM_USERNAME` and `KVM_PASSWORD_FD`. On Unix the last variable names an inherited pipe descriptor numbered at least 3, containing the password. The launcher must arrange the pipe separately from MCP stdin and close its write end after supplying the password. Do not put passwords in command arguments, URLs, MCP tool arguments or environment variables. The current password-descriptor implementation does not support non-Unix systems.

Optional `KVM_CA_FILE` selects a PEM CA; `KVM_INSECURE_TLS=1` disables certificate verification. Status, capabilities and snapshot tools are available by default. `KVM_ALLOW_CONTROL=1` enables key and mouse operations; `KVM_ALLOW_CONSEQUENTIAL=1` independently enables power, switch, GPIO and media operations. A tool's `confirmed` field is not evidence of human consent: the host must obtain consent independently. See [security](SECURITY.md).

## Two-factor authentication

For devices requiring TOTP, `--totp` prompts for a current code in the CLI or TUI. The CLI requires cookie authentication and an interactive password prompt for this option. For MCP, `KVM_TOTP_FD` identifies a separate inherited Unix pipe containing the current code. Do not put the code in arguments, environment variables or MCP messages. Login rejection does not say whether TOTP is required; provide a fresh code for each login. The adapter appends the six-digit code to the password for cookie login and limits the combined GLKVM credential to 63 bytes. The inspected [PiKVM](https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/kvmd/auth.py) and [GLKVM](https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/kvmd/auth.py) source revisions implement this password-and-code format.

## Library

`kvm-core` exposes the async `Device` and `VideoStream` traits, status and capability types, input methods and explicitly confirmed control operations. `kvm-kvmd` implements `Device` as `Kvmd`: construct it with `Config`, authenticate with `login` or `login_with_totp` and complete approval if needed, then call the trait methods. Header and HTTP Basic authentication constructors are also available. Inspect `examples/in-process/src/main.rs` for an in-process consumer using a synthetic `Device`; run it with `cargo run --manifest-path examples/in-process/Cargo.toml`. It is not a hardware demonstration.

Library callers should explicitly await `release_inputs()` when ending input and handle its error. A submitted KVMD input request continues if its caller is cancelled; an ambiguous outcome must not be replayed blindly and leaves cleanup indeterminate for that adapter instance. Dropping the adapter attempts asynchronous cleanup on its captured runtime, but does not wait for confirmation and cannot guarantee cleanup after runtime or process termination. Dropping a video stream releases its transport resources. See [security](SECURITY.md) for the client shutdown boundaries.

### Errors

Every fallible call returns `kvm_core::Error`. The enum's documentation says when each variant occurs and what a caller should do. It is on `main` (`cargo doc -p kvm-core --open` in a checkout of `main`) and ships with the first release that follows the commit adding it, so a release tagged before that commit lacks it. The variants divide by whether the device may have acted: `Authentication`, `Unsupported`, `Transport`, `Http` and `InvalidResponse` returned by an input or control method mean the requested input or control was not submitted (releases owed to earlier interrupted presses may have been sent first), while `AmbiguousInput` and `AmbiguousControl` mean it was, or may have been, submitted with an unknown effect. `login` and `complete_approval` return `AmbiguousInput` without sending their own request while inputs are held, or when the task sending owed releases failed (a panic or runtime shutdown), which is as ambiguous as a failed input request; both may have sent owed releases first. A failed owed release itself is reported to diagnostics, leaves input indeterminate and does not stop the login or approval. Never replay an ambiguous operation automatically; observe the device and the controlled host first. An acknowledged control request whose effect could not be observed is not an error but `Ok(ControlOutcome::Unverified)`.

### Using the crates from another project

No crate is published to crates.io or any other registry, and publishing one needs separate authorisation, so another project depends on this repository through Cargo's git dependencies. Pin a release tag, or the commit it points to with `rev`, and use the same reference for every crate from this repository: `kvm-kvmd` takes `kvm-core` from the checkout it was built from, so a tag for one crate and a `rev` for the other, even naming the same commit, gives two distinct `kvm-core` crates whose `Device` trait and `Error` type do not match. The [releases](https://github.com/Mearman/kvm-control/releases) list the tags; `v0.1.1` below stands for the release you have reviewed.

```toml
[dependencies]
kvm-core = { git = "https://github.com/Mearman/kvm-control", tag = "v0.1.1" }
kvm-kvmd = { git = "https://github.com/Mearman/kvm-control", tag = "v0.1.1" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
url = "2"
zeroize = "1"
```

To pin by commit, replace `tag = "v0.1.1"` with `rev = "529c8f88010c8b9d1342914a33bbe8c185209132"`, the commit `v0.1.1` points to, in both lines. `Cargo.lock` records the resolved commit either way; change the reference in `Cargo.toml` to move to another release. The crates use the 2024 edition and declare no minimum supported Rust version, so build them with a current stable toolchain. A project that only consumes or implements the `Device` contract, as [`examples/in-process`](examples/in-process) does, needs `kvm-core` and not `kvm-kvmd`; its [manifest](examples/in-process/Cargo.toml) lists the other crates that implementing the traits needs.

A minimal consumer of a real device runs the adapter in its own process, like `examples/in-process`, but with `Kvmd` in place of the synthetic device. It reads the password from redirected standard input; the key press types the letter `a` on the controlled host.

```rust
use std::{io::BufRead, time::Duration};

use kvm_core::{Capability, Device, DeviceProfile};
use kvm_kvmd::{Config, Kvmd, LoginState};
use url::Url;
use zeroize::Zeroizing;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Read the password from redirected standard input, never from arguments or the environment.
    // Zeroizing clears this String's final buffer when it is dropped; buffers freed as read_line grows the String, and standard input's own buffer, keep their bytes until reused.
    let mut password = Zeroizing::new(String::new());
    std::io::stdin().lock().read_line(&mut password)?;
    let adapter = Kvmd::new(Config {
        endpoint: Url::parse("https://kvm.example.com/")?,
        profile: DeviceProfile::PiKvm,
        trusted_ca_pem: None,
        insecure_tls: false,
        timeout: Duration::from_secs(10),
    })?;
    let state = adapter
        .login("operator", password.trim_end_matches(['\r', '\n']))
        .await?;
    drop(password);
    if state != LoginState::Authenticated {
        return Err("approve the login on the device, then call complete_approval".into());
    }
    // Depend on the Device contract, as examples/in-process does.
    let device: &dyn Device = &adapter;
    println!("Snapshot: {} bytes", device.snapshot().await?.jpeg.len());
    if device.capabilities().await?.keyboard == Capability::Supported {
        let pressed = device.key("KeyA", true).await;
        // Release even when the press has an ambiguous outcome.
        let released = device.release_inputs().await;
        pressed?;
        released?;
    }
    Ok(())
}
```

**Runtime and shared state.** Call the adapter from inside a Tokio runtime: input requests and JPEG validation run as tasks on the runtime that made the call, and dropping the adapter schedules its input cleanup on the runtime that last took the input lock, so keep that runtime driven until they finish. `Kvmd` is `Send` and `Sync` but not `Clone`; share one instance between tasks behind an `Arc` (an `Arc<Kvmd>` coerces to `Arc<dyn Device>`). The instance owns the authenticated session, the record of held keys and buttons and whether an ambiguous outcome has made input indeterminate, so a second instance for the same device shares none of that and cannot release what the first pressed. Input calls on one instance run one at a time, in the order they request the input lock. The snapshot capability stays `Unknown` until that instance's `snapshot()` has returned a valid JPEG, and live video until one of its video streams has delivered a frame. Once its local checks and the held-input check pass, `login` discards the instance's current session before sending its request, so a login that fails after that point leaves the instance unauthenticated; one rejected by those checks, such as for a malformed TOTP code or held inputs, keeps the old session. The adapter bounds how many snapshots and how much video stream storage it holds at once, process-wide: every `Kvmd` instance in the process shares those bounds, and a call beyond them returns `Error::Transport`.

**TLS.** `Config::endpoint` must be an HTTPS URL without credentials, a query or a fragment; `Kvmd::new` rejects anything else with `Error::InvalidResponse`. Certificates are verified against the Mozilla root set the crates bundle through `webpki-roots`, not the operating system's trust store, plus the PEM certificate in `trusted_ca_pem`, which is the way to trust a device's own CA. `insecure_tls: true` disables verification entirely, including the host name check, and exposes the session and credentials to anyone who can intercept the connection. The adapter speaks HTTP/1.1, follows no redirects and retries nothing.

**Secrets.** `Config` holds no credentials. Pass them to `login`, `login_with_totp`, `Kvmd::with_header_auth` or `Kvmd::with_basic_auth`, read from a prompt, a pipe or a secret store, never from command-line arguments, environment variables, the endpoint URL or a configuration file. The adapter keeps its copy of the cookie-login password and code in a buffer that is zeroed when dropped, though copies made while encoding the request are not, and marks header credentials sensitive so they are not formatted in debug output; the caller's own copies are the caller's to clear. `zeroize::Zeroizing` in the example clears only the string's final buffer: buffers freed as `read_line` grows the string, and standard input's own buffer, are not cleared, so treat process memory as holding the password until the process exits. A TOTP code is valid for one login, so obtain a fresh one each time. See [security](SECURITY.md).

**API stability.** There is no API stability guarantee, and any release may change the public API. Release versions come from commit messages: below 1.0.0 a commit marked as breaking bumps the minor version and every other releasing commit the patch version (see [compatibility](COMPATIBILITY.md)), but nothing compares the public API between releases, so a breaking change is only recognised when its commit says so. Pin an exact tag or commit, and read the release notes and the crate documentation before moving to another.

### Video transports

The adapter implements one live-video transport, MJPEG over uStreamer's HTTP stream. The [video transport investigation](docs/video-transports.md) describes the H.264 and WebRTC transports, including GLKVM's adaptive WebRTC mode, that the inspected PiKVM and GLKVM sources also offer, how each is negotiated, what the adapter does when a device offers only one of them, and a design for selecting a transport from discovered capabilities.
