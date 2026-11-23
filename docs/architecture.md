# Banksia architecture

Status: MVP design, November 2026. Tracks issue #1.

Banksia backs up Microsoft 365 and Entra ID data into storage the customer owns, restores it back into the tenant, and proves on a schedule that restores work. Banksia covers Microsoft 365 and Entra ID. It does not back up cloud infrastructure, virtual machines, disks or agent workspaces.

The MVP covers Exchange Online mailboxes and OneDrive. SharePoint, Teams and Entra ID objects have reserved places in the design (see [Extension points](#extension-points)) but no code yet.

## Goals

1. Point-in-time backup of mailboxes and OneDrive with Microsoft Graph, unattended, using application permissions.
2. Storage the customer owns and controls: Azure Blob, Cloudflare R2 (S3 API) or a local directory.
3. Client-side encryption with a key the customer holds. The storage provider never sees plaintext.
4. Write-once repository layout, so the repository can sit behind storage immutability (WORM) policies.
5. Item-level restore into the tenant through a reviewable plan, an explicit approval and an audited apply step.
6. Scheduled restore tests into a safe location, verified by reading the data back, with a tamper-evident evidence pack.

## Non-goals

- A general GRC, SIEM or posture product.
- Backing up Azure or AWS resources, VMs or disks.
- Microsoft 365 Backup Storage as the only engine. It may become an optional backend later.

## Decision: language and layout

Banksia is written in **Rust** as a Cargo workspace.

Reasons:

- A single static binary is easy to run on a customer host, in a container or on a schedule, with no runtime to patch.
- Memory safety and strong typing suit code that handles credentials, keys and untrusted HTTP responses.
- Mature crates for the parts that matter: `object_store` (Azure Blob, S3 and local storage, with create-only conditional writes), `reqwest` with rustls, `aws-lc-rs` and RustCrypto for cryptography, `axum` for the local fake tenant used in tests.
- .NET would give first-party Graph SDKs, but the Graph surface Banksia needs is small (about fifteen endpoints) and the generated SDKs add size without solving throttling, checkpointing or restore idempotency, which are the hard parts.

Layout:

| Crate | Purpose |
| --- | --- |
| `crates/banksia-graph` | Narrow Microsoft Graph client: app-only auth, retry and throttling, paging, delta, streaming downloads, uploads. |
| `crates/banksia-core` | Configuration, crypto, repository, workloads (mail, drive), restore planner and executor, restore tests, evidence packs, audit. |
| `crates/banksia-cli` | The `banksia` binary. |
| `crates/banksia-fakegraph` | An in-process fake tenant (HTTP server) used by tests. Never shipped. |

### Decision: no dependency on Junction for the MVP

jusso-dev/junction already speaks Microsoft Graph in Rust. Banksia does not depend on it for the MVP:

- Junction's auth crate is built for interactive and developer flows and pulls in OS keychain integration that an unattended backup service does not need.
- Junction pins `reqwest` 0.12 while `object_store` uses 0.13. Depending on both would ship two HTTP and TLS stacks.
- Banksia's client needs backup-specific behaviour: manual redirect handling so a bearer token is never sent to a download host, per-mailbox concurrency limits, and distinct errors for expired delta state.
- Junction moves on its own release train. Banksia's restore path should not change because a generated endpoint catalogue did.

Revisit if Junction publishes a small, stable, app-only auth crate on `reqwest` 0.13.

## Components and data flow

```
                     +-------------------------- customer host ---------------------------+
 Microsoft Graph     |                                                                    |
 (tenant)            |  banksia backup run                                                |
   ^  |              |    graph client --> workload (mail, drive) --> chunk, hash,        |
   |  | read         |                                                encrypt --> store --+--> Azure Blob / R2 / local
   |  +--------------+                                                                    |    (customer owned,
   |                 |  banksia restore plan     (read repository only, writes a plan)    |     immutable)
   | write           |  banksia restore apply    (plan hash approval, journal, audit)     |
   +-----------------+  banksia restore-test run (safe targets, read-back verification,   |
                     |                            evidence pack)                          |
                     |  local: banksia.toml, key file, state dir (journals, checkpoints)  |
                     +--------------------------------------------------------------------+
```

Each command is a short-lived process. Scheduling is done by systemd timers, cron, Kubernetes CronJobs or a container scheduler (examples in `deploy/`). There is no long-running daemon or web console in the MVP.

## Repository format

A repository is a prefix in a bucket or container (or a local directory). Format version 1:

```
banksia.repo.json                                  plaintext: format, repo id, created, key check
blobs/<2 hex>/<64 hex>                             encrypted content chunks, content addressed
principals/<principal tag>                         encrypted object id and UPN
snapshots/<principal tag>/<workload>/<snapshot id> encrypted snapshot manifest (JSON)
audit/<yyyy>/<mm>/<event id>                       encrypted audit events
evidence/<run id>/<file>                           encrypted copy of each evidence pack
```

- **Master key.** 32 random bytes held by the customer in a key file or environment variable. Never written to the repository.
- **Derived keys.** HKDF-SHA256 derives separate keys for data encryption, object identifiers and evidence signing.
- **Encryption.** XChaCha20-Poly1305 with a random 192-bit nonce per object. The object path is bound in as associated data, so a ciphertext cannot be swapped to another name without detection.
- **Blob ids.** HMAC-SHA256 of the plaintext chunk under the identifier key. Identical chunks deduplicate. Storage cannot test whether a known file is present, which plain SHA-256 names would allow.
- **Principal tags.** HMAC of the user's Entra object id, truncated to 128 bits. Object names do not reveal who a backup belongs to.
- **Key check.** `banksia.repo.json` holds an HMAC of a fixed label. A wrong key is rejected before anything is written.
- **Snapshot ids** are UTC timestamps with a random suffix, so they sort by time.
- **Principals.** `principals/<tag>` holds the encrypted object id and UPN for each backed-up user, so `--user` lookups work without exposing names in object paths.
- **Chunking.** Content is split into fixed 5 MiB chunks (16 x 320 KiB, which also suits OneDrive upload sessions). Content-defined chunking is a later improvement.
- **Write once.** Every write uses a create-only conditional put (`If-None-Match: *` or the local equivalent). An object that already exists is treated as already written, which is always correct because names are derived from content or are unique. Banksia never overwrites or deletes repository objects. Retention is enforced by storage lifecycle and immutability policies (see [security.md](security.md)).

### Snapshot manifest

Each snapshot is a complete point-in-time view of one workload for one user. Unchanged items are carried forward from the parent snapshot by reference, so restoring from any snapshot needs only that manifest and the blobs it names. The manifest also holds the Graph delta links needed for the next incremental run.

Known limit: manifests are a single JSON document. A mailbox with several hundred thousand items produces a manifest of tens of megabytes. Sharded manifests are a planned improvement.

## Backup flow

### Common behaviour

- **Auth.** OAuth 2.0 client credentials with a certificate (preferred) or secret, against the tenant's authority. Tokens are cached in memory and refreshed before expiry. A 401 forces one refresh and retry.
- **Throttling.** 429, 503 and 504 are retried. `Retry-After` is honoured. Otherwise capped exponential backoff with jitter. The retry budget is configurable. Concurrency is bounded per process.
- **Paging.** `@odata.nextLink` is followed until a `@odata.deltaLink` is returned.
- **Expired delta.** HTTP 410 or `syncStateNotFound` triggers a full resync of that folder or drive. Unchanged items are still matched against the parent snapshot by id and change marker, so content is not downloaded again.
- **Checkpointing.** Content blobs are committed as they are written. A local checkpoint in the state directory (encrypted with the data key) records items committed during an unfinished run. A rerun skips those downloads. The snapshot manifest is written last, so a snapshot either exists completely or not at all.
- **Partial results.** An item that fails (for example a hash mismatch) is recorded in the manifest's error list, the previous version is carried forward if one exists, and the snapshot is marked partial. The command exits non-zero.

### Exchange Online mailboxes

1. Resolve the user (`GET /users/{upn}`), keeping the object id as the stable key.
2. Enumerate the folder tree including hidden folders.
3. For each folder, run message delta from the stored delta link (or from scratch).
4. For new or changed messages (by `changeKey`), download MIME from `/messages/{id}/$value`, chunk, encrypt and store.
5. Record per-message metadata: folder, subject, sender, received time, `internetMessageId`, size and content hash.

MIME is used because it keeps headers, bodies and attachments together and is accepted by Graph v1.0 for import. Calendar, contacts, archive mailboxes and mailbox permissions are follow-ups.

### OneDrive

1. Resolve the user's default drive.
2. Run root delta from the stored delta link (or from scratch).
3. Rebuild full paths from parent references after applying the delta.
4. For new or changed files (by `cTag`), stream `/items/{id}/content`, follow the redirect to the pre-authenticated download URL without the bearer token, chunk, encrypt and store.
5. Verify size and Microsoft's `quickXorHash` for every file. Record SHA-256 as well for Banksia's own verification.

File versions, sharing permissions and OneNote notebooks are follow-ups.

### Backup window

Graph throttling sets the pace, not bandwidth. The first full backup of a large tenant can take days. Incrementals are usually minutes per user. Banksia reports per-run duration and item counts so operators can see whether runs fit the window. Exchange throttles per mailbox, so concurrency across mailboxes helps far more than concurrency within one.

## Restore model

Restores write into the tenant, which is the riskiest operation Banksia performs. The model is plan, approve, apply.

1. **Plan** (`banksia restore plan`). Reads the repository, resolves the target user with the read-only backup app, and writes a `restore.plan_created` audit event. Selects a snapshot and items (a message, a folder, a file or a drive folder), a target user and a target location. Writes a plan file and prints a summary and the plan's SHA-256 hash. No tenant writes. Planning is the dry run.
2. **Approve.** A person reviews the plan and supplies the full 64-character hash to apply. The plan hash binds the approval to exact content: any edit changes the hash.
3. **Apply** (`banksia restore apply --plan p.json --approve <hash>`). Checks that restore is enabled, the target is on the allowlist, the target object id still matches the plan, and the repository matches. Writes an audit event, then executes actions in order, recording each completed action in a local journal. `--dry-run` performs every check and read-only lookup, then reports what would happen.

Rules:

- **Additive only.** Restore creates messages, folders and files. It never deletes or overwrites tenant data. The default target is a new folder, `Banksia Restore/<plan id>`, in the target mailbox or OneDrive. In-place restore recreates the original folder path and uses OneDrive's `rename` conflict behaviour.
- **Idempotent.** Before each write, Banksia checks the journal and the tenant: messages by `internetMessageId` in the target folder, files by path, size and `quickXorHash`. A rerun after interruption skips completed work.
- **Separate identity.** Restore uses a different app registration from backup, so the backup credential cannot write to the tenant.

Known gap: messages imported from MIME through Graph v1.0 can appear as drafts in Outlook. Tracked in issue #10.

## Restore tests and evidence

`banksia restore-test run`:

1. Loads the latest snapshot for each configured user and workload and checks its age against `max_snapshot_age_hours`. A configured user or workload with no snapshot, or whose latest snapshot is partial, fails the check.
2. Picks a deterministic sample using a recorded seed.
3. Builds a restore plan to the configured safe target (a throwaway mailbox and OneDrive) and applies it with the normal executor, under a restore-test policy that only permits the safe targets. This works even when `restore.enabled` is false, because the policy cannot reach any other user.
4. Reads every restored item back from the tenant and verifies it: files by size, `quickXorHash` and SHA-256 of the downloaded content; messages by subject, `internetMessageId`, SHA-256 of each text body (after normalising line endings) and SHA-256 of each attachment. A restored message that cannot be parsed fails.
5. Writes an evidence pack: `evidence.json`, `report.md`, `manifest.json` (SHA-256 of each file) and `manifest.hmac` (HMAC-SHA256 over the manifest with the evidence key). A copy goes to the repository. `banksia evidence verify <dir>` checks it.

Evidence records what was tested, when, from which snapshot, the snapshot age, time to restore and per-item results. It does not contain message bodies or file contents, and identifies source users by principal tag.

## Configuration and state

- `banksia.toml` (gitignored, `banksia.example.toml` committed). Unknown keys are rejected. Secrets are never accepted inline: the file names environment variables or files that hold them.
- State directory (default `./.banksia-state`): checkpoints and restore journals. Losing it is safe; Banksia falls back to tenant lookups for idempotency and to full delta for backup.

## Extension points

Workloads share the repository, snapshot, restore plan and evidence formats. Adding one means adding a backup module, item kinds and restore actions.

- **Entra ID objects** (issue #9). Workload `entra`, principal tag derived from the tenant id. Items are JSON documents per object (users, groups, applications, service principals, conditional access policies) with relationship lists. Restore actions will be restore-from-soft-delete (`/directory/deletedItems/{id}/restore`) and attribute-level patch with a diff shown in the plan. Expect stricter approval than mail or files.
- **SharePoint sites.** Workload `site`, reusing the drive engine per document library.
- **Teams.** Files first (they live in SharePoint), then channel messages. Chat restore through Graph is limited and needs its own design.
- **Microsoft 365 Backup Storage** as an optional engine for very large tenants, behind the same restore plan and evidence model.
- **Key wrapping** with Azure Key Vault or another KMS.
