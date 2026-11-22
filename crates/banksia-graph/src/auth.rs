//! OAuth 2.0 client credentials for Entra ID app registrations.
//!
//! Certificates are preferred: Banksia signs a short-lived client assertion
//! (PS256, `x5t#S256` header) with a PKCS#8 RSA key. Client secrets are
//! supported for evaluation.

use crate::{error::GraphError, pem};
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fmt,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
/// Refresh tokens this long before they expire.
const REFRESH_MARGIN: Duration = Duration::from_secs(300);

pub enum Credential {
    Secret(Zeroizing<String>),
    Certificate {
        /// base64url SHA-256 thumbprint of the DER certificate.
        x5t_s256: String,
        key: aws_lc_rs::rsa::KeyPair,
    },
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Credential::Secret(_) => f.write_str("Credential::Secret([REDACTED])"),
            Credential::Certificate { x5t_s256, .. } => f
                .debug_struct("Credential::Certificate")
                .field("x5t_s256", x5t_s256)
                .finish_non_exhaustive(),
        }
    }
}

impl Credential {
    pub fn secret(value: &str) -> Self {
        Credential::Secret(Zeroizing::new(value.to_string()))
    }

    /// Build from a PEM certificate and a PEM PKCS#8 RSA private key.
    pub fn certificate(cert_pem: &str, key_pem: &str) -> Result<Self, GraphError> {
        let cert = pem::decode(cert_pem, "CERTIFICATE")
            .ok_or_else(|| GraphError::Auth("certificate is not a PEM CERTIFICATE".into()))?;
        let der = Zeroizing::new(pem::decode(key_pem, "PRIVATE KEY").ok_or_else(|| {
            GraphError::Auth("private key is not a PEM PKCS#8 PRIVATE KEY".into())
        })?);
        let key = aws_lc_rs::rsa::KeyPair::from_pkcs8(&der)
            .map_err(|_| GraphError::Auth("private key is not a valid RSA PKCS#8 key".into()))?;
        Ok(Credential::Certificate {
            x5t_s256: B64URL.encode(Sha256::digest(&cert)),
            key,
        })
    }
}

#[derive(Clone)]
struct CachedToken {
    value: Zeroizing<String>,
    refresh_at: Instant,
}

/// Acquires and caches app-only tokens for one app registration.
pub struct AppAuth {
    token_url: String,
    client_id: String,
    scope: String,
    credential: Credential,
    cache: Mutex<Option<CachedToken>>,
}

impl fmt::Debug for AppAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppAuth")
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field("scope", &self.scope)
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct TokenError {
    error: Option<String>,
    error_description: Option<String>,
}

impl AppAuth {
    /// `authority_host` is for example `https://login.microsoftonline.com`.
    /// `scope` is for example `https://graph.microsoft.com/.default`.
    pub fn new(
        authority_host: &str,
        tenant: &str,
        client_id: &str,
        scope: &str,
        credential: Credential,
    ) -> Self {
        Self {
            token_url: format!(
                "{}/{}/oauth2/v2.0/token",
                authority_host.trim_end_matches('/'),
                crate::seg(tenant)
            ),
            client_id: client_id.to_string(),
            scope: scope.to_string(),
            credential,
            cache: Mutex::new(None),
        }
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub async fn invalidate(&self) {
        *self.cache.lock().await = None;
    }

    pub async fn token(&self, http: &reqwest::Client) -> Result<Zeroizing<String>, GraphError> {
        let mut cache = self.cache.lock().await;
        if let Some(t) = cache.as_ref()
            && Instant::now() < t.refresh_at
        {
            return Ok(t.value.clone());
        }
        let fresh = self.request(http).await?;
        let value = fresh.value.clone();
        *cache = Some(fresh);
        Ok(value)
    }

    async fn request(&self, http: &reqwest::Client) -> Result<CachedToken, GraphError> {
        let mut form: Vec<(&str, String)> = vec![
            ("grant_type", "client_credentials".into()),
            ("client_id", self.client_id.clone()),
            ("scope", self.scope.clone()),
        ];
        let assertion;
        match &self.credential {
            Credential::Secret(s) => form.push(("client_secret", s.to_string())),
            Credential::Certificate { x5t_s256, key } => {
                assertion = Zeroizing::new(client_assertion(
                    &self.token_url,
                    &self.client_id,
                    x5t_s256,
                    key,
                )?);
                form.push((
                    "client_assertion_type",
                    "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".into(),
                ));
                form.push(("client_assertion", assertion.to_string()));
            }
        }
        let sent = http.post(&self.token_url).form(&form).send().await;
        // Scrub the form (it may hold the client secret) whether or not the send worked.
        for (_, v) in form.iter_mut() {
            zeroize::Zeroize::zeroize(v);
        }
        let resp = sent
            .map_err(|e| GraphError::Auth(format!("token request failed: {}", e.without_url())))?;
        let status = resp.status();
        let body = resp.bytes().await.map_err(|e| {
            GraphError::Auth(format!("token response unreadable: {}", e.without_url()))
        })?;
        if !status.is_success() {
            let detail = serde_json::from_slice::<TokenError>(&body)
                .ok()
                .map(|e| {
                    let desc = e.error_description.unwrap_or_default();
                    // Entra descriptions are multi-line with trace ids; keep the first line.
                    format!(
                        "{}: {}",
                        e.error.unwrap_or_else(|| "error".into()),
                        desc.lines().next().unwrap_or("")
                    )
                })
                .unwrap_or_else(|| "no detail".into());
            return Err(GraphError::Auth(format!(
                "token endpoint returned {status}: {detail}"
            )));
        }
        let t: TokenResponse = serde_json::from_slice(&body)
            .map_err(|_| GraphError::Auth("token response is not valid JSON".into()))?;
        let lifetime = Duration::from_secs(t.expires_in.unwrap_or(3600));
        Ok(CachedToken {
            value: Zeroizing::new(t.access_token),
            refresh_at: Instant::now() + lifetime.saturating_sub(REFRESH_MARGIN),
        })
    }
}

fn client_assertion(
    aud: &str,
    client_id: &str,
    x5t_s256: &str,
    key: &aws_lc_rs::rsa::KeyPair,
) -> Result<String, GraphError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut jti = [0u8; 16];
    getrandom::fill(&mut jti)
        .map_err(|_| GraphError::Auth("random number generator failed".into()))?;
    let header = serde_json::json!({ "alg": "PS256", "typ": "JWT", "x5t#S256": x5t_s256 });
    let claims = serde_json::json!({
        "aud": aud,
        "iss": client_id,
        "sub": client_id,
        "jti": hex::encode(jti),
        "nbf": now.saturating_sub(60),
        "iat": now,
        "exp": now + 600,
    });
    let signing_input = format!(
        "{}.{}",
        B64URL.encode(header.to_string()),
        B64URL.encode(claims.to_string())
    );
    let mut sig = vec![0u8; key.public_modulus_len()];
    key.sign(
        &aws_lc_rs::signature::RSA_PSS_SHA256,
        &aws_lc_rs::rand::SystemRandom::new(),
        signing_input.as_bytes(),
        &mut sig,
    )
    .map_err(|_| GraphError::Auth("cannot sign client assertion".into()))?;
    Ok(format!("{signing_input}.{}", B64URL.encode(sig)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::signature::KeyPair as _;

    #[test]
    fn assertion_shape_and_signature() {
        let key = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
        let pkcs8 = AsDer::<aws_lc_rs::encoding::Pkcs8V1Der>::as_der(&key).unwrap();
        let key_pem = pem::encode(pkcs8.as_ref(), "PRIVATE KEY");
        // Any DER blob works as a stand-in certificate for the thumbprint.
        let cert_pem = pem::encode(b"not really a certificate", "CERTIFICATE");
        let cred = Credential::certificate(&cert_pem, &key_pem).unwrap();
        let Credential::Certificate { x5t_s256, key } = &cred else {
            panic!()
        };
        let jwt = client_assertion(
            "https://login.example/t/oauth2/v2.0/token",
            "app",
            x5t_s256,
            key,
        )
        .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value =
            serde_json::from_slice(&B64URL.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "PS256");
        assert_eq!(
            header["x5t#S256"],
            B64URL.encode(Sha256::digest(b"not really a certificate"))
        );
        let claims: serde_json::Value =
            serde_json::from_slice(&B64URL.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://login.example/t/oauth2/v2.0/token");
        assert_eq!(claims["iss"], "app");
        assert!(claims["exp"].as_u64().unwrap() > claims["iat"].as_u64().unwrap());

        let public = key.public_key().as_ref().to_vec();
        let verifier = aws_lc_rs::signature::UnparsedPublicKey::new(
            &aws_lc_rs::signature::RSA_PSS_2048_8192_SHA256,
            public,
        );
        verifier
            .verify(
                format!("{}.{}", parts[0], parts[1]).as_bytes(),
                &B64URL.decode(parts[2]).unwrap(),
            )
            .unwrap();
        assert!(!format!("{cred:?}").contains("PRIVATE"));
    }

    #[test]
    fn secret_debug_redacted() {
        assert!(!format!("{:?}", Credential::secret("s3cret")).contains("s3cret"));
    }
}
