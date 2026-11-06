use std::{collections::BTreeSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use kvm_core::{
    Capabilities, Capability, Confirmation, Control, ControlOutcome, Device, DeviceProfile,
    DeviceStatus, Error, PowerAction, Result, Snapshot,
};
use reqwest::{Client, Method, StatusCode};
use tokio::sync::Mutex;
use url::Url;

/// Connection parameters. Credentials are supplied separately and are never formatted for logs.
pub struct Config {
    /// HTTPS device origin or reverse-proxy prefix; the adapter appends `api/`.
    pub endpoint: Url,
    pub profile: DeviceProfile,
    pub trusted_ca_pem: Option<Vec<u8>>,
    pub insecure_tls: bool,
    pub timeout: Duration,
}

/// Result of the device's explicit two-step login response.
#[derive(Debug, PartialEq, Eq)]
pub enum LoginState {
    Authenticated,
    ApprovalPending { expires_in: Option<u64> },
}

pub struct Kvmd {
    client: Client,
    endpoint: Url,
    profile: DeviceProfile,
    inputs: Mutex<Inputs>,
    auth_headers: reqwest::header::HeaderMap,
    approval_token: Mutex<Option<String>>,
    cookies: Arc<reqwest::cookie::Jar>,
}

#[derive(Default)]
struct Inputs {
    keys: BTreeSet<String>,
    buttons: BTreeSet<String>,
}

impl Kvmd {
    /// Builds a client without contacting the device. TLS verification is enabled unless explicitly disabled.
    pub fn new(config: Config) -> Result<Self> {
        if config.endpoint.scheme() != "https"
            || config.endpoint.host_str().is_none()
            || !config.endpoint.username().is_empty()
            || config.endpoint.password().is_some()
            || config.endpoint.query().is_some()
            || config.endpoint.fragment().is_some()
        {
            return Err(Error::InvalidResponse(
                "endpoint must be an HTTPS URL without credentials, query or fragment".into(),
            ));
        }
        let cookies = Arc::new(reqwest::cookie::Jar::default());
        let mut builder = Client::builder()
            .timeout(config.timeout)
            .cookie_provider(cookies.clone())
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .danger_accept_invalid_certs(config.insecure_tls);
        if let Some(pem) = config.trusted_ca_pem {
            let ca = reqwest::Certificate::from_pem(&pem)
                .map_err(|_| Error::InvalidResponse("invalid CA certificate".into()))?;
            builder = builder.add_root_certificate(ca);
        }
        let client = builder
            .build()
            .map_err(|_| Error::Transport("could not build HTTP client".into()))?;
        Ok(Self {
            client,
            endpoint: config.endpoint,
            profile: config.profile,
            inputs: Mutex::new(Inputs::default()),
            auth_headers: reqwest::header::HeaderMap::new(),
            approval_token: Mutex::new(None),
            cookies,
        })
    }

    /// Builds a client with credentials sent in KVMD's authentication headers.
    pub fn with_header_auth(config: Config, username: &str, password: &str) -> Result<Self> {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut device = Self::new(config)?;
        let mut headers = HeaderMap::new();
        for (name, value) in [("x-kvmd-user", username), ("x-kvmd-passwd", password)] {
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| Error::InvalidResponse("invalid authentication header".into()))?;
            value.set_sensitive(true);
            headers.insert(HeaderName::from_static(name), value);
        }
        // Rebuilding here would lose the configured CA and TLS policy, so headers are held by the device instead.
        device.auth_headers = headers;
        Ok(device)
    }

    /// Builds a client using HTTP Basic authentication over the configured HTTPS connection.
    pub fn with_basic_auth(config: Config, username: &str, password: &str) -> Result<Self> {
        use base64::Engine;
        if username.contains(':') || password.contains(':') {
            return Err(Error::InvalidResponse(
                "Basic credentials cannot contain colons".into(),
            ));
        }
        let mut device = Self::new(config)?;
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        let mut header = reqwest::header::HeaderValue::from_str(&format!("Basic {encoded}"))
            .map_err(|_| Error::InvalidResponse("invalid Basic credentials".into()))?;
        header.set_sensitive(true);
        device
            .auth_headers
            .insert(reqwest::header::AUTHORIZATION, header);
        Ok(device)
    }

    /// Checks whether the server accepts the configured authentication.
    pub async fn check_auth(&self) -> Result<()> {
        self.json("auth/check").await.map(|_| ())
    }

    /// Returns the explicitly selected device profile.
    pub fn profile(&self) -> DeviceProfile {
        self.profile
    }

    /// Authenticates using KVMD's cookie login. A rejected login may also represent a device-specific challenge.
    pub async fn login(&self, username: &str, password: &str) -> Result<LoginState> {
        if !self.auth_headers.is_empty() {
            return Err(Error::InvalidResponse(
                "cookie login cannot replace configured header credentials".into(),
            ));
        }
        let mut pending = self.approval_token.lock().await;
        *pending = None;
        self.cookies
            .add_cookie_str("auth_token=; Max-Age=0; Path=/", &self.endpoint);
        let url = self.url("auth/login")?;
        let response = self
            .client
            .post(url)
            .form(&[("user", username), ("passwd", password)])
            .send()
            .await
            .map_err(|_| Error::Transport("login request failed".into()))?;
        Self::check(response.status())?;
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|_| Error::InvalidResponse("invalid login response".into()))?;
        if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(Error::Authentication);
        }
        let result = body
            .get("result")
            .ok_or_else(|| Error::InvalidResponse("missing login result".into()))?;
        if result
            .get("two_step_required")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            let token = result
                .get("two_step_token")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| Error::InvalidResponse("missing approval token".into()))?;
            let expires_in = result
                .get("expires_in")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| Error::InvalidResponse("missing approval expiry".into()))?;
            *pending = Some(token.to_owned());
            return Ok(LoginState::ApprovalPending {
                expires_in: Some(expires_in),
            });
        }
        Ok(LoginState::Authenticated)
    }

    /// Checks a pending GLKVM approval without exposing its temporary token.
    pub async fn complete_approval(&self) -> Result<LoginState> {
        let mut pending = self.approval_token.lock().await;
        let token = pending
            .as_ref()
            .ok_or_else(|| Error::InvalidResponse("no approval is pending".into()))?;
        let response = self
            .client
            .post(self.url("auth/two_step_complete")?)
            .form(&[("two_step_token", token)])
            .send()
            .await
            .map_err(|_| Error::Transport("approval request failed".into()))?;
        Self::check(response.status())?;
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|_| Error::InvalidResponse("invalid approval response".into()))?;
        if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(Error::Authentication);
        }
        let result = body
            .get("result")
            .ok_or_else(|| Error::InvalidResponse("missing approval result".into()))?;
        if result.get("status").and_then(serde_json::Value::as_str) == Some("pending") {
            return Ok(LoginState::ApprovalPending { expires_in: None });
        }
        if result
            .get("token")
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(Error::InvalidResponse(
                "missing approved session token".into(),
            ));
        }
        *pending = None;
        Ok(LoginState::Authenticated)
    }

    fn url(&self, path: &str) -> Result<Url> {
        let mut base = self.endpoint.clone();
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        base.join(&format!("api/{path}"))
            .map_err(|_| Error::InvalidResponse("invalid endpoint".into()))
    }

    fn check(status: StatusCode) -> Result<()> {
        if status.is_success() {
            Ok(())
        } else if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            Err(Error::Authentication)
        } else {
            Err(Error::Http(status.as_u16()))
        }
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response> {
        let response = self
            .client
            .get(self.url(path)?)
            .headers(self.auth_headers.clone())
            .send()
            .await
            .map_err(|_| Error::Transport("request failed".into()))?;
        Self::check(response.status())?;
        Ok(response)
    }

    async fn json(&self, path: &str) -> Result<serde_json::Value> {
        let value: serde_json::Value = self
            .get(path)
            .await?
            .json()
            .await
            .map_err(|_| Error::InvalidResponse("invalid JSON".into()))?;
        if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(Error::InvalidResponse(
                "KVMD response was not successful".into(),
            ));
        }
        value
            .get("result")
            .cloned()
            .ok_or_else(|| Error::InvalidResponse("missing result".into()))
    }

    async fn discover_endpoint(&self, path: &str) -> Result<Capability> {
        match self.json(path).await {
            Ok(state) => {
                let available = match path {
                    "gpio" => state
                        .pointer("/state/outputs")
                        .and_then(serde_json::Value::as_object)
                        .map(|outputs| !outputs.is_empty()),
                    "switch" => state
                        .pointer("/model/ports")
                        .and_then(serde_json::Value::as_array)
                        .map(|ports| !ports.is_empty()),
                    _ => state.get("enabled").and_then(serde_json::Value::as_bool),
                };
                Ok(match available {
                    Some(true) => Capability::Supported,
                    Some(false) => Capability::Unsupported,
                    None => Capability::Unknown,
                })
            }
            Err(Error::Http(404)) => Ok(Capability::Unsupported),
            Err(error) => Err(error),
        }
    }

    async fn input(&self, path: &str, params: &[(&str, String)]) -> Result<()> {
        let response = self
            .client
            .request(Method::POST, self.url(path)?)
            .headers(self.auth_headers.clone())
            .query(params)
            .send()
            .await
            .map_err(|_| Error::AmbiguousInput("request failed after submission".into()))?;
        Self::check(response.status()).map_err(|e| Error::AmbiguousInput(e.to_string()))?;
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|_| Error::AmbiguousInput("invalid input response".into()))?;
        if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(Error::AmbiguousInput("device did not confirm input".into()));
        }
        Ok(())
    }
}

#[async_trait]
impl Device for Kvmd {
    async fn status(&self) -> Result<DeviceStatus> {
        Ok(DeviceStatus {
            info: self.json("info").await?,
            hid: self.json("hid").await?,
            streamer: self.json("streamer").await?,
        })
    }

    async fn capabilities(&self) -> Result<Capabilities> {
        self.status().await?;
        // Status endpoints do not establish whether individual control operations are supported.
        Ok(Capabilities {
            status: Capability::Supported,
            snapshot: Capability::Unknown,
            keyboard: Capability::Unknown,
            mouse: Capability::Unknown,
            live_video: Capability::Unsupported,
            power: self.discover_endpoint("atx").await?,
            switch: self.discover_endpoint("switch").await?,
            gpio: self.discover_endpoint("gpio").await?,
            media: self.discover_endpoint("msd").await?,
        })
    }

    async fn snapshot(&self) -> Result<Snapshot> {
        // Limit compressed image data before collecting it, including chunked responses.
        const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
        let mut response = self.get("streamer/snapshot").await?;
        if response
            .content_length()
            .is_some_and(|n| n > MAX_SNAPSHOT_BYTES as u64)
        {
            return Err(Error::InvalidResponse(
                "snapshot exceeds the size limit".into(),
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Error::Transport("snapshot read failed".into()))?
        {
            if chunk.len() > MAX_SNAPSHOT_BYTES - bytes.len() {
                return Err(Error::InvalidResponse(
                    "snapshot exceeds the size limit".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        if !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
            return Err(Error::InvalidResponse(
                "snapshot is not a complete JPEG".into(),
            ));
        }
        Ok(Snapshot {
            jpeg: bytes.to_vec(),
        })
    }

    async fn key(&self, key: &str, pressed: bool) -> Result<()> {
        let mut inputs = self.inputs.lock().await;
        if pressed {
            inputs.keys.insert(key.to_owned());
        }
        self.input(
            "hid/events/send_key",
            &[("key", key.into()), ("state", pressed.to_string())],
        )
        .await?;
        if !pressed {
            inputs.keys.remove(key);
        }
        Ok(())
    }

    async fn mouse_button(&self, button: &str, pressed: bool) -> Result<()> {
        let mut inputs = self.inputs.lock().await;
        if pressed {
            inputs.buttons.insert(button.to_owned());
        }
        self.input(
            "hid/events/send_mouse_button",
            &[("button", button.into()), ("state", pressed.to_string())],
        )
        .await?;
        if !pressed {
            inputs.buttons.remove(button);
        }
        Ok(())
    }

    async fn mouse_move(&self, x: i16, y: i16) -> Result<()> {
        let _inputs = self.inputs.lock().await;
        self.input(
            "hid/events/send_mouse_move",
            &[("to_x", x.to_string()), ("to_y", y.to_string())],
        )
        .await
    }

    async fn control(
        &self,
        operation: Control,
        _confirmation: Confirmation,
    ) -> Result<ControlOutcome> {
        let observed = operation.clone();
        let (capability, path, params) = match operation {
            Control::Power(action) => (
                "atx",
                "atx/power",
                vec![
                    (
                        "action",
                        match action {
                            PowerAction::On => "on",
                            PowerAction::Off => "off",
                            PowerAction::OffHard => "off_hard",
                            PowerAction::ResetHard => "reset_hard",
                        }
                        .to_owned(),
                    ),
                    ("wait", "true".to_owned()),
                ],
            ),
            Control::Switch { port } => (
                "switch",
                "switch/set_active",
                vec![("port", port.to_string())],
            ),
            Control::Gpio { channel, state } => (
                "gpio",
                "gpio/switch",
                vec![
                    ("channel", channel),
                    ("state", state.to_string()),
                    ("wait", "true".to_owned()),
                ],
            ),
            Control::MediaConnect { connected } => (
                "msd",
                "msd/set_connected",
                vec![("connected", connected.to_string())],
            ),
            Control::MediaSelect { image } => ("msd", "msd/set_params", vec![("image", image)]),
        };
        if self.discover_endpoint(capability).await? != Capability::Supported {
            return Err(Error::Unsupported(format!(
                "{capability} capability not confirmed"
            )));
        }
        self.input(path, &params)
            .await
            .map_err(|error| Error::AmbiguousControl(error.to_string()))?;
        let state = self.json(capability).await.map_err(|_| {
            Error::AmbiguousControl("request acknowledged but observation failed".into())
        })?;
        let verified = match observed {
            Control::Power(PowerAction::On) => {
                state
                    .pointer("/leds/power")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
            }
            Control::Power(PowerAction::Off | PowerAction::OffHard) => {
                state
                    .pointer("/leds/power")
                    .and_then(serde_json::Value::as_bool)
                    == Some(false)
            }
            Control::Power(PowerAction::ResetHard) => false,
            Control::Switch { port } => {
                state
                    .pointer("/summary/synced")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                    && state
                        .pointer("/summary/active_port")
                        .and_then(serde_json::Value::as_u64)
                        == Some(u64::from(port))
            }
            Control::Gpio {
                channel,
                state: expected,
            } => state
                .pointer("/state/outputs")
                .and_then(|v| v.get(&channel))
                .is_some_and(|output| {
                    output.get("online").and_then(serde_json::Value::as_bool) == Some(true)
                        && output.get("busy").and_then(serde_json::Value::as_bool) == Some(false)
                        && output.get("state").and_then(serde_json::Value::as_bool)
                            == Some(expected)
                }),
            Control::MediaConnect { connected } => {
                state
                    .pointer("/drive/connected")
                    .and_then(serde_json::Value::as_bool)
                    == Some(connected)
            }
            Control::MediaSelect { image } => {
                state
                    .pointer("/drive/image/name")
                    .and_then(serde_json::Value::as_str)
                    == Some(image.as_str())
            }
        };
        Ok(if verified {
            ControlOutcome::ObservedMatching
        } else {
            ControlOutcome::Unverified
        })
    }

    async fn release_inputs(&self) -> Result<()> {
        let mut inputs = self.inputs.lock().await;
        let mut failed = false;
        for key in inputs.keys.clone() {
            if self
                .input(
                    "hid/events/send_key",
                    &[("key", key.clone()), ("state", "false".into())],
                )
                .await
                .is_ok()
            {
                inputs.keys.remove(&key);
            } else {
                failed = true;
            }
        }
        for button in inputs.buttons.clone() {
            if self
                .input(
                    "hid/events/send_mouse_button",
                    &[("button", button.clone()), ("state", "false".into())],
                )
                .await
                .is_ok()
            {
                inputs.buttons.remove(&button);
            } else {
                failed = true;
            }
        }
        if failed {
            Err(Error::AmbiguousInput(
                "one or more releases could not be confirmed".into(),
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(endpoint: &str) -> Config {
        Config {
            endpoint: Url::parse(endpoint).unwrap(),
            profile: DeviceProfile::PiKvm,
            trusted_ca_pem: None,
            insecure_tls: false,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn paths_include_api_and_preserve_proxy_prefix() {
        let client = Kvmd::new(config("https://example.com/proxy")).unwrap();
        assert_eq!(
            client.url("auth/login").unwrap().as_str(),
            "https://example.com/proxy/api/auth/login"
        );
        assert_eq!(
            client.url("hid/events/send_key").unwrap().as_str(),
            "https://example.com/proxy/api/hid/events/send_key"
        );
        let client = Kvmd::new(config("https://example.com/")).unwrap();
        assert_eq!(
            client.url("info").unwrap().as_str(),
            "https://example.com/api/info"
        );
    }

    #[test]
    fn rejects_insecure_and_credential_bearing_endpoints() {
        for endpoint in [
            "http://example.com/",
            "https://user:password@example.com/",
            "https://example.com/?token=x",
            "https://example.com/#fragment",
        ] {
            assert!(Kvmd::new(config(endpoint)).is_err());
        }
    }

    #[test]
    fn auth_statuses_are_distinguished() {
        assert!(matches!(
            Kvmd::check(StatusCode::UNAUTHORIZED),
            Err(Error::Authentication)
        ));
        assert!(matches!(
            Kvmd::check(StatusCode::FORBIDDEN),
            Err(Error::Authentication)
        ));
        assert!(matches!(
            Kvmd::check(StatusCode::NOT_FOUND),
            Err(Error::Http(404))
        ));
        assert!(Kvmd::check(StatusCode::OK).is_ok());
    }
}
