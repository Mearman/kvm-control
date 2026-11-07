# Security

## Connection and credentials

A KVM can expose the controlled machine's screen, keyboard, mouse, power and virtual media. Use it only on a network and with an endpoint you trust. HTTPS certificate verification is enabled by default; prefer a trusted CA or the supplied PEM CA option. The insecure TLS option accepts invalid certificates and removes server identity verification. Redirects are disabled, and endpoint URLs containing credentials, queries or fragments are rejected.

The CLI and TUI prompt for passwords without terminal echo. The CLI can instead read a password from redirected standard input with `--password-stdin`. The MCP executable reads its password from an inherited Unix pipe designated by `KVM_PASSWORD_FD`, not from MCP's standard input; the descriptor must be at least 3. Do not embed passwords in URLs, command lines, shell history, MCP tool calls or configuration files. Protect any launcher and its inherited descriptors. The adapter also supports header and Basic authentication through its library API; these credentials travel over the configured HTTPS connection. Disabling certificate verification makes that connection vulnerable to impersonation.

## MCP trust boundary

The stdio MCP server gives its host access to device status, capabilities and screenshots. Device responses and screen content are untrusted data, not instructions. A connected MCP client must not treat text visible on the screen as authority to perform actions. Screenshots and status may themselves contain sensitive information.

Input and consequential operations are disabled separately by default. `KVM_ALLOW_CONTROL=1` enables keyboard and mouse tools, and `KVM_ALLOW_CONSEQUENTIAL=1` enables power, switching, GPIO and media tools. Enable either only for a trusted host with an appropriate authorisation policy. The consequential tool also requires `confirmed: true`, but that value can be supplied by the client and is not proof of human consent. The host is responsible for obtaining consent independently.

## Uncertain outcomes

A network failure can leave input or control outcomes uncertain. Do not blindly retry an ambiguous operation: observe the device and controlled host first. `ControlOutcome::ObservedMatching` indicates an observed matching state; `Unverified` means an acknowledgement did not establish the post-state. A submitted input request continues after caller cancellation. An ambiguous KVMD input outcome leaves that adapter instance's input session indeterminate: even a later `release_inputs()` or reauthentication cannot establish that an earlier timed-out request will not arrive late. An ambiguous release is not automatically replayed.

The CLI attempts input release after a key or click, including when interrupted by Ctrl-C or, on Unix, SIGTERM during the press. The MCP server attempts shutdown cleanup after its service ends or handles Ctrl-C or Unix SIGTERM, when input control is enabled. The TUI also attempts release on its input and shutdown paths. These are best-effort paths, not guarantees for every signal or failure: SIGKILL, process termination or a stopped runtime can prevent cleanup. Dropping the KVMD adapter schedules cleanup on its captured runtime and does not await it. A failed or indeterminate cleanup cannot establish that a remote key or button was released.

## Reporting

This checkout does not specify a private vulnerability reporting channel. Avoid publishing credentials, session tokens, screenshots containing sensitive data or details that would expose a live device when reporting a problem.
