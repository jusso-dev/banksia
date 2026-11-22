//! An in-process fake of the Microsoft Graph endpoints Banksia uses.
//!
//! Tests seed users, mail and files, change them between runs, inject
//! throttling and expired delta state, and inspect what restores wrote.
//! The fake enforces two safety properties so tests can prove them:
//! read-only clients cannot write, and pre-authenticated download and upload
//! URLs reject any request that carries a bearer token.

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use base64::Engine;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::SocketAddr,
    sync::{Arc, Mutex, MutexGuard},
};

#[derive(Debug, Clone)]
pub struct FakeUser {
    pub id: String,
    pub upn: String,
    pub display_name: String,
    pub drive_id: String,
    pub root_folder_id: String,
}

#[derive(Debug, Clone)]
pub struct FakeFolder {
    pub id: String,
    pub user_id: String,
    pub parent_id: String,
    pub name: String,
    pub hidden: bool,
}

#[derive(Debug, Clone)]
pub struct FakeMessage {
    pub id: String,
    pub user_id: String,
    pub folder_id: String,
    pub mime: Vec<u8>,
    pub subject: String,
    pub internet_message_id: Option<String>,
    pub from: Option<String>,
    pub received: String,
    pub change: u64,
    seq: u64,
}

#[derive(Debug, Clone)]
pub struct FakeItem {
    pub id: String,
    pub drive_id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub folder: bool,
    pub content: Vec<u8>,
    pub deleted: bool,
    content_version: u64,
    meta_version: u64,
    seq: u64,
}

#[derive(Debug, Clone)]
pub struct Fault {
    pub path_contains: String,
    pub status: u16,
    pub retry_after: Option<u64>,
    pub times: u32,
}

#[derive(Default)]
struct UploadSession {
    drive_id: String,
    parent_id: String,
    name: String,
    conflict: String,
    data: Vec<u8>,
    total: Option<u64>,
}

#[derive(Default)]
struct TenantState {
    base: String,
    next_id: u64,
    seq: u64,
    users: Vec<FakeUser>,
    folders: BTreeMap<String, FakeFolder>,
    messages: BTreeMap<String, FakeMessage>,
    /// (seq, folder id, message id)
    tombstones: Vec<(u64, String, String)>,
    items: BTreeMap<String, FakeItem>,
    uploads: HashMap<String, UploadSession>,
    tokens: HashMap<String, String>,
    read_only_clients: HashSet<String>,
    min_valid_seq: u64,
    page_size: usize,
    faults: Vec<Fault>,
    corrupt_downloads: HashSet<String>,
    corrupt_next_upload: bool,
    drop_next_upload_response: bool,
    fail_message_imports: u32,
    requests: Vec<String>,
    token_requests: usize,
    token_leaks: usize,
    writes: usize,
}

impl TenantState {
    fn id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}-{:06}", self.next_id)
    }
    fn bump(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
    fn user(&self, key: &str) -> Option<FakeUser> {
        self.users
            .iter()
            .find(|u| u.id.eq_ignore_ascii_case(key) || u.upn.eq_ignore_ascii_case(key))
            .cloned()
    }
    fn user_by_drive(&self, drive_id: &str) -> Option<FakeUser> {
        self.users.iter().find(|u| u.drive_id == drive_id).cloned()
    }
}

/// A running fake tenant. Dropping it stops the server.
pub struct FakeTenant {
    state: Arc<Mutex<TenantState>>,
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeTenant {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeTenant {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake tenant");
        let addr = listener.local_addr().expect("local addr");
        let state = Arc::new(Mutex::new(TenantState {
            base: format!("http://{addr}"),
            page_size: 100,
            ..Default::default()
        }));
        let app = Router::new()
            .fallback(handle)
            .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { state, addr, task }
    }

    fn st(&self) -> MutexGuard<'_, TenantState> {
        self.state.lock().expect("fake tenant state")
    }

    /// Authority host, for example `http://127.0.0.1:1234`.
    pub fn authority(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Graph base URL, for example `http://127.0.0.1:1234/v1.0`.
    pub fn graph_base(&self) -> String {
        format!("http://{}/v1.0", self.addr)
    }

    /// Change a user's UPN, keeping the object id.
    pub fn rename_user(&self, id: &str, upn: &str) {
        let mut s = self.st();
        let u = s
            .users
            .iter_mut()
            .find(|u| u.id == id)
            .expect("user exists");
        u.upn = upn.to_string();
    }

    pub fn set_page_size(&self, n: usize) {
        self.st().page_size = n.max(1);
    }

    /// Requests from this client id may only read.
    pub fn set_read_only_client(&self, client_id: &str) {
        self.st().read_only_clients.insert(client_id.to_string());
    }

    pub fn inject(&self, fault: Fault) {
        self.st().faults.push(fault);
    }

    /// Invalidate every delta token issued so far.
    pub fn expire_delta_tokens(&self) {
        let mut s = self.st();
        s.min_valid_seq = s.seq + 1;
    }

    /// Serve different bytes for this item than its reported hash.
    pub fn corrupt_download(&self, item_id: &str) {
        self.st().corrupt_downloads.insert(item_id.to_string());
    }

    /// Commit the next upload fragment but answer 504, as if the response was lost.
    pub fn drop_next_upload_response(&self) {
        self.st().drop_next_upload_response = true;
    }

    /// Reject the next `n` MIME message imports with 503.
    pub fn fail_message_imports(&self, n: u32) {
        self.st().fail_message_imports = n;
    }

    /// Flip a byte in the next uploaded file.
    pub fn corrupt_next_upload(&self) {
        self.st().corrupt_next_upload = true;
    }

    pub fn requests(&self) -> Vec<String> {
        self.st().requests.clone()
    }

    pub fn clear_requests(&self) {
        self.st().requests.clear();
    }

    pub fn token_requests(&self) -> usize {
        self.st().token_requests
    }

    pub fn token_leaks(&self) -> usize {
        self.st().token_leaks
    }

    pub fn writes(&self) -> usize {
        self.st().writes
    }

    // ---- seeding: users ----

    pub fn add_user(&self, upn: &str, display_name: &str) -> FakeUser {
        let mut s = self.st();
        let n = s.users.len() + 1;
        let id = format!("00000000-0000-4000-8000-{n:012}");
        let drive_id = format!("b!drive{n:04}");
        let root_folder_id = format!("AAMkroot{n:04}");
        let user = FakeUser {
            id: id.clone(),
            upn: upn.to_string(),
            display_name: display_name.to_string(),
            drive_id: drive_id.clone(),
            root_folder_id: root_folder_id.clone(),
        };
        s.users.push(user.clone());
        for name in ["Inbox", "Sent Items", "Deleted Items"] {
            let fid = s.id("AAMkfolder");
            s.folders.insert(
                fid.clone(),
                FakeFolder {
                    id: fid,
                    user_id: id.clone(),
                    parent_id: root_folder_id.clone(),
                    name: name.to_string(),
                    hidden: false,
                },
            );
        }
        let seq = s.bump();
        let root_id = format!("root-{drive_id}");
        s.items.insert(
            root_id.clone(),
            FakeItem {
                id: root_id,
                drive_id,
                parent_id: None,
                name: "root".into(),
                folder: true,
                content: vec![],
                deleted: false,
                content_version: 1,
                meta_version: 1,
                seq,
            },
        );
        user
    }

    // ---- seeding and inspection: mail ----

    /// Folder id by slash-separated display-name path, for example `Inbox/Projects`.
    pub fn folder_id(&self, user: &FakeUser, path: &str) -> Option<String> {
        let s = self.st();
        let mut parent = user.root_folder_id.clone();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            parent = s
                .folders
                .values()
                .find(|f| f.user_id == user.id && f.parent_id == parent && f.name == part)?
                .id
                .clone();
        }
        Some(parent)
    }

    pub fn add_mail_folder(
        &self,
        user: &FakeUser,
        parent_path: &str,
        name: &str,
        hidden: bool,
    ) -> String {
        let parent = self
            .folder_id(user, parent_path)
            .expect("parent folder exists");
        let mut s = self.st();
        let id = s.id("AAMkfolder");
        s.folders.insert(
            id.clone(),
            FakeFolder {
                id: id.clone(),
                user_id: user.id.clone(),
                parent_id: parent,
                name: name.to_string(),
                hidden,
            },
        );
        id
    }

    pub fn add_message(&self, user: &FakeUser, folder_path: &str, mime: &[u8]) -> String {
        let folder = self.folder_id(user, folder_path).expect("folder exists");
        let mut s = self.st();
        insert_message(&mut s, &user.id, &folder, mime.to_vec())
    }

    /// Change a message's metadata (new change key) without changing content.
    pub fn touch_message(&self, id: &str) {
        let mut s = self.st();
        let seq = s.bump();
        let m = s.messages.get_mut(id).expect("message exists");
        m.change += 1;
        m.seq = seq;
    }

    pub fn delete_message(&self, id: &str) {
        let mut s = self.st();
        let seq = s.bump();
        let m = s.messages.remove(id).expect("message exists");
        s.tombstones.push((seq, m.folder_id, m.id));
    }

    pub fn move_message(&self, id: &str, user: &FakeUser, folder_path: &str) {
        let folder = self.folder_id(user, folder_path).expect("folder exists");
        let mut s = self.st();
        let seq = s.bump();
        let m = s.messages.get_mut(id).expect("message exists");
        let old = std::mem::replace(&mut m.folder_id, folder);
        m.seq = seq;
        m.change += 1;
        let mid = m.id.clone();
        s.tombstones.push((seq, old, mid));
    }

    pub fn messages_in(&self, user: &FakeUser, folder_path: &str) -> Vec<FakeMessage> {
        let Some(folder) = self.folder_id(user, folder_path) else {
            return vec![];
        };
        self.st()
            .messages
            .values()
            .filter(|m| m.folder_id == folder)
            .cloned()
            .collect()
    }

    pub fn message_count(&self, user: &FakeUser) -> usize {
        self.st()
            .messages
            .values()
            .filter(|m| m.user_id == user.id)
            .count()
    }

    // ---- seeding and inspection: drive ----

    fn item_by_path(s: &TenantState, drive_id: &str, path: &str) -> Option<String> {
        let mut cur = format!("root-{drive_id}");
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur = s
                .items
                .values()
                .find(|i| {
                    !i.deleted
                        && i.drive_id == drive_id
                        && i.parent_id.as_deref() == Some(&cur)
                        && i.name.eq_ignore_ascii_case(part)
                })?
                .id
                .clone();
        }
        Some(cur)
    }

    pub fn add_drive_folder(&self, user: &FakeUser, path: &str) -> String {
        let (parent, name) = split_parent(path);
        let mut s = self.st();
        let pid = Self::item_by_path(&s, &user.drive_id, parent).expect("parent exists");
        insert_item(&mut s, &user.drive_id, &pid, name, true, vec![])
    }

    pub fn add_file(&self, user: &FakeUser, path: &str, content: &[u8]) -> String {
        let (parent, name) = split_parent(path);
        let mut s = self.st();
        let pid = Self::item_by_path(&s, &user.drive_id, parent).expect("parent exists");
        insert_item(&mut s, &user.drive_id, &pid, name, false, content.to_vec())
    }

    pub fn update_file(&self, id: &str, content: &[u8]) {
        let mut s = self.st();
        let seq = s.bump();
        let i = s.items.get_mut(id).expect("item exists");
        i.content = content.to_vec();
        i.content_version += 1;
        i.meta_version += 1;
        i.seq = seq;
    }

    pub fn rename_item(&self, id: &str, name: &str) {
        let mut s = self.st();
        let seq = s.bump();
        let i = s.items.get_mut(id).expect("item exists");
        i.name = name.to_string();
        i.meta_version += 1;
        i.seq = seq;
    }

    pub fn delete_item(&self, id: &str) {
        let mut s = self.st();
        let seq = s.bump();
        let i = s.items.get_mut(id).expect("item exists");
        i.deleted = true;
        i.seq = seq;
    }

    pub fn file_at(&self, user: &FakeUser, path: &str) -> Option<Vec<u8>> {
        let s = self.st();
        let id = Self::item_by_path(&s, &user.drive_id, path)?;
        let i = s.items.get(&id)?;
        (!i.folder).then(|| i.content.clone())
    }

    pub fn file_count(&self, user: &FakeUser) -> usize {
        self.st()
            .items
            .values()
            .filter(|i| i.drive_id == user.drive_id && !i.folder && !i.deleted)
            .count()
    }
}

fn split_parent(path: &str) -> (&str, &str) {
    let p = path.trim_matches('/');
    match p.rfind('/') {
        Some(i) => (&p[..i], &p[i + 1..]),
        None => ("", p),
    }
}

fn insert_message(s: &mut TenantState, user_id: &str, folder: &str, mime: Vec<u8>) -> String {
    let headers = parse_headers(&mime);
    let id = s.id("AAMkmsg");
    let seq = s.bump();
    s.messages.insert(
        id.clone(),
        FakeMessage {
            id: id.clone(),
            user_id: user_id.to_string(),
            folder_id: folder.to_string(),
            subject: headers.get("subject").cloned().unwrap_or_default(),
            internet_message_id: headers.get("message-id").cloned(),
            from: headers.get("from").cloned(),
            received: "2026-11-20T01:02:03Z".into(),
            mime,
            change: 1,
            seq,
        },
    );
    id
}

fn insert_item(
    s: &mut TenantState,
    drive_id: &str,
    parent: &str,
    name: &str,
    folder: bool,
    content: Vec<u8>,
) -> String {
    let id = s.id("01ITEM");
    let seq = s.bump();
    s.items.insert(
        id.clone(),
        FakeItem {
            id: id.clone(),
            drive_id: drive_id.to_string(),
            parent_id: Some(parent.to_string()),
            name: name.to_string(),
            folder,
            content,
            deleted: false,
            content_version: 1,
            meta_version: 1,
            seq,
        },
    );
    id
}

/// Very small RFC 5322 header reader: unfolds lines, lower-cases names.
fn parse_headers(mime: &[u8]) -> HashMap<String, String> {
    let text = String::from_utf8_lossy(mime);
    let head = text.split("\r\n\r\n").next().unwrap_or("");
    let head = head.split("\n\n").next().unwrap_or("");
    let mut out = HashMap::new();
    let mut last: Option<String> = None;
    for line in head.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(k) = &last
                && let Some(v) = out.get_mut(k)
            {
                let v: &mut String = v;
                v.push(' ');
                v.push_str(line.trim());
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            out.entry(k.clone()).or_insert_with(|| v.trim().to_string());
            last = Some(k);
        }
    }
    out
}

// ---------------------------------------------------------------- HTTP --

fn err(status: u16, code: &str, message: &str) -> Response {
    let st = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        st,
        axum::Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}

fn ok(v: Value) -> Response {
    (StatusCode::OK, axum::Json(v)).into_response()
}

fn created(v: Value) -> Response {
    (StatusCode::CREATED, axum::Json(v)).into_response()
}

fn query(uri: &Uri) -> HashMap<String, String> {
    uri.query()
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default()
}

fn decode_path(p: &str) -> String {
    percent_encoding::percent_decode_str(p)
        .decode_utf8_lossy()
        .into_owned()
}

async fn handle(
    State(state): State<Arc<Mutex<TenantState>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut s = state.lock().expect("fake tenant state");
    let path = uri.path().to_string();
    s.requests.push(format!("{method} {path}"));

    // Fault injection.
    if let Some(i) = s
        .faults
        .iter()
        .position(|f| f.times > 0 && path.contains(&f.path_contains))
    {
        let f = &mut s.faults[i];
        f.times -= 1;
        let (status, ra) = (f.status, f.retry_after);
        let mut r = err(
            status,
            if status == 429 {
                "TooManyRequests"
            } else {
                "ServiceUnavailable"
            },
            "injected fault",
        );
        if let Some(secs) = ra {
            r.headers_mut().insert(
                header::RETRY_AFTER,
                secs.to_string().parse().expect("header"),
            );
        }
        return r;
    }

    if path.ends_with("/oauth2/v2.0/token") && method == Method::POST {
        return token(&mut s, &body);
    }
    if let Some(rest) = path.strip_prefix("/download/") {
        if headers.contains_key(header::AUTHORIZATION) {
            s.token_leaks += 1;
            return err(
                400,
                "tokenNotAllowed",
                "pre-authenticated URL received a bearer token",
            );
        }
        let id = decode_path(rest);
        let Some(item) = s.items.get(&id).cloned() else {
            return err(404, "itemNotFound", "no such item");
        };
        let mut content = item.content;
        if s.corrupt_downloads.contains(&id) && !content.is_empty() {
            content[0] ^= 0xff;
        }
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/octet-stream")],
            content,
        )
            .into_response();
    }
    if let Some(sid) = path.strip_prefix("/upload/") {
        if headers.contains_key(header::AUTHORIZATION) {
            s.token_leaks += 1;
            return err(400, "tokenNotAllowed", "upload URL received a bearer token");
        }
        if method == Method::GET {
            return match s.uploads.get(sid) {
                Some(sess) => ok(json!({"nextExpectedRanges": [format!("{}-", sess.data.len())]})),
                None => err(404, "itemNotFound", "upload session not found"),
            };
        }
        if s.drop_next_upload_response {
            // Commit the fragment, then fail as if the response was lost.
            s.drop_next_upload_response = false;
            let _ = upload_fragment(&mut s, sid, &headers, &body);
            return err(504, "GatewayTimeout", "injected lost response");
        }
        return upload_fragment(&mut s, sid, &headers, &body);
    }
    let Some(gpath) = path.strip_prefix("/v1.0") else {
        return err(404, "notFound", "unknown path");
    };

    // Auth.
    let Some(client) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .and_then(|t| s.tokens.get(t).cloned())
    else {
        return err(
            401,
            "InvalidAuthenticationToken",
            "missing or invalid token",
        );
    };
    if method != Method::GET {
        if s.read_only_clients.contains(&client) {
            return err(
                403,
                "ErrorAccessDenied",
                "Access is denied. Check credentials and try again.",
            );
        }
        s.writes += 1;
    }
    let q = query(&uri);
    route(&mut s, &method, gpath, &q, &headers, &body)
}

fn token(s: &mut TenantState, body: &[u8]) -> Response {
    s.token_requests += 1;
    let form: HashMap<String, String> = url::form_urlencoded::parse(body).into_owned().collect();
    if form.get("grant_type").map(String::as_str) != Some("client_credentials") {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "unsupported_grant_type"})),
        )
            .into_response();
    }
    let Some(client_id) = form.get("client_id").cloned() else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "invalid_request"})),
        )
            .into_response();
    };
    let has_secret = form.get("client_secret").is_some_and(|v| !v.is_empty());
    let has_assertion = form
        .get("client_assertion")
        .is_some_and(|v| v.split('.').count() == 3);
    if !(has_secret || has_assertion) {
        return (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "invalid_client", "error_description": "AADSTS7000215: Invalid client secret provided.\r\nTrace ID: x"}))).into_response();
    }
    if form.get("client_secret").map(String::as_str) == Some("wrong") {
        return (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "invalid_client", "error_description": "AADSTS7000215: Invalid client secret provided.\r\nTrace ID: x"}))).into_response();
    }
    let n = s.token_requests;
    let tok = format!("fake-token-{n}-{client_id}");
    s.tokens.insert(tok.clone(), client_id);
    ok(json!({"token_type": "Bearer", "expires_in": 3599, "access_token": tok}))
}

fn route(
    s: &mut TenantState,
    method: &Method,
    path: &str,
    q: &HashMap<String, String>,
    headers: &HeaderMap,
    body: &[u8],
) -> Response {
    // Drive path addressing: /drives/{d}/root:/{path}[:][/content|/createUploadSession]
    if let Some(rest) = path.strip_prefix("/drives/")
        && let Some((drive, after)) = rest.split_once("/root:")
    {
        let drive = decode_path(drive);
        let after = decode_path(after);
        if let Some(p) = after.strip_suffix(":/content") {
            return match *method {
                Method::PUT => drive_put_content(s, &drive, p, q, body),
                _ => err(405, "notAllowed", "method"),
            };
        }
        if let Some(p) = after.strip_suffix(":/createUploadSession") {
            return create_upload_session(s, &drive, p, body);
        }
        let p = after.trim_end_matches(':');
        return match FakeTenant::item_by_path(s, &drive, p) {
            Some(id) => ok(item_json(s, &s.items[&id])),
            None => err(404, "itemNotFound", "The resource could not be found."),
        };
    }

    let segs: Vec<String> = path
        .split('/')
        .filter(|p| !p.is_empty())
        .map(decode_path)
        .collect();
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    match (method.clone(), segs.as_slice()) {
        (Method::GET, ["users", u]) => match s.user(u) {
            Some(user) => ok(
                json!({"id": user.id, "userPrincipalName": user.upn, "displayName": user.display_name}),
            ),
            None => err(404, "Request_ResourceNotFound", "Resource does not exist."),
        },
        (Method::GET, ["users", u, "drive"]) => match s.user(u) {
            Some(user) => ok(json!({"id": user.drive_id, "driveType": "business"})),
            None => err(404, "Request_ResourceNotFound", "Resource does not exist."),
        },
        (Method::GET, ["users", u, "mailFolders"]) => {
            let Some(user) = s.user(u) else {
                return err(404, "ErrorInvalidUser", "no user");
            };
            list_folders(s, &user, &user.root_folder_id, q, path)
        }
        (Method::GET, ["users", u, "mailFolders", f, "childFolders"]) => {
            let Some(user) = s.user(u) else {
                return err(404, "ErrorInvalidUser", "no user");
            };
            let f = f.to_string();
            list_folders(s, &user, &f, q, path)
        }
        (Method::POST, ["users", u, "mailFolders"]) => {
            let Some(user) = s.user(u) else {
                return err(404, "ErrorInvalidUser", "no user");
            };
            create_folder(s, &user, &user.root_folder_id.clone(), body)
        }
        (Method::POST, ["users", u, "mailFolders", f, "childFolders"]) => {
            let Some(user) = s.user(u) else {
                return err(404, "ErrorInvalidUser", "no user");
            };
            let f = f.to_string();
            create_folder(s, &user, &f, body)
        }
        (Method::GET, ["users", u, "mailFolders", f, "messages", "delta"]) => {
            let Some(user) = s.user(u) else {
                return err(404, "ErrorInvalidUser", "no user");
            };
            let f = f.to_string();
            mail_delta(s, &user, &f, q)
        }
        (Method::GET, ["users", u, "mailFolders", f, "messages"]) => {
            let Some(user) = s.user(u) else {
                return err(404, "ErrorInvalidUser", "no user");
            };
            let wanted = q.get("$filter").and_then(|f| {
                let a = f.find('\'')?;
                let b = f.rfind('\'')?;
                (b > a).then(|| f[a + 1..b].replace("''", "'"))
            });
            let list: Vec<Value> = s
                .messages
                .values()
                .filter(|m| m.user_id == user.id && m.folder_id == *f)
                .filter(|m| wanted.is_none() || m.internet_message_id == wanted)
                .map(message_json)
                .collect();
            ok(json!({"value": list}))
        }
        (Method::POST, ["users", u, "mailFolders", f, "messages"]) => {
            let Some(user) = s.user(u) else {
                return err(404, "ErrorInvalidUser", "no user");
            };
            if !s.folders.contains_key(*f) {
                return err(404, "ErrorItemNotFound", "folder not found");
            }
            if s.fail_message_imports > 0 {
                s.fail_message_imports -= 1;
                return err(503, "ServiceUnavailable", "injected import failure");
            }
            let ct = headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !ct.starts_with("text/plain") {
                return err(
                    400,
                    "ErrorMimeContentInvalid",
                    "expected base64 MIME as text/plain",
                );
            }
            let cleaned: Vec<u8> = body
                .iter()
                .copied()
                .filter(|b| !b.is_ascii_whitespace())
                .collect();
            let Ok(mime) = base64::engine::general_purpose::STANDARD.decode(cleaned) else {
                return err(400, "ErrorMimeContentInvalidBase64String", "invalid base64");
            };
            let f = f.to_string();
            let id = insert_message(s, &user.id, &f, mime);
            created(message_json(&s.messages[&id]))
        }
        (Method::GET, ["users", u, "messages", m, "$value"]) => {
            let Some(user) = s.user(u) else {
                return err(404, "ErrorInvalidUser", "no user");
            };
            match s.messages.get(*m).filter(|m| m.user_id == user.id) {
                Some(m) => (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "message/rfc822")],
                    m.mime.clone(),
                )
                    .into_response(),
                None => err(
                    404,
                    "ErrorItemNotFound",
                    "The specified object was not found in the store.",
                ),
            }
        }
        (Method::GET, ["users", u, "messages", m]) => {
            let Some(user) = s.user(u) else {
                return err(404, "ErrorInvalidUser", "no user");
            };
            match s.messages.get(*m).filter(|m| m.user_id == user.id) {
                Some(m) => ok(message_json(m)),
                None => err(
                    404,
                    "ErrorItemNotFound",
                    "The specified object was not found in the store.",
                ),
            }
        }
        (Method::GET, ["drives", d, "root", "delta"]) => {
            let d = d.to_string();
            drive_delta(s, &d, q)
        }
        (Method::GET, ["drives", d, "root"]) => {
            let id = format!("root-{d}");
            match s.items.get(&id) {
                Some(i) => ok(item_json(s, i)),
                None => err(404, "itemNotFound", "no drive"),
            }
        }
        (Method::GET, ["drives", _d, "items", i, "content"]) => match s.items.get(*i) {
            Some(item) if !item.folder && !item.deleted => {
                let loc = format!("{}/download/{}?sig=fake", s.base, item.id);
                (StatusCode::FOUND, [(header::LOCATION, loc)]).into_response()
            }
            _ => err(404, "itemNotFound", "The resource could not be found."),
        },
        (Method::GET, ["drives", d, "items", parent, "children"]) => {
            let list: Vec<Value> = s
                .items
                .values()
                .filter(|i| {
                    i.drive_id == *d && i.parent_id.as_deref() == Some(*parent) && !i.deleted
                })
                .map(|i| item_json(s, i))
                .collect();
            ok(json!({"value": list}))
        }
        (Method::POST, ["drives", d, "items", parent, "children"]) => {
            let (d, parent) = (d.to_string(), parent.to_string());
            drive_create_folder(s, &d, &parent, body)
        }
        (Method::POST, ["drives", d, "root", "children"]) => {
            let d = d.to_string();
            let parent = format!("root-{d}");
            drive_create_folder(s, &d, &parent, body)
        }
        _ => err(
            400,
            "BadRequest",
            &format!("fake tenant does not implement {method} {path}"),
        ),
    }
}

fn page<T: Clone>(all: &[T], q: &HashMap<String, String>, size: usize) -> (Vec<T>, Option<usize>) {
    let skip: usize = q.get("$skip").and_then(|v| v.parse().ok()).unwrap_or(0);
    let end = (skip + size).min(all.len());
    let next = (end < all.len()).then_some(end);
    (all[skip.min(all.len())..end].to_vec(), next)
}

fn list_folders(
    s: &TenantState,
    user: &FakeUser,
    parent: &str,
    q: &HashMap<String, String>,
    path: &str,
) -> Response {
    let include_hidden = q.get("includeHiddenFolders").map(String::as_str) == Some("true");
    let all: Vec<Value> = s
        .folders
        .values()
        .filter(|f| f.user_id == user.id && f.parent_id == parent && (include_hidden || !f.hidden))
        .map(|f| {
            let children = s.folders.values().filter(|c| c.parent_id == f.id).count();
            let total = s.messages.values().filter(|m| m.folder_id == f.id).count();
            json!({"id": f.id, "displayName": f.name, "parentFolderId": f.parent_id,
                   "childFolderCount": children, "totalItemCount": total, "isHidden": f.hidden})
        })
        .collect();
    let (items, next) = page(&all, q, s.page_size);
    let mut v = json!({"value": items});
    if let Some(n) = next {
        let hidden = if include_hidden {
            "includeHiddenFolders=true&"
        } else {
            ""
        };
        v["@odata.nextLink"] = json!(format!("{}/v1.0{}?{hidden}$skip={n}", s.base, path));
    }
    ok(v)
}

fn create_folder(s: &mut TenantState, user: &FakeUser, parent: &str, body: &[u8]) -> Response {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let Some(name) = v["displayName"].as_str() else {
        return err(400, "ErrorInvalidRequest", "displayName required");
    };
    if s.folders
        .values()
        .any(|f| f.user_id == user.id && f.parent_id == parent && f.name.eq_ignore_ascii_case(name))
    {
        return err(
            409,
            "ErrorFolderExists",
            "A folder with the specified name already exists.",
        );
    }
    let id = s.id("AAMkfolder");
    let f = FakeFolder {
        id: id.clone(),
        user_id: user.id.clone(),
        parent_id: parent.to_string(),
        name: name.to_string(),
        hidden: false,
    };
    s.folders.insert(id.clone(), f);
    created(
        json!({"id": id, "displayName": name, "parentFolderId": parent, "childFolderCount": 0, "totalItemCount": 0}),
    )
}

fn message_json(m: &FakeMessage) -> Value {
    json!({
        "id": m.id,
        "changeKey": format!("ck-{}-{}", m.id, m.change),
        "subject": m.subject,
        "from": m.from.as_ref().map(|f| json!({"emailAddress": {"address": f}})),
        "receivedDateTime": m.received,
        "internetMessageId": m.internet_message_id,
        "hasAttachments": false,
        "parentFolderId": m.folder_id,
    })
}

/// Parse a delta or skip token of the form `since:upto:offset`.
fn parse_token(t: &str) -> Option<(u64, u64, usize)> {
    let mut it = t.split(':');
    Some((
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    ))
}

fn mail_delta(
    s: &TenantState,
    user: &FakeUser,
    folder: &str,
    q: &HashMap<String, String>,
) -> Response {
    if !s.folders.get(folder).is_some_and(|f| f.user_id == user.id) {
        return err(404, "ErrorItemNotFound", "folder not found");
    }
    let (since, upto, offset) = if let Some(t) = q.get("$skiptoken") {
        match parse_token(t) {
            Some(x) => x,
            None => return err(400, "badToken", "bad skiptoken"),
        }
    } else if let Some(t) = q.get("$deltatoken") {
        match t.parse::<u64>() {
            Ok(since) => (since, s.seq, 0),
            Err(_) => return err(400, "badToken", "bad deltatoken"),
        }
    } else {
        (0, s.seq, 0)
    };
    if since > 0 && since < s.min_valid_seq {
        return err(
            410,
            "syncStateNotFound",
            "The sync state generation is not found.",
        );
    }
    let mut changes: Vec<(u64, Value)> = s
        .messages
        .values()
        .filter(|m| m.folder_id == folder && m.seq > since && m.seq <= upto)
        .map(|m| (m.seq, message_json(m)))
        .collect();
    if since > 0 {
        changes.extend(
            s.tombstones
                .iter()
                .filter(|(seq, f, _)| f == folder && *seq > since && *seq <= upto)
                .map(|(seq, _, id)| (*seq, json!({"id": id, "@removed": {"reason": "deleted"}}))),
        );
    }
    changes.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1["id"].as_str().cmp(&b.1["id"].as_str()))
    });
    let all: Vec<Value> = changes.into_iter().map(|(_, v)| v).collect();
    let end = (offset + s.page_size).min(all.len());
    let mut v = json!({"value": all[offset.min(all.len())..end]});
    let base = format!(
        "{}/v1.0/users/{}/mailFolders/{}/messages/delta",
        s.base, user.id, folder
    );
    if end < all.len() {
        v["@odata.nextLink"] = json!(format!("{base}?$skiptoken={since}:{upto}:{end}"));
    } else {
        v["@odata.deltaLink"] = json!(format!("{base}?$deltatoken={upto}"));
    }
    ok(v)
}

fn item_json(s: &TenantState, i: &FakeItem) -> Value {
    let mut v = json!({
        "id": i.id,
        "name": i.name,
        "eTag": format!("\"e:{},{}\"", i.id, i.meta_version),
        "cTag": format!("\"c:{},{}\"", i.id, i.content_version),
        "createdDateTime": "2026-11-01T00:00:00Z",
        "lastModifiedDateTime": format!("2026-11-01T00:00:{:02}Z", i.seq % 60),
        "parentReference": i.parent_id.as_ref().map(|p| json!({"id": p, "driveId": i.drive_id})),
    });
    if i.deleted {
        v["deleted"] = json!({"state": "deleted"});
        return v;
    }
    if i.parent_id.is_none() {
        v["root"] = json!({});
    }
    if i.folder {
        let n = s
            .items
            .values()
            .filter(|c| c.parent_id.as_deref() == Some(&i.id) && !c.deleted)
            .count();
        v["folder"] = json!({"childCount": n});
        v["size"] = json!(0);
    } else {
        v["size"] = json!(i.content.len());
        v["file"] = json!({"mimeType": "application/octet-stream", "hashes": {"quickXorHash": banksia_quickxor::hash_base64(&i.content)}});
    }
    v
}

fn drive_delta(s: &TenantState, drive: &str, q: &HashMap<String, String>) -> Response {
    if s.user_by_drive(drive).is_none() {
        return err(404, "itemNotFound", "drive not found");
    }
    let (since, upto, offset) = if let Some(t) = q.get("token") {
        match parse_token(t) {
            Some(x) => x,
            None => match t.parse::<u64>() {
                Ok(since) => (since, s.seq, 0),
                Err(_) => return err(400, "badToken", "bad token"),
            },
        }
    } else {
        (0, s.seq, 0)
    };
    if since > 0 && since < s.min_valid_seq {
        return err(
            410,
            "resyncRequired",
            "Resync required. Replace any local items with the server's version.",
        );
    }
    let mut changes: Vec<&FakeItem> = s
        .items
        .values()
        .filter(|i| {
            i.drive_id == drive && i.seq > since && i.seq <= upto && (since > 0 || !i.deleted)
        })
        .collect();
    changes.sort_by(|a, b| a.seq.cmp(&b.seq).then_with(|| a.id.cmp(&b.id)));
    let all: Vec<Value> = changes.into_iter().map(|i| item_json(s, i)).collect();
    let end = (offset + s.page_size).min(all.len());
    let mut v = json!({"value": all[offset.min(all.len())..end]});
    let base = format!("{}/v1.0/drives/{}/root/delta", s.base, drive);
    if end < all.len() {
        v["@odata.nextLink"] = json!(format!("{base}?token={since}:{upto}:{end}"));
    } else {
        v["@odata.deltaLink"] = json!(format!("{base}?token={upto}"));
    }
    ok(v)
}

fn drive_create_folder(s: &mut TenantState, drive: &str, parent: &str, body: &[u8]) -> Response {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let Some(name) = v["name"].as_str() else {
        return err(400, "invalidRequest", "name required");
    };
    if !s
        .items
        .get(parent)
        .is_some_and(|p| p.folder && !p.deleted && p.drive_id == drive)
    {
        return err(404, "itemNotFound", "parent not found");
    }
    if let Some(existing) = s.items.values().find(|i| {
        !i.deleted && i.parent_id.as_deref() == Some(parent) && i.name.eq_ignore_ascii_case(name)
    }) {
        if v["@microsoft.graph.conflictBehavior"].as_str() == Some("fail") {
            return err(
                409,
                "nameAlreadyExists",
                "An item with the same name already exists under the parent",
            );
        }
        return ok(item_json(s, existing));
    }
    let id = insert_item(s, drive, parent, name, true, vec![]);
    created(item_json(s, &s.items[&id]))
}

fn store_file(
    s: &mut TenantState,
    drive: &str,
    path: &str,
    conflict: &str,
    mut data: Vec<u8>,
) -> Response {
    let (parent_path, name) = split_parent(path);
    let Some(parent) = FakeTenant::item_by_path(s, drive, parent_path) else {
        return err(404, "itemNotFound", "parent folder not found");
    };
    if s.corrupt_next_upload && !data.is_empty() {
        data[0] ^= 0xff;
        s.corrupt_next_upload = false;
    }
    if let Some(existing) = FakeTenant::item_by_path(s, drive, path) {
        match conflict {
            "fail" => {
                return err(
                    409,
                    "nameAlreadyExists",
                    "The specified item name already exists.",
                );
            }
            "replace" => {
                let seq = s.bump();
                let i = s.items.get_mut(&existing).expect("exists");
                i.content = data;
                i.content_version += 1;
                i.meta_version += 1;
                i.seq = seq;
                return ok(item_json(s, &s.items[&existing]));
            }
            _ => {
                let (stem, ext) = match name.rfind('.') {
                    Some(i) if i > 0 => (&name[..i], &name[i..]),
                    _ => (name, ""),
                };
                let mut n = 1;
                let new_name = loop {
                    let candidate = format!("{stem} {n}{ext}");
                    let p = if parent_path.is_empty() {
                        candidate.clone()
                    } else {
                        format!("{parent_path}/{candidate}")
                    };
                    if FakeTenant::item_by_path(s, drive, &p).is_none() {
                        break candidate;
                    }
                    n += 1;
                };
                let id = insert_item(s, drive, &parent, &new_name, false, data);
                return created(item_json(s, &s.items[&id]));
            }
        }
    }
    let id = insert_item(s, drive, &parent, name, false, data);
    created(item_json(s, &s.items[&id]))
}

fn drive_put_content(
    s: &mut TenantState,
    drive: &str,
    path: &str,
    q: &HashMap<String, String>,
    body: &[u8],
) -> Response {
    if body.len() > 4 * 1024 * 1024 {
        return err(
            413,
            "requestTooLarge",
            "use an upload session for files over 4 MiB",
        );
    }
    let conflict = q
        .get("@microsoft.graph.conflictBehavior")
        .cloned()
        .unwrap_or_else(|| "replace".into());
    store_file(s, drive, path, &conflict, body.to_vec())
}

fn create_upload_session(s: &mut TenantState, drive: &str, path: &str, body: &[u8]) -> Response {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let conflict = v["item"]["@microsoft.graph.conflictBehavior"]
        .as_str()
        .unwrap_or("replace");
    let (parent_path, name) = split_parent(path);
    let Some(parent) = FakeTenant::item_by_path(s, drive, parent_path) else {
        return err(404, "itemNotFound", "parent folder not found");
    };
    if conflict == "fail" && FakeTenant::item_by_path(s, drive, path).is_some() {
        return err(
            409,
            "nameAlreadyExists",
            "The specified item name already exists.",
        );
    }
    let sid = s.id("session");
    s.uploads.insert(
        sid.clone(),
        UploadSession {
            drive_id: drive.to_string(),
            parent_id: parent,
            name: name.to_string(),
            conflict: conflict.to_string(),
            ..Default::default()
        },
    );
    ok(
        json!({"uploadUrl": format!("{}/upload/{sid}", s.base), "expirationDateTime": "2026-12-01T00:00:00Z"}),
    )
}

fn upload_fragment(s: &mut TenantState, sid: &str, headers: &HeaderMap, body: &[u8]) -> Response {
    let Some(range) = headers
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
    else {
        return err(400, "invalidRange", "Content-Range required");
    };
    let parsed = (|| {
        let r = range.strip_prefix("bytes ")?;
        let (span, total) = r.split_once('/')?;
        let (a, b) = span.split_once('-')?;
        Some((
            a.parse::<u64>().ok()?,
            b.parse::<u64>().ok()?,
            total.parse::<u64>().ok()?,
        ))
    })();
    let Some((start, end, total)) = parsed else {
        return err(400, "invalidRange", "bad Content-Range");
    };
    let Some(sess) = s.uploads.get_mut(sid) else {
        return err(404, "itemNotFound", "upload session not found");
    };
    if end + 1 - start != body.len() as u64 {
        return err(
            400,
            "invalidRange",
            "Content-Range does not match body length",
        );
    }
    sess.total = Some(total);
    let have = sess.data.len() as u64;
    if start < have {
        // Graph rejects a range it has already committed.
        return err(
            416,
            "invalidRange",
            "The uploaded fragment overlaps with data that has already been received.",
        );
    } else if start > have {
        return err(416, "invalidRange", "fragment out of order");
    } else {
        sess.data.extend_from_slice(body);
    }
    if sess.data.len() as u64 == total {
        let sess = s.uploads.remove(sid).expect("session");
        let parent_path = {
            // Rebuild the parent path for store_file.
            let mut parts = vec![];
            let mut cur = Some(sess.parent_id.clone());
            while let Some(id) = cur {
                let Some(i) = s.items.get(&id) else { break };
                if i.parent_id.is_none() {
                    break;
                }
                parts.push(i.name.clone());
                cur = i.parent_id.clone();
            }
            parts.reverse();
            parts.join("/")
        };
        let path = if parent_path.is_empty() {
            sess.name.clone()
        } else {
            format!("{parent_path}/{}", sess.name)
        };
        return store_file(s, &sess.drive_id, &path, &sess.conflict, sess.data);
    }
    (
        StatusCode::ACCEPTED,
        axum::Json(json!({"nextExpectedRanges": [format!("{}-", s.uploads[sid].data.len())]})),
    )
        .into_response()
}
