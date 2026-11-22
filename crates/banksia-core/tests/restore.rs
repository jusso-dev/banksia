mod common;

use banksia_core::{
    backup,
    config::Workload,
    restore::{
        self, PlanRequest, Selection,
        apply::{ApplyContext, Outcome, apply},
    },
    snapshot::Snapshot,
};
use banksia_fakegraph::FakeUser;
use common::*;

struct R {
    env: Env,
    u: FakeUser,
    target: FakeUser,
    restore_graph: banksia_graph::GraphClient,
}

async fn setup() -> R {
    let env = Env::new().await;
    let t = &env.tenant;
    let u = t.add_user("adele@contoso.onmicrosoft.com", "Adele");
    let target = t.add_user("restore-test@contoso.onmicrosoft.com", "Restore Test");
    t.add_mail_folder(&u, "Inbox", "Projects", false);
    t.add_message(&u, "Inbox", &mime("Quarterly report", "m1", "numbers"));
    t.add_message(&u, "Inbox/Projects", &mime("Banksia kickoff", "m2", "plan"));
    t.add_message(
        &u,
        "Inbox/Projects",
        &mime("Banksia follow up", "m3", "more"),
    );
    t.add_drive_folder(&u, "Documents");
    t.add_drive_folder(&u, "Documents/Plans");
    t.add_file(&u, "Documents/Plans/plan.txt", b"the plan");
    t.add_file(
        &u,
        "Documents/Plans/big.bin",
        &blob(3, banksia_core::repo::CHUNK_SIZE + 777),
    );
    t.add_file(&u, "Documents/readme.md", b"# readme");
    let restore_graph = graph(t, RESTORE_APP);
    R {
        env,
        u,
        target,
        restore_graph,
    }
}

async fn snap(r: &R, w: Workload) -> Snapshot {
    backup::backup_one(&r.env.backup_ctx(), &r.u.upn, w)
        .await
        .unwrap()
}

fn ctx<'a>(r: &'a R, enabled: bool, allowed: &[&str]) -> ApplyContext<'a> {
    ApplyContext {
        repo: &r.env.repo,
        graph: &r.restore_graph,
        state_dir: r.env.state.path().to_path_buf(),
        enabled,
        allowed_targets: allowed.iter().map(|s| s.to_string()).collect(),
        operator: "test-operator".into(),
        audit_kind: "restore",
    }
}

fn plan_for(
    r: &R,
    s: &Snapshot,
    sel: Selection,
    target: &FakeUser,
    in_place: bool,
) -> (restore::RestorePlan, Vec<u8>, String) {
    let plan = restore::build_plan(
        &r.env.repo,
        PlanRequest {
            snapshot: s,
            selection: sel,
            target_user: target.upn.clone(),
            target_object_id: target.id.clone(),
            in_place,
            created_by: "test-operator".into(),
            purpose: "test".into(),
        },
    )
    .unwrap();
    let bytes = restore::encode_plan(&plan).unwrap();
    let hash = restore::plan_hash(&bytes);
    (plan, bytes, hash)
}

#[tokio::test]
async fn mail_folder_restore_to_alternate_location_and_rerun() {
    let r = setup().await;
    let s = snap(&r, Workload::Mail).await;
    let (plan, bytes, hash) = plan_for(
        &r,
        &s,
        Selection::MailFolder("Inbox/Projects".into()),
        &r.target,
        false,
    );
    assert_eq!(plan.summary.messages, 2);
    let (plan2, hash2) = restore::verify_approval(&bytes, &hash[..12]).unwrap();
    assert_eq!(plan2, plan);
    assert_eq!(hash2, hash);

    let c = ctx(&r, true, &[&r.target.upn]);
    let writes_before = r.env.tenant.writes();
    let rep = apply(&c, &plan, &hash, false).await.unwrap();
    assert!(rep.ok(), "{:#?}", rep.results);
    let dest = format!("Banksia Restore/{}/Inbox/Projects", plan.plan_id);
    let got = r.env.tenant.messages_in(&r.target, &dest);
    assert_eq!(got.len(), 2);
    assert_eq!(
        got.iter()
            .find(|m| m.subject == "Banksia kickoff")
            .unwrap()
            .mime,
        mime("Banksia kickoff", "m2", "plan")
    );
    assert_eq!(
        r.env.tenant.message_count(&r.u),
        3,
        "source mailbox untouched"
    );
    let writes = r.env.tenant.writes() - writes_before;

    // Rerun: nothing new is written (journal).
    let rep2 = apply(&c, &plan, &hash, false).await.unwrap();
    assert_eq!(rep2.created, 0);
    assert_eq!(r.env.tenant.writes() - writes_before, writes);
    assert_eq!(r.env.tenant.messages_in(&r.target, &dest).len(), 2);

    // Rerun with the journal lost: tenant lookups prevent duplicates.
    std::fs::remove_dir_all(r.env.state.path().join("restore-journal")).unwrap();
    let rep3 = apply(&c, &plan, &hash, false).await.unwrap();
    assert_eq!(rep3.created, 0, "{:#?}", rep3.results);
    assert_eq!(r.env.tenant.messages_in(&r.target, &dest).len(), 2);

    let kinds: Vec<String> = r
        .env
        .repo
        .audit_events()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "restore.apply_started")
            .count(),
        3
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "restore.apply_finished")
            .count(),
        3
    );
}

#[tokio::test]
async fn single_message_restore_in_place() {
    let r = setup().await;
    let s = snap(&r, Workload::Mail).await;
    let mail = s.mail.as_ref().unwrap();
    let m = mail
        .messages
        .iter()
        .find(|m| m.subject.as_deref() == Some("Banksia kickoff"))
        .unwrap();
    // The user deletes the message after the backup.
    r.env.tenant.delete_message(&m.id);
    assert_eq!(r.env.tenant.messages_in(&r.u, "Inbox/Projects").len(), 1);

    let (plan, _, hash) = plan_for(
        &r,
        &s,
        Selection::MailMessages(vec![m.id.clone()]),
        &r.u,
        true,
    );
    let c = ctx(&r, true, &[&r.u.upn]);
    let rep = apply(&c, &plan, &hash, false).await.unwrap();
    assert!(rep.ok(), "{:#?}", rep.results);
    let back = r.env.tenant.messages_in(&r.u, "Inbox/Projects");
    assert_eq!(back.len(), 2);
    // The folders already existed, so only the message was created.
    assert_eq!(rep.created, 1);
}

#[tokio::test]
async fn dry_run_writes_nothing_and_reports() {
    let r = setup().await;
    let s = snap(&r, Workload::Drive).await;
    let (plan, _, hash) = plan_for(&r, &s, Selection::All, &r.target, false);
    let writes = r.env.tenant.writes();
    // Dry run works even with restores disabled.
    let rep = apply(&ctx(&r, false, &[&r.target.upn]), &plan, &hash, true)
        .await
        .unwrap();
    assert!(rep.dry_run && rep.ok());
    assert_eq!(rep.created as usize, plan.actions.len());
    assert!(
        rep.results
            .iter()
            .all(|x| x.outcome == Outcome::WouldCreate)
    );
    assert_eq!(r.env.tenant.writes(), writes, "dry run must not write");
}

#[tokio::test]
async fn refusals_disabled_allowlist_and_tampered_plan() {
    let r = setup().await;
    let s = snap(&r, Workload::Mail).await;
    let (plan, bytes, hash) = plan_for(&r, &s, Selection::All, &r.target, false);

    let e = apply(&ctx(&r, false, &[&r.target.upn]), &plan, &hash, false)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("disabled"), "{e}");

    let e = apply(
        &ctx(&r, true, &["someone-else@contoso.onmicrosoft.com"]),
        &plan,
        &hash,
        false,
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("allowlist"), "{e}");

    // Any edit to the plan breaks the approval.
    let tampered = String::from_utf8(bytes.clone())
        .unwrap()
        .replace(&r.target.upn, &r.u.upn)
        .into_bytes();
    let e = restore::verify_approval(&tampered, &hash).unwrap_err();
    assert!(e.to_string().contains("does not match"), "{e}");
    assert!(
        restore::verify_approval(&bytes, &hash[..8]).is_err(),
        "short approvals are refused"
    );

    // A target UPN that now resolves to a different account is refused.
    let mut moved = plan.clone();
    moved.target.object_id = r.u.id.clone();
    let e = apply(&ctx(&r, true, &[&r.target.upn]), &moved, &hash, false)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("object id"), "{e}");

    assert_eq!(r.env.tenant.message_count(&r.target), 0);
}

#[tokio::test]
async fn drive_restore_with_upload_session_and_no_overwrite() {
    let r = setup().await;
    let s = snap(&r, Workload::Drive).await;
    // After the backup, the user overwrites plan.txt.
    let plan_id = s
        .drive
        .as_ref()
        .unwrap()
        .by_path("Documents/Plans/plan.txt")
        .unwrap()
        .id
        .clone();
    r.env.tenant.update_file(&plan_id, b"ruined");

    let (plan, _, hash) = plan_for(
        &r,
        &s,
        Selection::DrivePath("Documents/Plans".into()),
        &r.u,
        true,
    );
    assert_eq!(plan.summary.files, 2);
    let c = ctx(&r, true, &[&r.u.upn]);
    let rep = apply(&c, &plan, &hash, false).await.unwrap();
    assert!(rep.ok(), "{:#?}", rep.results);
    let by = |o: Outcome| rep.results.iter().filter(|x| x.outcome == o).count();
    assert_eq!(by(Outcome::CreatedRenamed), 1, "{:#?}", rep.results);
    assert_eq!(
        by(Outcome::AlreadyPresent),
        3,
        "two folders and big.bin already match"
    );
    assert_eq!(
        r.env
            .tenant
            .file_at(&r.u, "Documents/Plans/plan.txt")
            .unwrap(),
        b"ruined",
        "never overwritten"
    );
    assert_eq!(
        r.env
            .tenant
            .file_at(&r.u, "Documents/Plans/plan 1.txt")
            .unwrap(),
        b"the plan"
    );

    // Journal lost, rerun: the renamed copy is found, nothing new created.
    std::fs::remove_dir_all(r.env.state.path().join("restore-journal")).unwrap();
    let rep2 = apply(&c, &plan, &hash, false).await.unwrap();
    assert_eq!(rep2.created, 0, "{:#?}", rep2.results);

    // Alternate location restore of everything, including the multi-chunk file.
    let (plan, _, hash) = plan_for(&r, &s, Selection::All, &r.target, false);
    let rep = apply(&ctx(&r, true, &[&r.target.upn]), &plan, &hash, false)
        .await
        .unwrap();
    assert!(rep.ok(), "{:#?}", rep.results);
    let root = format!("Banksia Restore/{}", plan.plan_id);
    assert_eq!(
        r.env
            .tenant
            .file_at(&r.target, &format!("{root}/Documents/Plans/big.bin"))
            .unwrap(),
        blob(3, banksia_core::repo::CHUNK_SIZE + 777)
    );
    assert_eq!(r.env.tenant.token_leaks(), 0);
}

#[tokio::test]
async fn interrupted_apply_resumes_without_duplicates() {
    let r = setup().await;
    let s = snap(&r, Workload::Mail).await;
    let (plan, _, hash) = plan_for(&r, &s, Selection::All, &r.target, false);
    let c = ctx(&r, true, &[&r.target.upn]);
    // The first import fails every retry (one try plus five retries).
    r.env.tenant.fail_message_imports(6);
    let rep = apply(&c, &plan, &hash, false).await.unwrap();
    assert_eq!(rep.failed, 1, "{:#?}", rep.results);
    assert_eq!(r.env.tenant.message_count(&r.target), 2);

    let rep2 = apply(&c, &plan, &hash, false).await.unwrap();
    assert!(rep2.ok(), "{:#?}", rep2.results);
    assert_eq!(
        rep2.created, 1,
        "only the failed message is created on rerun"
    );
    assert_eq!(r.env.tenant.message_count(&r.target), 3);
}

#[tokio::test]
async fn failed_upload_verification_is_reported() {
    let r = setup().await;
    let s = snap(&r, Workload::Drive).await;
    let (plan, _, hash) = plan_for(
        &r,
        &s,
        Selection::DrivePath("Documents/readme.md".into()),
        &r.target,
        false,
    );
    r.env.tenant.corrupt_next_upload();
    let rep = apply(&ctx(&r, true, &[&r.target.upn]), &plan, &hash, false)
        .await
        .unwrap();
    assert!(!rep.ok());
    let f = rep
        .results
        .iter()
        .find(|x| x.outcome == Outcome::Failed)
        .unwrap();
    assert!(f.detail.as_ref().unwrap().contains("quickXorHash"), "{f:?}");
}
