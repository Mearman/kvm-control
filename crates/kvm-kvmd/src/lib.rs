use std::{collections::BTreeSet, time::Duration};

use async_trait::async_trait;
use kvm_core::{
    Capabilities, Capability, Device, DeviceProfile, DeviceStatus, Error, Result, Snapshot,
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

pub struct Kvmd {
    client: Client,
    endpoint: Url,
    profile: DeviceProfile,
    inputs: Mutex<Inputs>,
    auth_headers: reqwest::header::HeaderMap,
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
        let mut builder = Client::builder()
            .timeout(config.timeout)
            .cookie_store(true)
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

    /// Checks whether the server accepts the configured authentication.
    pub async fn check_auth(&self) -> Result<()> {
        self.json("auth/check").await.map(|_| ())
    }

    /// Returns the explicitly selected device profile.
    pub fn profile(&self) -> DeviceProfile {
        self.profile
    }

    /// Authenticates using KVMD's cookie login. A rejected login may also represent a device-specific challenge.
    pub async fn login(&self, username: &str, password: &str) -> Result<()> {
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
        Ok(())
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
        })
    }

    async fn snapshot(&self) -> Result<Snapshot> {
        let bytes = self
            .get("streamer/snapshot")
            .await?
            .bytes()
            .await
            .map_err(|_| Error::Transport("snapshot read failed".into()))?;
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
