//! End-to-end: the real `banksia` binary against the fake tenant.

use banksia_fakegraph::FakeTenant;
use std::{path::Path, process::Command};

pub struct Setup {
    pub dir: tempfile::TempDir,
    pub tenant: FakeTenant,
}

impl Setup {
    pub async fn new(extra: &str) -> Self {
        let tenant = FakeTenant::start().await;
        tenant.set_read_only_client("backup-app");
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let cfg = format!(
            r#"
[tenant]
tenant_id = "contoso.onmicrosoft.com"
authority_host = "{authority}"

[graph]
base_url = "{graph}"
allow_insecure_http_for_tests = true
max_retries = 2

[storage]
kind = "local"
path = "{repo}"

[key]
master_key = {{ file = "{key}" }}

[state]
dir = "{state}"

[backup.auth]
client_id = "backup-app"
client_secret = {{ env = "BANKSIA_TEST_BACKUP_SECRET" }}

[[backup.users]]
id = "adele@contoso.onmicrosoft.com"
{extra}
"#,
            authority = tenant.authority(),
            graph = tenant.graph_base(),
            repo = d.join("repo").display(),
            key = d.join("master.key").display(),
            state = d.join("state").display(),
        );
        std::fs::write(d.join("banksia.toml"), cfg).unwrap();
        Self { dir, tenant }
    }

    pub fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_banksia"))
            .arg("--config")
            .arg(self.dir.path().join("banksia.toml"))
            .args(args)
            .current_dir(self.dir.path())
            .env("BANKSIA_TEST_BACKUP_SECRET", "backup-secret-value")
            .env("BANKSIA_TEST_RESTORE_SECRET", "restore-secret-value")
            .env("BANKSIA_LOG", "warn")
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    pub fn path(&self, p: &str) -> std::path::PathBuf {
        self.dir.path().join(p)
    }
}

fn no_secrets(s: &str) {
    assert!(
        !s.contains("backup-secret-value") && !s.contains("restore-secret-value"),
        "secret leaked: {s}"
    );
    assert!(!s.contains("banksia-key-v1:"), "key leaked: {s}");
}

pub fn init(s: &Setup) {
    let key = s.path("master.key");
    let (c, out, err) = s.run(&["key", "generate", "--out", key.to_str().unwrap()]);
    assert_eq!(c, 0, "{err}");
    no_secrets(&out);
    let (c, _, err) = s.run(&["repo", "init"]);
    assert_eq!(c, 0, "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn backup_and_list_snapshots() {
    let s = Setup::new("").await;
    let u = s.tenant.add_user("adele@contoso.onmicrosoft.com", "Adele");
    s.tenant.add_message(
        &u,
        "Inbox",
        b"Subject: Hello\r\nMessage-ID: <h@x>\r\n\r\nbody",
    );
    s.tenant.add_file(&u, "Documents.txt", b"doc");
    init(&s);

    let (c, out, err) = s.run(&["backup", "run"]);
    assert_eq!(c, 0, "{out}{err}");
    assert!(
        out.contains("OK    adele@contoso.onmicrosoft.com mail"),
        "{out}"
    );
    assert!(
        out.contains("OK    adele@contoso.onmicrosoft.com drive"),
        "{out}"
    );
    no_secrets(&out);
    no_secrets(&err);

    let (c, out, _) = s.run(&["snapshots", "list"]);
    assert_eq!(c, 0);
    assert_eq!(out.lines().count(), 2, "{out}");

    let (c, out, _) = s.run(&[
        "snapshots",
        "show",
        "--user",
        "adele@contoso.onmicrosoft.com",
        "--workload",
        "mail",
        "--items",
    ]);
    assert_eq!(c, 0);
    assert!(out.contains("Inbox") && out.contains("Hello"), "{out}");

    // Nothing in the repository directory is plaintext.
    let mut stack = vec![s.path("repo")];
    while let Some(p) = stack.pop() {
        for e in std::fs::read_dir(&p).unwrap() {
            let e = e.unwrap().path();
            if e.is_dir() {
                stack.push(e);
            } else if e.file_name().unwrap() != "banksia.repo.json" {
                let raw = std::fs::read(&e).unwrap();
                let text = String::from_utf8_lossy(&raw);
                assert!(
                    !text.contains("Hello") && !text.contains("adele"),
                    "{}",
                    e.display()
                );
            }
        }
    }
    assert!(
        !Path::new(&s.path("state/checkpoints"))
            .read_dir()
            .unwrap()
            .any(|_| true),
        "checkpoints cleared"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn inline_secret_is_refused_without_echo() {
    let s = Setup::new("").await;
    let cfg = std::fs::read_to_string(s.path("banksia.toml"))
        .unwrap()
        .replace(
            r#"client_secret = { env = "BANKSIA_TEST_BACKUP_SECRET" }"#,
            r#"client_secret = "pasted-secret-123""#,
        );
    std::fs::write(s.path("banksia.toml"), cfg).unwrap();
    let (c, out, err) = s.run(&["repo", "info"]);
    assert_eq!(c, 1);
    assert!(err.contains("inline secrets are not allowed"), "{err}");
    assert!(!err.contains("pasted-secret-123") && !out.contains("pasted-secret-123"));
}

#[tokio::test(flavor = "multi_thread")]
async fn backup_failure_exits_non_zero() {
    let s = Setup::new("").await;
    // No such user in the tenant.
    init(&s);
    let (c, out, _) = s.run(&["backup", "run"]);
    assert_eq!(c, 2, "{out}");
    assert!(out.contains("FAIL"), "{out}");
}

const RESTORE_CFG: &str = r#"
[restore]
enabled = true
allowed_targets = ["restore-test@contoso.onmicrosoft.com"]

[restore.auth]
client_id = "restore-app"
client_secret = { env = "BANKSIA_TEST_RESTORE_SECRET" }
"#;

#[tokio::test(flavor = "multi_thread")]
async fn restore_plan_dry_run_and_apply() {
    let s = Setup::new(RESTORE_CFG).await;
    let u = s.tenant.add_user("adele@contoso.onmicrosoft.com", "Adele");
    let t = s
        .tenant
        .add_user("restore-test@contoso.onmicrosoft.com", "Restore Test");
    s.tenant.add_drive_folder(&u, "Documents");
    s.tenant.add_file(&u, "Documents/plan.txt", b"the plan");
    init(&s);
    assert_eq!(s.run(&["backup", "run", "--workload", "drive"]).0, 0);

    let plan = s.path("plan.json");
    let (c, out, err) = s.run(&[
        "restore",
        "plan",
        "--user",
        &u.upn,
        "--workload",
        "drive",
        "--path",
        "Documents",
        "--target",
        &t.upn,
        "--reason",
        "ticket 42",
        "--out",
        plan.to_str().unwrap(),
    ]);
    assert_eq!(c, 0, "{err}");
    let hash = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("hash"))
        .unwrap()
        .trim()
        .to_string();
    let writes = s.tenant.writes();

    // Wrong approval refused.
    let (c, _, err) = s.run(&[
        "restore",
        "apply",
        "--plan",
        plan.to_str().unwrap(),
        "--approve",
        "000000000000",
    ]);
    assert_eq!(c, 1);
    assert!(err.contains("does not match"), "{err}");

    let (c, out, err) = s.run(&[
        "restore",
        "apply",
        "--plan",
        plan.to_str().unwrap(),
        "--approve",
        &hash[..12],
        "--dry-run",
    ]);
    assert_eq!(c, 0, "{out}{err}");
    assert!(out.contains("dry run: 4 would be created"), "{out}");
    assert_eq!(s.tenant.writes(), writes);

    let (c, out, err) = s.run(&[
        "restore",
        "apply",
        "--plan",
        plan.to_str().unwrap(),
        "--approve",
        &hash,
    ]);
    assert_eq!(c, 0, "{out}{err}");
    let restored = s.tenant.file_count(&t);
    assert_eq!(restored, 1);
    no_secrets(&out);
    no_secrets(&err);

    let (c, out, _) = s.run(&["audit", "list"]);
    assert_eq!(c, 0);
    for k in [
        "restore.plan_created",
        "restore.dry_run",
        "restore.apply_started",
        "restore.apply_finished",
    ] {
        assert!(out.contains(k), "{k} missing: {out}");
    }

    // Restoring into a user not on the allowlist is refused at apply.
    let plan2 = s.path("plan2.json");
    let (c, _, err) = s.run(&[
        "restore",
        "plan",
        "--user",
        &u.upn,
        "--workload",
        "drive",
        "--all",
        "--out",
        plan2.to_str().unwrap(),
    ]);
    assert_eq!(c, 0);
    assert!(err.contains("not on restore.allowed_targets"), "{err}");
    let text = std::fs::read(&plan2).unwrap();
    let h = banksia_core_hash(&text);
    let (c, _, err) = s.run(&[
        "restore",
        "apply",
        "--plan",
        plan2.to_str().unwrap(),
        "--approve",
        &h,
    ]);
    assert_eq!(c, 1);
    assert!(err.contains("allowlist"), "{err}");
}

fn banksia_core_hash(b: &[u8]) -> String {
    banksia_core::restore::plan_hash(b)
}
