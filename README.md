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

`key` and `click` press and then release the input. Ctrl-C, or SIGTERM or SIGINT on Unix, during the press ends it and the CLI still releases the input, failing with an `interrupted` error whose input outcome is uncertain. A signal during the release is named in the error only when the press or the release failed; a completed press with a confirmed release still succeeds. Each failure states whether cleanup was confirmed.

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
