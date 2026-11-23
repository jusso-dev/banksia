//! Append-only audit events, stored encrypted in the repository.

use crate::repo::Repository;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditEvent {
    pub id: String,
    pub time: String,
    pub kind: String,
    pub operator: String,
    pub host: String,
    pub plan_id: Option<String>,
    pub plan_hash: Option<String>,
    pub details: Value,
}

pub fn operator() -> String {
    std::env::var("BANKSIA_OPERATOR")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

pub fn host() -> String {
    std::env::var("BANKSIA_HOST")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

impl Repository {
    pub async fn audit(
        &self,
        kind: &str,
        operator: &str,
        plan: Option<(&str, &str)>,
        details: Value,
    ) -> Result<AuditEvent> {
        let now = crate::time::now();
        let id = crate::time::sortable_id();
        let ev = AuditEvent {
            id: id.clone(),
            time: crate::time::rfc3339(now),
            kind: kind.to_string(),
            operator: operator.to_string(),
            host: host(),
            plan_id: plan.map(|p| p.0.to_string()),
            plan_hash: plan.map(|p| p.1.to_string()),
            details,
        };
        let path = format!("audit/{:04}/{:02}/{id}", now.year(), u8::from(now.month()));
        self.put_doc(&path, &ev).await?;
        Ok(ev)
    }

    pub async fn audit_events(&self) -> Result<Vec<AuditEvent>> {
        let mut out = Vec::new();
        for p in self.list("audit/").await? {
            if let Some(ev) = self.get_doc::<AuditEvent>(&p).await? {
                out.push(ev);
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }
}
