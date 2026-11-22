mod common;

use banksia_core::{backup, config::Workload};
use banksia_fakegraph::Fault;
use common::*;

#[tokio::test]
async fn mailbox_initial_and_incremental() {
    let env = Env::new().await;
    let t = &env.tenant;
    t.set_page_size(2);
    let u = t.add_user("adele@contoso.onmicrosoft.com", "Adele");
    t.add_mail_folder(&u, "Inbox", "Projects", false);
    t.add_mail_folder(&u, "", "Hidden Stuff", true);
    let m1 = t.add_message(&u, "Inbox", &mime("Quarterly report", "m1", "numbers"));
    let m2 = t.add_message(&u, "Inbox/Projects", &mime("Banksia kickoff", "m2", "plan"));
    let m3 = t.add_message(&u, "Hidden Stuff", &mime("Hidden", "m3", "h"));
    let ctx = env.backup_ctx();

    let s1 = backup::backup_one(&ctx, &u.upn, Workload::Mail)
        .await
        .unwrap();
    let mail = s1.mail.as_ref().unwrap();
    assert!(!s1.partial);
    assert_eq!(mail.messages.len(), 3);
    assert!(mail.folder_by_path("Inbox/Projects").is_some());
    assert!(mail.folder_by_path("Hidden Stuff").unwrap().hidden);
    let e1 = mail.messages.iter().find(|m| m.id == m1).unwrap();
    assert_eq!(e1.subject.as_deref(), Some("Quarterly report"));
    assert_eq!(
        env.repo.read_content(&e1.content).await.unwrap(),
        mime("Quarterly report", "m1", "numbers")
    );
    assert_eq!(t.writes(), 0, "backup never writes to the tenant");

    // No changes: nothing new stored.
    let s2 = backup::backup_one(&ctx, &u.upn, Workload::Mail)
        .await
        .unwrap();
    assert_eq!(s2.parent.as_deref(), Some(s1.id.as_str()));
    assert_eq!(s2.stats.items, 3);
    assert_eq!(s2.stats.items_downloaded, 0);
    assert_eq!(s2.stats.chunks_written, 0);

    // Add, change, delete and move.
    t.add_message(&u, "Inbox", &mime("New one", "m4", "fresh"));
    t.touch_message(&m1);
    t.delete_message(&m2);
    t.move_message(&m3, &u, "Inbox");
    let s3 = backup::backup_one(&ctx, &u.upn, Workload::Mail)
        .await
        .unwrap();
    let mail3 = s3.mail.as_ref().unwrap();
    assert_eq!(mail3.messages.len(), 3);
    assert!(
        mail3.messages.iter().all(|m| m.id != m2),
        "deleted message absent from new snapshot"
    );
    let inbox = mail3.folder_by_path("Inbox").unwrap().id.clone();
    assert_eq!(
        mail3
            .messages
            .iter()
            .find(|m| m.id == m3)
            .unwrap()
            .folder_id,
        inbox
    );
    assert_eq!(
        s3.stats.chunks_written, 1,
        "only the new message's content is stored"
    );

    // The older snapshot still has the deleted message.
    let old = env
        .repo
        .load_snapshot(&s1.principal_tag, Workload::Mail, &s1.id)
        .await
        .unwrap();
    assert!(old.mail.unwrap().messages.iter().any(|m| m.id == m2));
}

#[tokio::test]
async fn mailbox_expired_delta_falls_back_without_redownloading() {
    let env = Env::new().await;
    let t = &env.tenant;
    let u = t.add_user("adele@contoso.onmicrosoft.com", "Adele");
    for i in 0..4 {
        t.add_message(&u, "Inbox", &mime(&format!("m{i}"), &format!("m{i}"), "x"));
    }
    let ctx = env.backup_ctx();
    backup::backup_one(&ctx, &u.upn, Workload::Mail)
        .await
        .unwrap();
    t.expire_delta_tokens();
    t.add_message(&u, "Inbox", &mime("after", "after", "y"));
    let s = backup::backup_one(&ctx, &u.upn, Workload::Mail)
        .await
        .unwrap();
    assert!(s.stats.full_resyncs >= 1);
    assert_eq!(s.stats.items, 5);
    assert_eq!(
        s.stats.items_downloaded, 1,
        "unchanged messages are matched by change key"
    );
}

#[tokio::test]
async fn throttling_during_backup_is_survived() {
    let env = Env::new().await;
    let t = &env.tenant;
    let u = t.add_user("adele@contoso.onmicrosoft.com", "Adele");
    t.add_message(&u, "Inbox", &mime("a", "a", "x"));
    t.add_file(&u, "a.txt", b"hello");
    t.inject(Fault {
        path_contains: "/$value".into(),
        status: 429,
        retry_after: Some(0),
        times: 2,
    });
    t.inject(Fault {
        path_contains: "/root/delta".into(),
        status: 503,
        retry_after: None,
        times: 2,
    });
    let results = backup::run(
        &env.backup_ctx(),
        &[banksia_core::config::UserSelection {
            id: u.upn.clone(),
            workloads: vec![Workload::Mail, Workload::Drive],
        }],
    )
    .await;
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.ok()), "{results:?}");
}

#[tokio::test]
async fn onedrive_initial_incremental_and_hash_check() {
    let env = Env::new().await;
    let t = &env.tenant;
    let u = t.add_user("adele@contoso.onmicrosoft.com", "Adele");
    t.add_drive_folder(&u, "Documents");
    t.add_drive_folder(&u, "Documents/Plans");
    let big = blob(7, banksia_core::repo::CHUNK_SIZE + 12345);
    let big_id = t.add_file(&u, "Documents/Plans/big.bin", &big);
    let small_id = t.add_file(&u, "Documents/notes.txt", b"remember the milk");
    let gone_id = t.add_file(&u, "old.txt", b"old");
    let ctx = env.backup_ctx();

    let s1 = backup::backup_one(&ctx, &u.upn, Workload::Drive)
        .await
        .unwrap();
    let d1 = s1.drive.as_ref().unwrap();
    assert!(!s1.partial, "{:?}", s1.errors);
    assert_eq!(s1.stats.items, 3);
    let e = d1.by_path("Documents/Plans/big.bin").unwrap();
    assert_eq!(e.content.as_ref().unwrap().chunks.len(), 2);
    assert_eq!(
        env.repo
            .read_content(e.content.as_ref().unwrap())
            .await
            .unwrap(),
        big
    );
    assert_eq!(t.token_leaks(), 0);

    let s2 = backup::backup_one(&ctx, &u.upn, Workload::Drive)
        .await
        .unwrap();
    assert_eq!(s2.stats.items_downloaded, 0);
    assert_eq!(s2.stats.chunks_written, 0);

    t.rename_item(&small_id, "notes-renamed.txt");
    t.delete_item(&gone_id);
    t.update_file(&big_id, &blob(9, 1000));
    let s3 = backup::backup_one(&ctx, &u.upn, Workload::Drive)
        .await
        .unwrap();
    let d3 = s3.drive.as_ref().unwrap();
    assert!(d3.by_path("Documents/notes-renamed.txt").is_some());
    assert!(d3.by_path("Documents/notes.txt").is_none());
    assert!(d3.by_path("old.txt").is_none());
    assert_eq!(
        s3.stats.items_downloaded, 1,
        "rename is metadata only; only the changed file downloads"
    );
    assert_eq!(
        env.repo
            .read_content(
                d3.by_path("Documents/Plans/big.bin")
                    .unwrap()
                    .content
                    .as_ref()
                    .unwrap()
            )
            .await
            .unwrap(),
        blob(9, 1000)
    );

    // Folder rename moves children paths.
    let docs = t.add_drive_folder(&u, "Archive");
    t.rename_item(&docs, "Archive 2026");
    t.add_file(&u, "Archive 2026/x.txt", b"x");
    let s4 = backup::backup_one(&ctx, &u.upn, Workload::Drive)
        .await
        .unwrap();
    assert!(
        s4.drive
            .as_ref()
            .unwrap()
            .by_path("Archive 2026/x.txt")
            .is_some()
    );
}

#[tokio::test]
async fn onedrive_hash_mismatch_is_reported_not_stored() {
    let env = Env::new().await;
    let t = &env.tenant;
    let u = t.add_user("adele@contoso.onmicrosoft.com", "Adele");
    let id = t.add_file(&u, "a.txt", b"genuine content");
    t.add_file(&u, "b.txt", b"fine");
    t.corrupt_download(&id);
    let s = backup::backup_one(&env.backup_ctx(), &u.upn, Workload::Drive)
        .await
        .unwrap();
    assert!(s.partial);
    assert_eq!(s.errors.len(), 1);
    assert!(s.errors[0].reason.contains("quickXorHash"));
    assert!(s.drive.as_ref().unwrap().by_path("a.txt").is_none());
    assert!(s.drive.as_ref().unwrap().by_path("b.txt").is_some());
}

#[tokio::test]
async fn interrupted_run_resumes_from_checkpoint() {
    let env = Env::new().await;
    let t = &env.tenant;
    let u = t.add_user("adele@contoso.onmicrosoft.com", "Adele");
    for i in 0..3 {
        t.add_file(&u, &format!("f{i}.bin"), &blob(i, 2000));
    }
    let last = t.add_file(&u, "z-last.bin", &blob(99, 2000));
    // The last download fails every retry, so the run ends with an error...
    t.inject(Fault {
        path_contains: format!("/items/{last}/content"),
        status: 503,
        retry_after: None,
        times: 6,
    });
    let ctx = env.backup_ctx();
    let s1 = backup::backup_one(&ctx, &u.upn, Workload::Drive)
        .await
        .unwrap();
    assert!(s1.partial);
    assert_eq!(s1.stats.items_downloaded, 3);

    // ...and the next run only downloads the file that failed.
    let s2 = backup::backup_one(&ctx, &u.upn, Workload::Drive)
        .await
        .unwrap();
    assert!(!s2.partial, "{:?}", s2.errors);
    assert_eq!(s2.stats.items_downloaded, 1);
}

#[tokio::test]
async fn object_names_do_not_reveal_users() {
    let env = Env::new().await;
    let t = &env.tenant;
    let u = t.add_user("adele@contoso.onmicrosoft.com", "Adele");
    t.add_message(&u, "Inbox", &mime("Secret subject", "s", "x"));
    backup::backup_one(&env.backup_ctx(), &u.upn, Workload::Mail)
        .await
        .unwrap();
    for p in env.store.list("").await.unwrap() {
        assert!(!p.contains("adele") && !p.contains(&u.id), "{p}");
        let raw = env.store.get(&p).await.unwrap().unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(
            !text.contains("Secret subject") && !text.contains("adele"),
            "{p} leaks plaintext"
        );
    }
}

#[tokio::test]
async fn failed_message_is_retried_on_next_incremental() {
    let env = Env::new().await;
    let t = &env.tenant;
    let u = t.add_user("adele@contoso.onmicrosoft.com", "Adele");
    t.add_message(&u, "Inbox", &mime("ok", "ok", "x"));
    let bad = t.add_message(&u, "Inbox", &mime("flaky", "flaky", "y"));
    t.inject(Fault {
        path_contains: format!("/messages/{bad}/$value"),
        status: 503,
        retry_after: None,
        times: 6,
    });
    let ctx = env.backup_ctx();
    let s1 = backup::backup_one(&ctx, &u.upn, Workload::Mail)
        .await
        .unwrap();
    assert!(s1.partial);
    assert_eq!(s1.mail.as_ref().unwrap().messages.len(), 1);
    let s2 = backup::backup_one(&ctx, &u.upn, Workload::Mail)
        .await
        .unwrap();
    assert!(!s2.partial);
    assert_eq!(s2.mail.as_ref().unwrap().messages.len(), 2);
    assert_eq!(s2.stats.items_downloaded, 1);
}
