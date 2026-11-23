#![allow(dead_code)]

use banksia_core::{backup::BackupContext, crypto::MasterKey, repo::Repository, store::Store};
use banksia_fakegraph::FakeTenant;
use banksia_graph::{AppAuth, Credential, GraphClient, GraphSettings};
use std::time::Duration;

pub const BACKUP_APP: &str = "banksia-backup";
pub const RESTORE_APP: &str = "banksia-restore";

pub fn graph(t: &FakeTenant, client_id: &str) -> GraphClient {
    let auth = AppAuth::new(
        &t.authority(),
        "contoso.onmicrosoft.com",
        client_id,
        "https://graph.microsoft.com/.default",
        Credential::secret("test-secret"),
    );
    GraphClient::new(
        GraphSettings {
            base_url: t.graph_base(),
            max_retries: 5,
            concurrency: 4,
            base_backoff: Duration::from_millis(2),
            max_backoff: Duration::from_secs(2),
            allow_http: true,
        },
        auth,
    )
    .unwrap()
}

pub struct Env {
    pub tenant: FakeTenant,
    pub repo: Repository,
    pub store: Store,
    pub state: tempfile::TempDir,
}

impl Env {
    pub async fn new() -> Self {
        let tenant = FakeTenant::start().await;
        tenant.set_read_only_client(BACKUP_APP);
        let store = Store::memory();
        let repo = Repository::init(store.clone(), &MasterKey::generate())
            .await
            .unwrap();
        Self {
            tenant,
            repo,
            store,
            state: tempfile::tempdir().unwrap(),
        }
    }

    pub fn backup_ctx(&self) -> BackupContext {
        BackupContext {
            repo: self.repo.clone(),
            graph: graph(&self.tenant, BACKUP_APP),
            state_dir: self.state.path().to_path_buf(),
            parallel_jobs: 2,
        }
    }
}

pub fn mime(subject: &str, id: &str, body: &str) -> Vec<u8> {
    format!(
        "From: Megan <megan@contoso.onmicrosoft.com>\r\nTo: Adele <adele@contoso.onmicrosoft.com>\r\nSubject: {subject}\r\nMessage-ID: <{id}@contoso.onmicrosoft.com>\r\nDate: Thu, 20 Nov 2026 10:00:00 +1100\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
    )
    .into_bytes()
}

pub fn blob(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}
