//! Build Graph clients from configuration.

use crate::config::{AppCredentials, Config};
use anyhow::{Context, Result};
use banksia_graph::{AppAuth, Credential, GraphClient, GraphSettings};

pub fn client(cfg: &Config, creds: &AppCredentials, what: &str) -> Result<GraphClient> {
    let credential = if let (Some(cert), Some(key)) = (&creds.certificate_path, &creds.private_key)
    {
        let cert_pem = std::fs::read_to_string(cert)
            .with_context(|| format!("{what}: cannot read certificate {}", cert.display()))?;
        let key_pem = key.load(&format!("{what}.private_key"))?;
        Credential::certificate(&cert_pem, key_pem.expose()).with_context(|| what.to_string())?
    } else {
        let secret = creds
            .client_secret
            .as_ref()
            .context("no credential configured")?
            .load(&format!("{what}.client_secret"))?;
        Credential::secret(secret.expose())
    };
    let scope = format!("{}/.default", origin(&cfg.graph.base_url));
    let auth = AppAuth::new(
        &cfg.tenant.authority_host,
        &cfg.tenant.tenant_id,
        &creds.client_id,
        &scope,
        credential,
    );
    let settings = GraphSettings {
        base_url: cfg.graph.base_url.clone(),
        max_retries: cfg.graph.max_retries,
        concurrency: cfg.graph.concurrency,
        allow_http: cfg.graph.allow_insecure_http_for_tests,
        ..Default::default()
    };
    Ok(GraphClient::new(settings, auth)?)
}

fn origin(base: &str) -> String {
    let after = base.find("://").map(|i| i + 3).unwrap_or(0);
    match base[after..].find('/') {
        Some(i) => base[..after + i].to_string(),
        None => base.to_string(),
    }
}

/// Prefer header for mail requests: immutable ids survive folder moves.
pub fn mail_prefer() -> Vec<(&'static str, String)> {
    vec![(
        "Prefer",
        "IdType=\"ImmutableId\", odata.maxpagesize=100".to_string(),
    )]
}

#[cfg(test)]
mod tests {
    #[test]
    fn origin_of_graph_url() {
        assert_eq!(
            super::origin("https://graph.microsoft.com/v1.0"),
            "https://graph.microsoft.com"
        );
        assert_eq!(
            super::origin("http://127.0.0.1:9/v1.0"),
            "http://127.0.0.1:9"
        );
    }
}
