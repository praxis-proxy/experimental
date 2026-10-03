//! RS256 validation plus an administrator-owned active credential registry.
use std::{collections::BTreeMap, path::PathBuf};

use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;

use super::config::Config;

/// Bounded registry size; larger deployments need a dedicated credential store.
const REGISTRY_LIMIT: u64 = 1_048_576;

/// A verification failure without credential material in its diagnostics.
#[derive(Debug, thiserror::Error)]
pub(super) enum AuthError {
    /// Missing, invalid, unregistered or revoked caller credential.
    #[error("invalid caller credential")]
    Invalid,
    /// The authoritative registry could not be loaded safely.
    #[error("credential registry unavailable")]
    Unavailable,
}

/// Required identity claims; standard audience/issuer/time checks are in the JWT library.
#[derive(Clone, Deserialize)]
struct Claims {
    /// Stable metering identity.
    sub: String,
    /// Creation time, also checked against future issuance.
    iat: u64,
    /// Distinct credential identity on every rotation.
    jti: String,
}

/// Versioned active credential registry.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    /// Supported schema version.
    version: u8,
    /// Stable subjects mapped to the one active token digest.
    users: BTreeMap<String, Credential>,
}

/// Non-secret credential metadata.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credential {
    /// SHA-256 of the entire signed token, not just its claims.
    digest: String,
    /// Explicit manual revocation switch.
    active: bool,
}

/// Shared trust configuration for inference and the dashboard callback.
pub(super) struct Verifier {
    /// Pinned public key loaded during configuration validation.
    key: DecodingKey,
    /// Strict algorithm, issuer and audience constraints; expiry is optional.
    validation: Validation,
    /// Live registry path, read on each new authentication.
    path: PathBuf,
}

impl Verifier {
    /// Load and validate the public trust configuration.
    pub(super) fn new(config: &Config) -> Result<Self, praxis_filter::FilterError> {
        if config.issuer.is_empty() || config.audience.is_empty() {
            return Err("manual_jwt: issuer and audience must not be empty".into());
        }
        let key = DecodingKey::from_rsa_pem(&std::fs::read(&config.public_key_file)?)?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&config.issuer]);
        validation.set_audience(&[&config.audience]);
        validation.set_required_spec_claims(&["iss", "aud", "sub"]);
        validation.validate_nbf = true;
        validation.leeway = 0;
        Ok(Self {
            key,
            validation,
            path: config.registry_file.clone(),
        })
    }

    /// Verify RFC 7519 claims and the current registry; never cache admission.
    pub(super) async fn authenticate(&self, token: &str) -> Result<String, AuthError> {
        if token.len() > 8192 {
            return Err(AuthError::Invalid);
        }
        let claims = jsonwebtoken::decode::<Claims>(token, &self.key, &self.validation)
            .map_err(|_error| AuthError::Invalid)?
            .claims;
        if !valid_subject(&claims.sub)
            || claims.jti.is_empty()
            || claims.jti.len() > 128
            || claims.iat > jsonwebtoken::get_current_timestamp()
        {
            return Err(AuthError::Invalid);
        }
        let registry = self.read_registry().await?;
        let digest = hex::encode(Sha256::digest(token.as_bytes()));
        match registry.users.get(&claims.sub) {
            Some(entry) if entry.active && entry.digest == digest => Ok(claims.sub),
            Some(_) | None => Err(AuthError::Invalid),
        }
    }

    /// Read a bounded snapshot so atomic replacement takes effect on the next request.
    async fn read_registry(&self) -> Result<Registry, AuthError> {
        let file = tokio::fs::File::open(&self.path)
            .await
            .map_err(|error| registry_io(&error))?;
        let mut data = Vec::new();
        file.take(REGISTRY_LIMIT.saturating_add(1))
            .read_to_end(&mut data)
            .await
            .map_err(|error| registry_io(&error))?;
        if u64::try_from(data.len()).map_err(|_error| AuthError::Unavailable)? > REGISTRY_LIMIT {
            return Err(AuthError::Unavailable);
        }
        let registry = decode_registry(&data)?;
        if registry.version != 1
            || registry.users.iter().any(|(subject, entry)| {
                !valid_subject(subject)
                    || entry.digest.len() != 64
                    || !entry.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Err(AuthError::Unavailable);
        }
        Ok(registry)
    }
}

/// Parse registry JSON without echoing its contents into diagnostics.
fn decode_registry(data: &[u8]) -> Result<Registry, AuthError> {
    serde_json::from_slice(data).map_err(|error| {
        tracing::warn!(
            line = error.line(),
            column = error.column(),
            "manual_jwt: invalid registry JSON"
        );
        AuthError::Unavailable
    })
}

/// Report registry I/O failures without credential contents.
fn registry_io(error: &std::io::Error) -> AuthError {
    tracing::warn!(kind = ?error.kind(), "manual_jwt: registry I/O failure");
    AuthError::Unavailable
}

/// Subjects remain safe in headers, paths and the manually managed directory.
fn valid_subject(subject: &str) -> bool {
    subject.bytes().next().is_some_and(|byte| byte.is_ascii_alphanumeric())
        && subject.len() <= 128
        && subject
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.@-".contains(&byte))
}
