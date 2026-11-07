# Compatibility and verification

## Device profiles

The KVMD adapter provides `PiKvm` and `GlKvm` profiles. Its implementation follows selected upstream KVMD routes and response shapes; the upstream revisions inspected are linked in the module documentation in [`crates/kvm-kvmd/src/lib.rs`](crates/kvm-kvmd/src/lib.rs). A profile name is not a tested device or firmware matrix. No physical hardware was tested for this documentation, and the tests in this checkout do not establish real-device interoperability.

Capability discovery can return `Supported`, `Unsupported` or `Unknown`; unknown is not support. Snapshot and live-video capabilities remain `Unknown` until this connection receives a valid JPEG from the respective operation; a failed operation does not establish that it is unsupported. HID discovery requires a successful HID response: a request failure makes `capabilities()` fail rather than returning unknown HID capabilities. A successful response can still leave keyboard or mouse capability unknown when the relevant state is absent or offline. Absolute mouse movement requires the reported absolute mouse mode. Control operations require the relevant capability to be confirmed and can return an unverified outcome even when accepted.

Cookie login supports a device approval flow. Cookies are accepted from login or approval responses but later cookie rotation and expiry updates are ignored. Firmware requiring rotation is therefore unsupported. On expiry or revocation, authenticate again; do not automatically replay uncertain input or control operations.

## Operation evidence matrix

These rows record implementation and synthetic evidence, not a support guarantee. Model means the adapter profile, not a verified hardware model. Firmware release is unknown in every row; the cited upstream source revisions are not identified firmware releases. Physical status is not tested for this documentation in every row. Test names identify fixtures in this checkout and do not imply a test was run for this documentation.

| Operation | Model/profile | Firmware release | Source revision | Synthetic evidence | Physical status |
| --- | --- | --- | --- | --- | --- |
| Cookie login and status | PiKVM, GLKVM | Unknown | Authentication source revisions P and G below | `cookie_login_and_status_over_verified_tls` and `gl_approval_is_not_reported_as_authenticated` in `crates/kvm-kvmd/tests/https_contract.rs` | Not tested |
| Header and Basic authentication | PiKVM, GLKVM | Unknown | Adapter implementation; firmware revision unknown | `header_auth_over_verified_tls` and `basic_auth_and_discovered_control_requests` in `crates/kvm-kvmd/tests/https_contract.rs` | Not tested |
| Snapshot and capability discovery | PiKVM, GLKVM | Unknown | Adapter implementation; firmware revision unknown | `snapshot_rejects_oversized_response` in `crates/kvm-kvmd/tests/https_contract.rs`; `capabilities_include_mouse_mode` in `crates/kvm-cli/tests/cli.rs` | Not tested |
| Keyboard and mouse | PiKVM, GLKVM | Unknown | Adapter implementation; firmware revision unknown | `key_press_is_followed_by_release` and `absolute_move_rejects_relative_and_unknown_modes` in `crates/kvm-cli/tests/cli.rs` | Not tested |
| Power, switch, GPIO and media control | PiKVM, GLKVM | Unknown | Adapter implementation; firmware revision unknown | `basic_auth_and_discovered_control_requests` in `crates/kvm-kvmd/tests/https_contract.rs`; `protocol_round_trip` in `crates/kvm-mcp/tests/protocol.rs` | Not tested |
| MJPEG stream and WebSocket lease | PiKVM, GLKVM | Unknown | Stream source revisions P, G and U below | `trusted_tls_login_cookie_reaches_lease_and_stream` and `continuous_frames_and_drop_close_lease` in `crates/kvm-kvmd/src/video_tests.rs` | Not tested |
| MCP credential input | Host process, not a device model | Not applicable | `crates/kvm-mcp/src/main.rs` | `accepts_pipe_and_rejects_non_pipe` in `crates/kvm-mcp/src/main.rs` | Not applicable |

P is PiKVM `kvmd` revision `fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef`; G is GLKVM `glkvm` revision `3e8dd23c4bd638a4650433664cc3b0f3b4d29395`; U is uStreamer revision `11fd828ab00c01a39bd0843ef442ef0b6d3cac41`. The specific upstream source files inspected are linked from [`crates/kvm-kvmd/src/lib.rs`](crates/kvm-kvmd/src/lib.rs); these revisions do not prove that all listed operations work on a released device. Synthetic cases cover selected paths and do not establish every operation on both profiles.

## Connection and video

The adapter requires HTTPS and rejects endpoint URLs containing credentials, query parameters or fragments. It does not follow redirects. A direct device origin and a reverse-proxy path prefix are both represented by the same endpoint configuration. A proxy must preserve the API routes and WebSocket upgrade as well as the separate MJPEG route. Live video opens a WebSocket stream lease and reads multipart JPEG from `streamer/stream`; it is not a WebRTC client. A device or proxy that does not expose those routes cannot provide this live stream through the adapter.

## Terminal and platform

The terminal client needs an interactive terminal and a stream of decodable JPEG frames. Its image protocol can be selected explicitly; native image protocols need support from the terminal and, under tmux, externally enabled passthrough. Without terminal key-release events, input falls back to key taps. A capability reported as unknown or unsupported can prevent input forwarding. The MCP executable's inherited password pipe is implemented for Unix only; this is a prerequisite independent of whether another part of the workspace compiles on a platform.

## Evidence in the repository

Unit tests exercise capability handling, input behaviour, video parsing and terminal logic. Integration tests use synthetic devices, loopback HTTPS servers, generated test certificates and fake MCP devices, including protocol and shutdown cases. The example in `examples/in-process` implements a synthetic device. These are useful contract and regression tests, not tests against a physical PiKVM, GLKVM, attached host, native terminal, proxy configuration or a particular firmware release. No hardware, credentials or live device were used to prepare this documentation.

## Hosted verification

The `origin/combined-reconciled` revision `67df5061151629901ffb78ae3922d4056ccc0af6` configures pull-request checks on GitHub-hosted Ubuntu and macOS runners in `.github/workflows/verification.yml`: formatting, Clippy, workspace tests and the independent consumer tests. The corresponding [hosted run](https://github.com/Mearman/kvm-control/actions/runs/42326735406) completed successfully. This is an observation for that revision, not for this documentation branch or a hardware compatibility result. Workflow configuration and actual run results are separate evidence; check the run for the commit being evaluated.

## Distribution

At this revision the repository has workspace manifests and a lockfile but no checked-in release workflow or binary packaging configuration. Do not assume a published package, binary for a particular operating system or architecture, signed release, supported upgrade path or stable API from the version in `Cargo.toml`. Build from source and verify the exact target and device combination you intend to use.
