//! Explicit manual credential trust configuration.
use std::path::PathBuf;

use serde::Deserialize;

/// Listener role; dashboard origins are validated before a filter can be built.
pub(super) enum Mode {
    /// Authenticate inference and assert its spending identity.
    Inference,
    /// Private `MaaS`-compatible credential validation endpoint.
    Callback,
    /// Strip untrusted identity and check browser request origins.
    Dashboard(AllowedOrigins),
}

/// Nonempty exact serialized HTTPS origins, with no paths or credentials.
pub(super) struct AllowedOrigins(Vec<String>);

impl AllowedOrigins {
    /// Match only a single, explicitly configured browser origin.
    pub(super) fn contains(&self, origin: &str) -> bool {
        self.0.iter().any(|allowed| origin == allowed)
    }
}

impl TryFrom<Vec<String>> for AllowedOrigins {
    type Error = &'static str;

    fn try_from(origins: Vec<String>) -> Result<Self, Self::Error> {
        if origins.is_empty() || origins.iter().any(|origin| !valid_origin(origin)) {
            return Err("dashboard requires explicit serialized HTTPS origins without paths or trailing slash");
        }
        Ok(Self(origins))
    }
}

/// Trust roots and validated operation of one filter instance.
#[derive(Deserialize)]
#[serde(try_from = "RawConfig")]
pub(super) struct Config {
    /// Exact audience accepted by this deployment.
    pub audience: String,
    /// Exact issuer accepted by this deployment.
    pub issuer: String,
    /// Listener role and its mode-specific constraints.
    pub mode: Mode,
    /// Administrator's RS256 public key; private key stays offline.
    pub public_key_file: PathBuf,
    /// Atomically replaced active-credential registry, mounted as a directory.
    pub registry_file: PathBuf,
}

impl TryFrom<RawConfig> for Config {
    type Error = &'static str;

    fn try_from(raw: RawConfig) -> Result<Self, Self::Error> {
        let mode = match raw.mode {
            RawMode::Inference => Mode::Inference,
            RawMode::Callback => Mode::Callback,
            RawMode::Dashboard => Mode::Dashboard(raw.allowed_origins.try_into()?),
        };
        Ok(Self {
            audience: raw.audience,
            issuer: raw.issuer,
            mode,
            public_key_file: raw.public_key_file,
            registry_file: raw.registry_file,
        })
    }
}

/// Serialized modes retain the existing flat YAML configuration format.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawMode {
    /// Authenticate an inference caller.
    Inference,
    /// Validate a dashboard login on the private callback listener.
    Callback,
    /// Sanitize dashboard traffic and require an allowed browser origin.
    Dashboard,
}

/// Unvalidated input, converted before configuration is exposed to the filter.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    /// Unused outside dashboard mode, for compatibility with shared templates.
    #[serde(default)]
    allowed_origins: Vec<String>,
    /// Exact audience accepted by this deployment.
    audience: String,
    /// Exact issuer accepted by this deployment.
    issuer: String,
    /// Listener role.
    mode: RawMode,
    /// Public trust key path.
    public_key_file: PathBuf,
    /// Live administrator-owned registry path.
    registry_file: PathBuf,
}

/// Require the canonical ASCII serialization used by browser Origin headers.
fn valid_origin(origin: &str) -> bool {
    url::Url::parse(origin)
        .is_ok_and(|parsed| parsed.scheme() == "https" && parsed.origin().ascii_serialization() == origin)
}
