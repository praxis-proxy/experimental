//! Explicit manual credential trust configuration.
use std::path::PathBuf;

use serde::Deserialize;

/// Listener role; authentication must never be conditional within a chain.
#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Mode {
    /// Authenticate inference and assert its spending identity.
    Inference,
    /// Private `MaaS`-compatible credential validation endpoint.
    Callback,
    /// Strip untrusted dashboard identity and check browser request origins.
    Dashboard,
}

/// Trust roots and operation of one filter instance.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    /// Listener role.
    pub mode: Mode,
    /// Administrator's RS256 public key; private key stays offline.
    pub public_key_file: PathBuf,
    /// Atomically replaced active-credential registry, mounted as a directory.
    pub registry_file: PathBuf,
    /// Exact issuer accepted by this deployment.
    pub issuer: String,
    /// Exact audience accepted by this deployment.
    pub audience: String,
    /// HTTPS origins permitted for unsafe browser requests, including login.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}
