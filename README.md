# kvm-control

Rust libraries and local clients for controlling KVMD-compatible devices over HTTPS. The workspace contains a device abstraction, a KVMD adapter, a command-line client, a terminal client and an MCP server. The implemented PiKVM and GLKVM profiles are not a claim of verified compatibility with every device or firmware version. See [compatibility](COMPATIBILITY.md) and [security](SECURITY.md) before connecting a device.

## Build from source

Install a Rust toolchain supporting the 2024 edition, then from this checkout run `cargo build --workspace` or `cargo test --workspace`. These commands compile the project; they do not establish hardware compatibility. The repository contains no release workflow or packaged binaries at this revision. The workspace version is not evidence that packages have been published or that an API stability guarantee exists.

## Direct connection

Connect to the device's HTTPS origin, or an HTTPS reverse-proxy prefix exposing its KVMD routes. The endpoint must not contain credentials, a query or a fragment. For example, `https://kvm.example.com` is an origin, not a cloud account or relay. The adapter appends `/api/` to the supplied prefix. Live video also needs the prefix's `/streamer/stream` route and a WebSocket upgrade at `/api/ws?stream=true`. Network access to the device or proxy and working device credentials are required.

Use a certificate trusted by the client or supply the device CA with `--ca`. `--insecure-tls` disables certificate verification and should not be used on an untrusted network.

## CLI

Run `cargo run -p kvm-cli -- --help` for the complete options. For example, a read-only status request is:

```sh
cargo run -p kvm-cli -- --endpoint https://kvm.example.com --profile pikvm --username operator status
```

The CLI prompts for the password without echoing it. `--password-stdin` reads redirected standard input instead, not a terminal. Cookie authentication is the default; `--auth header` uses KVMD authentication headers. Device approval may be required during cookie login. Commands include `diagnose`, `status`, `capabilities`, `snapshot`, `key`, `click`, `move`, `power`, `switch`, `gpio`, `media-connect`, `media-select` and `video`. Consequential commands require `--confirm`; acknowledgement is not necessarily proof that the resulting state was observed. Snapshots are written to a new file and video emits newline-delimited JSON containing base64 JPEG frames; use `video --frames N` to bound the stream.

## Terminal client

Run `cargo run -p kvm-tui -- --endpoint https://kvm.example.com --profile pikvm --username operator`. Missing connection fields are prompted for, as is the password. An interactive terminal with a usable video stream is needed for the native terminal experience. Images use halfblocks or a terminal image protocol; set `KVM_TUI_IMAGE_PROTOCOL` to `halfblocks`, `kitty`, `sixel` or `iterm2` to choose explicitly. Native image protocols inside tmux require `allow-passthrough` to be enabled externally; the client does not change tmux configuration. Terminal keyboard release support affects whether held keys can be represented; otherwise keys are tapped.

In viewing mode, `i` enters input mode, `F12` leaves it, `r` refreshes, and `q` quits. `:` opens the control command prompt; commands include `power on`, `switch 1`, `gpio CHANNEL on`, `media connect`, `media disconnect` and `media select IMAGE`. The client asks for confirmation before consequential operations. Mouse forwarding requires a device reporting supported absolute mouse mode.

## MCP server

`kvm-mcp` is a local stdio MCP server, not a network listener. It reads `KVM_ENDPOINT`, `KVM_PROFILE` (`pikvm` or `glkvm`), `KVM_USERNAME` and `KVM_PASSWORD_FD`. On Unix the last variable names an inherited pipe descriptor numbered at least 3, containing the password. The launcher must arrange the pipe separately from MCP stdin and close its write end after supplying the password. Do not put passwords in command arguments, URLs, MCP tool arguments or environment variables. The current password-descriptor implementation does not support non-Unix systems.

Optional `KVM_CA_FILE` selects a PEM CA; `KVM_INSECURE_TLS=1` disables certificate verification. Status, capabilities and snapshot tools are available by default. `KVM_ALLOW_CONTROL=1` enables key and mouse operations; `KVM_ALLOW_CONSEQUENTIAL=1` independently enables power, switch, GPIO and media operations. A tool's `confirmed` field is not evidence of human consent: the host must obtain consent independently. See [security](SECURITY.md).

## Library

`kvm-core` exposes the async `Device` and `VideoStream` traits, status and capability types, input methods and explicitly confirmed control operations. `kvm-kvmd` implements `Device` as `Kvmd`: construct it with `Config`, authenticate with `login` and complete approval if needed, then call the trait methods. Header and HTTP Basic authentication constructors are also available. Inspect `examples/in-process/src/main.rs` for an in-process consumer using a synthetic `Device`; run it with `cargo run --manifest-path examples/in-process/Cargo.toml`. It is not a hardware demonstration.
