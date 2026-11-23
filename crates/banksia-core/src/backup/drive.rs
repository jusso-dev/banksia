//! OneDrive backup: root delta, path reconstruction, hash-verified content.

use super::Run;
use crate::snapshot::{DriveEntry, DriveSnapshot, Principal};
use anyhow::Result;
use banksia_graph::types::{Drive, DriveItem};
use std::collections::{BTreeMap, HashMap};

pub(crate) async fn backup(
    run: &mut Run<'_>,
    who: &Principal,
    parent: Option<&DriveSnapshot>,
) -> Result<DriveSnapshot> {
    let drive: Drive = run
        .graph
        .get_json(&format!("/users/{}/drive", who.object_id))
        .await?;
    let parent = parent.filter(|p| p.drive_id == drive.id);
    let start = format!("/drives/{}/root/delta", drive.id);

    let (changes, link, incremental) = match parent.and_then(|p| p.delta_link.clone()) {
        Some(link) => match run.graph.delta_all::<DriveItem>(&link, &[]).await {
            Ok((c, l)) => (c, l, true),
            Err(e) if e.is_sync_state_expired() => {
                tracing::info!("drive delta state expired, running full enumeration");
                run.stats.full_resyncs += 1;
                let (c, l) = run.graph.delta_all::<DriveItem>(&start, &[]).await?;
                (c, l, false)
            }
            Err(e) => return Err(e.into()),
        },
        None => {
            let (c, l) = run.graph.delta_all::<DriveItem>(&start, &[]).await?;
            (c, l, false)
        }
    };

    let previous: HashMap<&str, &DriveEntry> = parent
        .map(|p| p.items.iter().map(|i| (i.id.as_str(), i)).collect())
        .unwrap_or_default();
    let mut items: BTreeMap<String, DriveEntry> = if incremental {
        parent
            .map(|p| p.items.iter().map(|i| (i.id.clone(), i.clone())).collect())
            .unwrap_or_default()
    } else {
        BTreeMap::new()
    };

    // Apply metadata changes in order. Content is fetched afterwards.
    for it in changes {
        if it.deleted.is_some() {
            items.remove(&it.id);
            continue;
        }
        if it.package.is_some() {
            // OneNote notebooks and other packages are not backed up in version 1.0.
            continue;
        }
        let is_root = it.root.is_some();
        let folder = it.folder.is_some() || is_root;
        let prev_content = items
            .get(&it.id)
            .and_then(|e| e.content.clone())
            // Only reuse content when Graph gives a cTag to compare. With no
            // cTag, Banksia cannot tell whether the content changed.
            .filter(|_| {
                it.c_tag.is_some() && items.get(&it.id).map(|e| &e.c_tag) == Some(&it.c_tag)
            });
        items.insert(
            it.id.clone(),
            DriveEntry {
                id: it.id.clone(),
                parent_id: if is_root {
                    None
                } else {
                    it.parent_reference.as_ref().and_then(|r| r.id.clone())
                },
                name: if is_root {
                    String::new()
                } else {
                    it.name.clone().unwrap_or_default()
                },
                path: String::new(),
                folder,
                size: it.size.unwrap_or(0),
                c_tag: it.c_tag.clone(),
                e_tag: it.e_tag.clone(),
                quick_xor_hash: it.quick_xor_hash().map(str::to_string),
                modified: it.last_modified_date_time.clone(),
                content: if folder { None } else { prev_content },
            },
        );
    }

    // Rebuild paths. Items whose parent chain is broken (for example the
    // children of a deleted folder) are dropped.
    let ids: Vec<String> = items.keys().cloned().collect();
    let mut paths: HashMap<String, Option<String>> = HashMap::new();
    for id in &ids {
        let p = path_of(&items, id, &mut paths, 0);
        paths.insert(id.clone(), p);
    }
    items.retain(|id, e| match paths.get(id).cloned().flatten() {
        Some(p) => {
            e.path = p;
            true
        }
        None => false,
    });

    // Fetch content for new or changed files.
    let pending: Vec<String> = items
        .values()
        .filter(|e| !e.folder && e.content.is_none())
        .map(|e| e.id.clone())
        .collect();
    for id in pending {
        let entry = items.get(&id).expect("pending item").clone();
        let version = entry.c_tag.clone().filter(|v| !v.is_empty());
        let unchanged = previous
            .get(id.as_str())
            .filter(|p| version.is_some() && p.c_tag == version && p.content.is_some());
        let mut local_qx = None;
        let content = if let Some(p) = unchanged {
            run.stats.items_carried_forward += 1;
            local_qx = p.quick_xor_hash.clone();
            p.content.clone()
        } else if let Some(c) = version.as_deref().and_then(|v| run.cp.get(&id, v)) {
            run.stats.items_carried_forward += 1;
            Some(c.clone())
        } else {
            match run
                .fetch(&format!("/drives/{}/items/{id}/content", drive.id))
                .await
            {
                Ok(f) if f.content.size != entry.size => {
                    run.error(
                        &id,
                        format!(
                            "size mismatch: Graph reported {} bytes, downloaded {}",
                            entry.size, f.content.size
                        ),
                    );
                    None
                }
                Ok(f)
                    if entry
                        .quick_xor_hash
                        .as_ref()
                        .is_some_and(|h| *h != f.quick_xor) =>
                {
                    run.error(
                        &id,
                        "quickXorHash mismatch between Graph metadata and downloaded content",
                    );
                    None
                }
                Ok(f) => {
                    if let Some(v) = &version {
                        run.cp.record(&id, v, f.content.clone())?;
                    }
                    local_qx = Some(f.quick_xor);
                    Some(f.content)
                }
                Err(e)
                    if e.downcast_ref::<banksia_graph::GraphError>()
                        .is_some_and(|g| g.is_not_found()) =>
                {
                    // Deleted between the delta query and the download; the
                    // next delta reports the deletion.
                    items.remove(&id);
                    continue;
                }
                Err(e) => {
                    run.error(&id, format!("{e:#}"));
                    None
                }
            }
        };
        match content {
            Some(c) => {
                let e = items.get_mut(&id).expect("item");
                e.content = Some(c);
                // Keep a hash for restore checks even if Graph did not report one.
                if e.quick_xor_hash.is_none() {
                    e.quick_xor_hash = local_qx;
                }
            }
            None => match previous.get(id.as_str()).filter(|p| p.content.is_some()) {
                // Keep the last good version rather than losing the file.
                Some(p) => {
                    items.insert(
                        id.clone(),
                        DriveEntry {
                            path: items[&id].path.clone(),
                            ..(*p).clone()
                        },
                    );
                }
                None => {
                    items.remove(&id);
                }
            },
        }
    }

    // If any item failed, keep the previous delta link so the next run sees
    // those changes again. Unchanged items are matched by cTag, so this costs
    // metadata only.
    let delta_link = if run.errors.is_empty() {
        Some(link)
    } else {
        parent.and_then(|p| p.delta_link.clone())
    };
    run.stats.items = items.values().filter(|e| !e.folder).count() as u64;
    let mut list: Vec<DriveEntry> = items.into_values().collect();
    list.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(DriveSnapshot {
        drive_id: drive.id,
        delta_link,
        items: list,
    })
}

fn path_of(
    items: &BTreeMap<String, DriveEntry>,
    id: &str,
    memo: &mut HashMap<String, Option<String>>,
    depth: usize,
) -> Option<String> {
    if let Some(p) = memo.get(id) {
        return p.clone();
    }
    if depth > 512 {
        return None;
    }
    let e = items.get(id)?;
    let p = match &e.parent_id {
        None => Some(String::new()),
        Some(pid) => {
            let pp = path_of(items, pid, memo, depth + 1)?;
            Some(if pp.is_empty() {
                e.name.clone()
            } else {
                format!("{pp}/{}", e.name)
            })
        }
    };
    memo.insert(id.to_string(), p.clone());
    p
}
