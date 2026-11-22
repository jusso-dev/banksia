mod common;

use banksia_core::{
    backup,
    config::{RestoreTestConfig, Workload},
    evidence::{self, Verdict},
    restore_test::{RestoreTestContext, run},
};
use common::*;

const ATTACH_MIME: &str = "From: Megan <megan@contoso.onmicrosoft.com>\r\nSubject: Board pack\r\nMessage-ID: <board@contoso.onmicrosoft.com>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=xyz\r\n\r\n--xyz\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n--xyz\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=board.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0xLjQKJcfsj6IKMSAwIG9iago8PD4+CmVuZG9iagp0cmFpbGVyCjw8Pj4KJSVFT0YK\r\n--xyz--\r\n";

struct T {
    env: Env,
    test_graph: banksia_graph::GraphClient,
    policy: RestoreTestConfig,
}

async fn setup(max_age: u64) -> T {
    let env = Env::new().await;
    let t = &env.tenant;
    let u = t.add_user("adele@contoso.onmicrosoft.com", "Adele");
    t.add_user("restore-test@contoso.onmicrosoft.com", "Restore Test");
    for i in 0..6 {
        t.add_message(
            &u,
            "Inbox",
            &mime(&format!("Status {i}"), &format!("s{i}"), "fine"),
        );
    }
    t.add_message(&u, "Inbox", ATTACH_MIME.as_bytes());
    t.add_drive_folder(&u, "Documents");
    for i in 0..4 {
        t.add_file(
            &u,
            &format!("Documents/f{i}.bin"),
            &blob(i, 3000 + i as usize),
        );
    }
    t.add_file(
        &u,
        "Documents/big.bin",
        &blob(77, banksia_core::repo::CHUNK_SIZE + 5),
    );
    let ctx = env.backup_ctx();
    backup::backup_one(&ctx, &u.upn, Workload::Mail)
        .await
        .unwrap();
    backup::backup_one(&ctx, &u.upn, Workload::Drive)
        .await
        .unwrap();
    let test_graph = graph(t, "banksia-restore-test");
    T {
        env,
        test_graph,
        policy: RestoreTestConfig {
            mail_target: Some("restore-test@contoso.onmicrosoft.com".into()),
            drive_target: Some("restore-test@contoso.onmicrosoft.com".into()),
            sample_size: 10,
            max_snapshot_age_hours: max_age,
            evidence_dir: "unused".into(),
            auth: None,
        },
    }
}

fn ctx<'a>(t: &'a T, seed: Option<&str>) -> RestoreTestContext<'a> {
    RestoreTestContext {
        repo: &t.env.repo,
        test_graph: &t.test_graph,
        policy: &t.policy,
        state_dir: t.env.state.path().to_path_buf(),
        operator: "scheduler".into(),
        repository_label: "memory".into(),
        seed: seed.map(str::to_string),
        users: vec![],
    }
}

#[tokio::test]
async fn passing_run_writes_verifiable_evidence() {
    let t = setup(26).await;
    let pack = run(&ctx(&t, Some("00ff"))).await.unwrap();
    assert_eq!(pack.result, Verdict::Pass, "{:#?}", pack.checks);
    assert_eq!(pack.summary.checks, 2);
    assert_eq!(pack.summary.items_tested, 12);
    assert_eq!(pack.summary.items_failed, 0);
    let mail = pack
        .checks
        .iter()
        .find(|c| c.workload == Workload::Mail)
        .unwrap();
    assert!(mail.items.iter().any(|i| {
        i.checks_passed
            .iter()
            .any(|c| c == "attachments_sha256 (1)")
    }));
    let drive = pack
        .checks
        .iter()
        .find(|c| c.workload == Workload::Drive)
        .unwrap();
    assert!(
        drive
            .items
            .iter()
            .all(|i| i.checks_passed.contains(&"sha256_content".to_string()))
    );

    assert_eq!(t.env.tenant.token_leaks(), 0);

    let dir = tempfile::tempdir().unwrap();
    let out = evidence::write(&pack, &t.env.repo, dir.path())
        .await
        .unwrap();
    let back = evidence::verify(&out, t.env.repo.keys()).unwrap();
    assert_eq!(back, pack);

    // The pack holds no subjects, bodies, names or Graph ids.
    for name in ["evidence.json", "report.md"] {
        let text = std::fs::read_to_string(out.join(name)).unwrap();
        for leak in [
            "Status 0",
            "Board pack",
            "See attached",
            "adele",
            "f0.bin",
            "AAMk",
        ] {
            assert!(!text.contains(leak), "{name} leaks {leak}");
        }
    }
    let md = std::fs::read_to_string(out.join("report.md")).unwrap();
    assert!(md.contains("**PASS**") && md.contains("Essential Eight"));

    // Copy stored in the repository.
    let stored = t
        .env
        .repo
        .list(&format!("evidence/{}/", pack.run_id))
        .await
        .unwrap();
    assert_eq!(stored.len(), 4);

    // Same seed, same sample.
    let pack2 = run(&ctx(&t, Some("00ff"))).await.unwrap();
    let refs = |p: &evidence::EvidencePack| {
        p.checks
            .iter()
            .flat_map(|c| c.items.iter().map(|i| i.item_ref.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(refs(&pack), refs(&pack2));

    let kinds: Vec<String> = t
        .env
        .repo
        .audit_events()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&"restore_test.started".into()));
    assert!(kinds.contains(&"restore_test.apply_finished".into()));
    assert!(kinds.contains(&"restore_test.finished".into()));
}

#[tokio::test]
async fn tampered_evidence_is_detected() {
    let t = setup(26).await;
    let pack = run(&ctx(&t, None)).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = evidence::write(&pack, &t.env.repo, dir.path())
        .await
        .unwrap();
    let p = out.join("evidence.json");
    // Change one field: the file hash no longer matches the manifest.
    let text = std::fs::read_to_string(&p)
        .unwrap()
        .replace("\"scheduler\"", "\"someone else\"");
    std::fs::write(&p, text).unwrap();
    assert!(evidence::verify(&out, t.env.repo.keys()).is_err());

    // Re-signing needs the key: a forged manifest fails the HMAC.
    let m = out.join("manifest.hmac");
    std::fs::write(&m, "00".repeat(32)).unwrap();
    assert!(
        evidence::verify(&out, t.env.repo.keys())
            .unwrap_err()
            .to_string()
            .contains("signature")
    );
}

#[tokio::test]
async fn corrupted_restore_fails_with_reason_and_stale_snapshot_fails() {
    let t = setup(26).await;
    t.env.tenant.corrupt_next_upload();
    let pack = run(&ctx(&t, Some("01"))).await.unwrap();
    assert_eq!(pack.result, Verdict::Fail);
    assert_eq!(pack.summary.items_failed, 1, "{:#?}", pack.checks);
    let drive = pack
        .checks
        .iter()
        .find(|c| c.workload == Workload::Drive)
        .unwrap();
    assert_eq!(drive.result, Verdict::Fail);
    let failed = drive
        .items
        .iter()
        .find(|i| i.result == Verdict::Fail)
        .unwrap();
    assert!(
        failed.failure.as_ref().unwrap().contains("quickXorHash"),
        "{failed:?}"
    );
    assert!(pack.report_markdown().contains("## Item failures"));

    // A policy of zero hours makes every snapshot stale.
    let stale = setup(0).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let pack = run(&ctx(&stale, None)).await.unwrap();
    assert_eq!(pack.result, Verdict::Fail);
    assert!(
        pack.checks
            .iter()
            .all(|c| c.reasons.iter().any(|r| r.contains("hours old")))
    );
}

#[tokio::test]
async fn unconfigured_target_is_skipped() {
    let mut t = setup(26).await;
    t.policy.drive_target = None;
    let pack = run(&ctx(&t, None)).await.unwrap();
    let drive = pack
        .checks
        .iter()
        .find(|c| c.workload == Workload::Drive)
        .unwrap();
    assert_eq!(drive.result, Verdict::Skipped);
    assert_eq!(pack.result, Verdict::Pass, "mail still verified");
}
