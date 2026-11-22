use banksia_fakegraph::{FakeTenant, Fault};
use banksia_graph::{
    AppAuth, Credential, GraphClient, GraphError, GraphSettings,
    types::{DriveItem, MailFolder, Message, User},
};
use std::time::{Duration, Instant};

fn client(t: &FakeTenant, client_id: &str, secret: &str) -> GraphClient {
    let auth = AppAuth::new(
        &t.authority(),
        "contoso.onmicrosoft.com",
        client_id,
        "https://graph.microsoft.com/.default",
        Credential::secret(secret),
    );
    GraphClient::new(
        GraphSettings {
            base_url: t.graph_base(),
            max_retries: 4,
            concurrency: 4,
            base_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_secs(2),
            allow_http: true,
        },
        auth,
    )
    .unwrap()
}

#[tokio::test]
async fn token_is_cached_and_user_resolves() {
    let t = FakeTenant::start().await;
    let u = t.add_user("adele@contoso.onmicrosoft.com", "Adele");
    let g = client(&t, "backup-app", "s");
    let got: User = g
        .get_json(&format!("/users/{}", banksia_graph::seg(&u.upn)))
        .await
        .unwrap();
    assert_eq!(got.id, u.id);
    let _: User = g.get_json(&format!("/users/{}", u.id)).await.unwrap();
    assert_eq!(t.token_requests(), 1);
}

#[tokio::test]
async fn auth_failure_is_clear_and_secret_free() {
    let t = FakeTenant::start().await;
    t.add_user("a@x", "A");
    let g = client(&t, "backup-app", "wrong");
    let e = g.get_json::<User>("/users/a@x").await.unwrap_err();
    let msg = e.to_string();
    assert!(matches!(e, GraphError::Auth(_)), "{msg}");
    assert!(msg.contains("AADSTS7000215"), "{msg}");
    assert!(
        !msg.contains("Trace ID"),
        "only the first line is kept: {msg}"
    );
    assert!(!msg.contains("wrong"), "{msg}");
}

#[tokio::test]
async fn retries_429_with_retry_after_then_succeeds() {
    let t = FakeTenant::start().await;
    t.add_user("a@x", "A");
    t.inject(Fault {
        path_contains: "/users/a@x".into(),
        status: 429,
        retry_after: Some(1),
        times: 2,
    });
    let g = client(&t, "c", "s");
    let start = Instant::now();
    let _: User = g.get_json("/users/a@x").await.unwrap();
    assert!(
        start.elapsed() >= Duration::from_secs(2),
        "Retry-After must be honoured"
    );
}

#[tokio::test]
async fn gives_up_after_retry_budget() {
    let t = FakeTenant::start().await;
    t.add_user("a@x", "A");
    t.inject(Fault {
        path_contains: "/users/a@x".into(),
        status: 503,
        retry_after: None,
        times: 100,
    });
    let g = client(&t, "c", "s");
    let e = g.get_json::<User>("/users/a@x").await.unwrap_err();
    assert_eq!(e.status(), Some(503));
    let n = t
        .requests()
        .iter()
        .filter(|r| r.contains("/users/a@x"))
        .count();
    assert_eq!(n, 5, "one try plus four retries");
}

#[tokio::test]
async fn creates_are_not_retried_on_ambiguous_gateway_errors() {
    let t = FakeTenant::start().await;
    let u = t.add_user("a@x", "A");
    let inbox = t.folder_id(&u, "Inbox").unwrap();
    t.inject(Fault {
        path_contains: "/childFolders".into(),
        status: 504,
        retry_after: None,
        times: 1,
    });
    let g = client(&t, "c", "s");
    let e = g
        .post_json::<MailFolder>(
            &format!("/users/{}/mailFolders/{inbox}/childFolders", u.id),
            &serde_json::json!({"displayName": "X"}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.status(), Some(504));
}

#[tokio::test]
async fn paging_and_delta_with_expiry() {
    let t = FakeTenant::start().await;
    t.set_page_size(2);
    let u = t.add_user("a@x", "A");
    for i in 0..5 {
        t.add_message(
            &u,
            "Inbox",
            format!("Subject: m{i}\r\nMessage-ID: <m{i}@x>\r\n\r\nbody").as_bytes(),
        );
    }
    let inbox = t.folder_id(&u, "Inbox").unwrap();
    let g = client(&t, "c", "s");
    let (msgs, delta) = g
        .delta_all::<Message>(
            &format!("/users/{}/mailFolders/{inbox}/messages/delta", u.id),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(msgs.len(), 5);

    let id = msgs[0].id.clone();
    t.delete_message(&id);
    t.add_message(&u, "Inbox", b"Subject: new\r\n\r\nx");
    let (changes, delta2) = g.delta_all::<Message>(&delta, &[]).await.unwrap();
    assert_eq!(changes.len(), 2);
    assert!(changes.iter().any(|m| m.id == id && m.removed.is_some()));

    t.expire_delta_tokens();
    let e = g.delta_all::<Message>(&delta2, &[]).await.unwrap_err();
    assert!(e.is_sync_state_expired(), "{e}");

    let folders: Vec<MailFolder> = g
        .get_all(&format!(
            "/users/{}/mailFolders?includeHiddenFolders=true",
            u.id
        ))
        .await
        .unwrap();
    assert_eq!(folders.len(), 3, "paged across two pages");
}

#[tokio::test]
async fn download_redirect_does_not_leak_token() {
    let t = FakeTenant::start().await;
    let u = t.add_user("a@x", "A");
    let id = t.add_file(&u, "doc.txt", b"hello world");
    let g = client(&t, "c", "s");
    let item: DriveItem = g
        .get_json(&format!("/drives/{}/root:/doc.txt", u.drive_id))
        .await
        .unwrap();
    assert_eq!(item.id, id);
    let body = g
        .download(&format!("/drives/{}/items/{id}/content", u.drive_id))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"hello world");
    assert_eq!(t.token_leaks(), 0);
}

#[tokio::test]
async fn refuses_links_outside_graph_origin() {
    let t = FakeTenant::start().await;
    let g = client(&t, "c", "s");
    let e = g
        .get_json::<User>("https://evil.example/v1.0/users/x")
        .await
        .unwrap_err();
    assert!(matches!(e, GraphError::Refused(_)));
    assert_eq!(t.token_requests(), 0, "no token is even requested");
}

#[tokio::test]
async fn read_only_client_cannot_write() {
    let t = FakeTenant::start().await;
    let u = t.add_user("a@x", "A");
    t.set_read_only_client("backup-app");
    let g = client(&t, "backup-app", "s");
    let e = g
        .post_json::<MailFolder>(
            &format!("/users/{}/mailFolders", u.id),
            &serde_json::json!({"displayName": "X"}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.status(), Some(403));
}

#[tokio::test]
async fn upload_session_in_fragments() {
    let t = FakeTenant::start().await;
    let u = t.add_user("a@x", "A");
    let g = client(&t, "c", "s");
    let sess: banksia_graph::types::UploadSession = g
        .post_json(
            &format!("/drives/{}/root:/big.bin:/createUploadSession", u.drive_id),
            &serde_json::json!({"item": {"@microsoft.graph.conflictBehavior": "fail"}}),
        )
        .await
        .unwrap();
    let data: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
    let total = data.len() as u64;
    assert!(
        g.upload_fragment(&sess.upload_url, data[..600].to_vec().into(), 0, total)
            .await
            .unwrap()
            .is_none()
    );
    // A repeated fragment is accepted.
    assert!(
        g.upload_fragment(&sess.upload_url, data[..600].to_vec().into(), 0, total)
            .await
            .unwrap()
            .is_none()
    );
    let done = g
        .upload_fragment(&sess.upload_url, data[600..].to_vec().into(), 600, total)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done["size"], 1000);
    assert_eq!(t.file_at(&u, "big.bin").unwrap(), data);
    assert_eq!(t.token_leaks(), 0);
}
