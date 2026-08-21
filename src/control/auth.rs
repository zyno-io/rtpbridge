//! HMAC authorization for control-plane HTTP and WebSocket upgrades.
//!
//! Audio WebSocket endpoints use their server-minted, single-use connect token
//! instead. Keeping that capability separate means an AI/media consumer never
//! needs the shared control signing key.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

const AUTH_SCHEME: &str = "HMAC-SHA256";
const CANONICAL_PREFIX: &str = "rtpbridge-auth-v1";
const MAX_FUTURE_SKEW_SECS: i64 = 5;
const MIN_SECRET_BYTES: usize = 32;

type HmacSha256 = Hmac<Sha256>;

/// Authorization failure intentionally has no detailed public error reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    Unauthorized,
}

/// Verifies short-lived HMAC request signatures without exposing key material in
/// loggable configuration structures.
#[derive(Clone)]
pub struct HmacAuthenticator {
    secret: Arc<[u8]>,
    max_age_secs: u64,
}

impl std::fmt::Debug for HmacAuthenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HmacAuthenticator")
            .field("secret", &"[REDACTED]")
            .field("max_age_secs", &self.max_age_secs)
            .finish()
    }
}

impl HmacAuthenticator {
    /// Load a key from a mounted secret file. A terminal newline is ignored to
    /// support conventional Kubernetes Secret-file rendering.
    pub fn from_secret_file(path: &Path, max_age_secs: u64) -> anyhow::Result<Self> {
        let mut secret = std::fs::read(path)?;
        while matches!(secret.last(), Some(b'\n' | b'\r')) {
            secret.pop();
        }
        Self::new(secret, max_age_secs)
    }

    pub fn new(secret: Vec<u8>, max_age_secs: u64) -> anyhow::Result<Self> {
        if secret.len() < MIN_SECRET_BYTES {
            anyhow::bail!(
                "HMAC authorization secret must contain at least {MIN_SECRET_BYTES} bytes"
            );
        }
        if max_age_secs == 0 {
            anyhow::bail!("HMAC authorization maximum age must be greater than zero");
        }
        Ok(Self {
            secret: Arc::from(secret),
            max_age_secs,
        })
    }

    /// Verify an Authorization header against the exact HTTP method and request
    /// target. The signature includes the request target's query string.
    pub fn authorize(
        &self,
        authorization: Option<&str>,
        method: &str,
        target: &str,
    ) -> Result<(), AuthError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AuthError::Unauthorized)?
            .as_secs() as i64;
        self.authorize_at(authorization, method, target, now)
    }

    fn authorize_at(
        &self,
        authorization: Option<&str>,
        method: &str,
        target: &str,
        now_epoch_secs: i64,
    ) -> Result<(), AuthError> {
        let authorization = authorization.ok_or(AuthError::Unauthorized)?;
        let encoded = authorization
            .strip_prefix(&format!("{AUTH_SCHEME} "))
            .ok_or(AuthError::Unauthorized)?;
        let (timestamp, encoded_signature) =
            encoded.split_once(':').ok_or(AuthError::Unauthorized)?;
        if encoded_signature.contains(':') {
            return Err(AuthError::Unauthorized);
        }
        let timestamp = timestamp
            .parse::<i64>()
            .map_err(|_| AuthError::Unauthorized)?;
        let max_age = self.max_age_secs as i64;
        if timestamp > now_epoch_secs.saturating_add(MAX_FUTURE_SKEW_SECS)
            || now_epoch_secs.saturating_sub(timestamp) > max_age
        {
            return Err(AuthError::Unauthorized);
        }
        let signature = URL_SAFE_NO_PAD
            .decode(encoded_signature)
            .map_err(|_| AuthError::Unauthorized)?;
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).map_err(|_| AuthError::Unauthorized)?;
        mac.update(canonical_request(timestamp, method, target).as_bytes());
        mac.verify_slice(&signature)
            .map_err(|_| AuthError::Unauthorized)
    }

    #[cfg(test)]
    pub fn sign(&self, timestamp: i64, method: &str, target: &str) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts any key length");
        mac.update(canonical_request(timestamp, method, target).as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        format!("{AUTH_SCHEME} {timestamp}:{signature}")
    }
}

fn canonical_request(timestamp: i64, method: &str, target: &str) -> String {
    format!("{CANONICAL_PREFIX}\n{timestamp}\n{method}\n{target}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authenticator() -> HmacAuthenticator {
        HmacAuthenticator::new(vec![7; MIN_SECRET_BYTES], 60).unwrap()
    }

    #[test]
    fn accepts_matching_signature() {
        let auth = authenticator();
        let header = auth.sign(1_700_000_000, "GET", "/?trace=1");
        assert_eq!(
            auth.authorize_at(Some(&header), "GET", "/?trace=1", 1_700_000_010),
            Ok(())
        );
    }

    #[test]
    fn rejects_wrong_target_and_expired_signature() {
        let auth = authenticator();
        let header = auth.sign(1_700_000_000, "GET", "/sessions");
        assert_eq!(
            auth.authorize_at(Some(&header), "GET", "/recordings", 1_700_000_010),
            Err(AuthError::Unauthorized)
        );
        assert_eq!(
            auth.authorize_at(Some(&header), "GET", "/sessions", 1_700_000_061),
            Err(AuthError::Unauthorized)
        );
    }

    #[test]
    fn rejects_short_secret() {
        assert!(HmacAuthenticator::new(vec![0; MIN_SECRET_BYTES - 1], 60).is_err());
    }
}
