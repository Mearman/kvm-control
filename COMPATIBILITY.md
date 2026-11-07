# Compatibility and verification

## Device profiles

The KVMD adapter provides `PiKvm` and `GlKvm` profiles. Its implementation follows selected upstream KVMD routes and response shapes; the upstream revisions inspected are linked in the module documentation in [`crates/kvm-kvmd/src/lib.rs`](crates/kvm-kvmd/src/lib.rs). A profile name is not a tested device or firmware matrix. No physical hardware was tested for this documentation, and the tests in this checkout do not establish real-device interoperability.

Capability discovery can return `Supported`, `Unsupported` or `Unknown`; unknown is not support. Availability depends on what a device reports and exposes. Absolute mouse movement requires the reported absolute mouse mode. Control operations require the relevant capability to be confirmed and can return an unverified outcome even when accepted.

Cookie login supports a device approval flow. Cookies are accepted from login or approval responses but later cookie rotation and expiry updates are ignored. Firmware requiring rotation is therefore unsupported. On expiry or revocation, authenticate again; do not automatically replay uncertain input or control operations.

## Connection and video

The adapter requires HTTPS and rejects endpoint URLs containing credentials, query parameters or fragments. It does not follow redirects. A direct device origin and a reverse-proxy path prefix are both represented by the same endpoint configuration. A proxy must preserve the API routes and WebSocket upgrade as well as the separate MJPEG route. Live video opens a WebSocket stream lease and reads multipart JPEG from `streamer/stream`; it is not a WebRTC client. A device or proxy that does not expose those routes cannot provide this live stream through the adapter.

## Terminal and platform

The terminal client needs an interactive terminal and a stream of decodable JPEG frames. Its image protocol can be selected explicitly; native image protocols need support from the terminal and, under tmux, externally enabled passthrough. Without terminal key-release events, input falls back to key taps. A capability reported as unknown or unsupported can prevent input forwarding. The MCP executable's inherited password pipe is implemented for Unix only; this is a prerequisite independent of whether another part of the workspace compiles on a platform.

## Evidence in the repository

Unit tests exercise capability handling, input behaviour, video parsing and terminal logic. Integration tests use synthetic devices, loopback HTTPS servers, generated test certificates and fake MCP devices, including protocol and shutdown cases. The example in `examples/in-process` implements a synthetic device. These are useful contract and regression tests, not tests against a physical PiKVM, GLKVM, attached host, native terminal, proxy configuration or a particular firmware release. No hardware, credentials or live device were used to prepare this documentation.

## Distribution

At this revision the repository has workspace manifests and a lockfile but no checked-in release workflow or binary packaging configuration. Do not assume a published package, binary for a particular operating system or architecture, signed release, supported upgrade path or stable API from the version in `Cargo.toml`. Build from source and verify the exact target and device combination you intend to use.
