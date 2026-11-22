//! Exchange Online mailbox backup: folder tree, per-folder message delta, MIME.

use super::Run;
use crate::snapshot::{MailFolderEntry, MailSnapshot, MessageEntry, Principal};
use anyhow::Result;
use banksia_graph::types::{MailFolder, Message};
use std::collections::{BTreeMap, HashMap};

const SELECT: &str = "$select=subject,from,receivedDateTime,internetMessageId,changeKey,parentFolderId,hasAttachments";

pub(crate) async fn list_folders(run: &Run<'_>, user_id: &str) -> Result<Vec<MailFolderEntry>> {
    let mut out = Vec::new();
    let top: Vec<MailFolder> = run
        .graph
        .get_all(&format!(
            "/users/{user_id}/mailFolders?includeHiddenFolders=true"
        ))
        .await?;
    let mut queue: Vec<(MailFolder, String)> = top
        .into_iter()
        .map(|f| (f.clone(), f.display_name.clone()))
        .collect();
    while let Some((f, path)) = queue.pop() {
        if f.child_folder_count.unwrap_or(1) > 0 {
            let children: Vec<MailFolder> = run
                .graph
                .get_all(&format!(
                    "/users/{user_id}/mailFolders/{}/childFolders?includeHiddenFolders=true",
                    f.id
                ))
                .await?;
            for c in children {
                let p = format!("{path}/{}", c.display_name);
                queue.push((c, p));
            }
        }
        out.push(MailFolderEntry {
            id: f.id,
            parent_id: f.parent_folder_id,
            name: f.display_name,
            path,
            hidden: f.is_hidden.unwrap_or(false),
            delta_link: None,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

pub(crate) async fn backup(
    run: &mut Run<'_>,
    who: &Principal,
    parent: Option<&MailSnapshot>,
) -> Result<MailSnapshot> {
    let user_id = who.object_id.as_str();
    let mut folders = list_folders(run, user_id).await?;
    let prefer = crate::graph::mail_prefer();

    // Previous versions by message id, for carry-forward on full resync.
    let previous: HashMap<&str, &MessageEntry> = parent
        .map(|p| p.messages.iter().map(|m| (m.id.as_str(), m)).collect())
        .unwrap_or_default();
    let mut messages: Vec<MessageEntry> = Vec::new();

    for folder in folders.iter_mut() {
        let prior_link = parent
            .and_then(|p| p.folder(&folder.id))
            .and_then(|f| f.delta_link.clone());
        let start = format!(
            "/users/{user_id}/mailFolders/{}/messages/delta?{SELECT}",
            folder.id
        );
        let (changes, link, incremental) = match prior_link {
            Some(link) => match run.graph.delta_all::<Message>(&link, &prefer).await {
                Ok((c, l)) => (c, l, true),
                Err(e) if e.is_sync_state_expired() => {
                    tracing::info!(folder = %folder.path, "mail delta state expired, running full folder sync");
                    run.stats.full_resyncs += 1;
                    let (c, l) = run.graph.delta_all::<Message>(&start, &prefer).await?;
                    (c, l, false)
                }
                Err(e) => return Err(e.into()),
            },
            None => {
                let (c, l) = run.graph.delta_all::<Message>(&start, &prefer).await?;
                (c, l, false)
            }
        };
        let errors_before = run.errors.len();

        // Current contents of this folder: parent's view (incremental) or empty (full).
        let mut current: BTreeMap<String, MessageEntry> = if incremental {
            parent
                .map(|p| {
                    p.messages
                        .iter()
                        .filter(|m| m.folder_id == folder.id)
                        .map(|m| (m.id.clone(), m.clone()))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            BTreeMap::new()
        };

        // Collapse the change feed to the final state of each message before
        // downloading anything: a message added then removed in the same
        // window is never fetched, and one edited twice is fetched once.
        let mut last: BTreeMap<String, Message> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        for m in changes {
            if !last.contains_key(&m.id) {
                order.push(m.id.clone());
            }
            last.insert(m.id.clone(), m);
        }
        for id in order {
            let m = last.remove(&id).expect("collected above");
            if m.removed.is_some() {
                current.remove(&m.id);
                continue;
            }
            // Without a change key, Banksia cannot tell whether the message
            // changed, so it always downloads and never uses the checkpoint.
            let version = m.change_key.clone().filter(|v| !v.is_empty());
            let unchanged = previous
                .get(m.id.as_str())
                .filter(|p| version.is_some() && p.change_key == version);
            if let Some(p) = unchanged {
                run.stats.items_carried_forward += 1;
                current.insert(
                    m.id.clone(),
                    MessageEntry {
                        folder_id: folder.id.clone(),
                        ..(*p).clone()
                    },
                );
                continue;
            }
            let content = if let Some(c) = version.as_deref().and_then(|v| run.cp.get(&m.id, v)) {
                run.stats.items_carried_forward += 1;
                c.clone()
            } else {
                match run
                    .fetch(&format!("/users/{user_id}/messages/{}/$value", m.id))
                    .await
                {
                    Ok(f) => {
                        if let Some(v) = &version {
                            run.cp.record(&m.id, v, f.content.clone())?;
                        }
                        f.content
                    }
                    Err(e)
                        if e.downcast_ref::<banksia_graph::GraphError>()
                            .is_some_and(|g| g.is_not_found()) =>
                    {
                        // Deleted between the delta query and the download.
                        // The next delta reports the removal.
                        current.remove(&m.id);
                        continue;
                    }
                    Err(e) => {
                        run.error(&m.id, format!("{e:#}"));
                        // Keep the whole previous entry, including its old
                        // change key, so the next run downloads it again.
                        if let Some(p) = previous.get(m.id.as_str()) {
                            current.insert(
                                m.id.clone(),
                                MessageEntry {
                                    folder_id: folder.id.clone(),
                                    ..(*p).clone()
                                },
                            );
                        }
                        continue;
                    }
                }
            };
            current.insert(
                m.id.clone(),
                MessageEntry {
                    id: m.id,
                    folder_id: folder.id.clone(),
                    change_key: m.change_key,
                    subject: m.subject,
                    from: m.from.and_then(|f| f.email_address).and_then(|e| e.address),
                    received: m.received_date_time,
                    internet_message_id: m.internet_message_id,
                    content,
                },
            );
        }
        // If any message failed, keep the previous delta link so the next run
        // sees it again. Unchanged messages are matched by change key.
        folder.delta_link = if run.errors.len() == errors_before {
            Some(link)
        } else {
            parent
                .and_then(|p| p.folder(&folder.id))
                .and_then(|f| f.delta_link.clone())
        };
        messages.extend(current.into_values());
    }
    run.stats.items = messages.len() as u64;
    Ok(MailSnapshot { folders, messages })
}
