# Banksia threat model

Status: v1.0, November 2026. Tracks issue #1. Method: assets, trust boundaries, then threats by STRIDE category with mitigations and open gaps.

## Assets

| Asset | Why it matters |
| --- | --- |
| Backed-up mail and files | Confidential personal and business data. |
| Master key | Decrypts every backup. |
| Backup app credential | Reads every in-scope mailbox and OneDrive. |
| Restore app credential | Writes into the tenant. |
| Repository integrity | A restore is only as good as the data it restores. |
| Evidence packs | Used to show auditors that restores work. |
| Tenant data after restore | A bad restore can confuse users or leak data into the wrong mailbox. |

## Actors

- **Operator.** Runs Banksia, prepares restore plans.
- **Approver.** Approves restore plans. May be the operator in small deployments.
- **Storage provider.** Hosts encrypted data. Trusted for availability, not for confidentiality or integrity.
- **Microsoft 365.** Source and restore target. Trusted.
- **External attacker.** May steal credentials, compromise the host or storage account, or tamper with network traffic.
- **Malicious insider.** May try to exfiltrate mail through restore, or destroy backups before an attack.
- **Ransomware operator.** Wants to delete or encrypt backups.

## Trust boundaries

1. Banksia host to Microsoft Graph (TLS, OAuth tokens).
2. Banksia host to storage (TLS, storage credentials, client-side encryption).
3. Operator to Banksia (configuration, plan approval).
4. Local disk on the Banksia host (key file, state directory, evidence output).

## Threats and mitigations

### Spoofing

| Threat | Mitigation |
| --- | --- |
| Attacker uses a stolen backup credential to read mail. | Certificates over secrets; Exchange RBAC for Applications to scope mailboxes; Entra sign-in logs for the app; credential rotation. Read access is inherent to backup. |
| Attacker uses a stolen restore credential to plant mail. | Separate restore app; keep its credential released only for approved restores; scope with RBAC for Applications. |
| Man in the middle on Graph or storage. | TLS with platform certificate verification, never disabled. |
| Bearer token leaks to a download host. | Redirects are followed manually and the token is only sent to the configured Graph host. |

### Tampering

| Threat | Mitigation |
| --- | --- |
| Storage provider or attacker modifies stored objects. | Authenticated encryption with the object path as associated data; blob ids are HMACs of content and are verified on restore. |
| Attacker swaps one user's snapshot for another's. | The path, including principal tag, is authenticated. Manifests carry the user's object id inside the ciphertext and restore checks it. |
| Restore plan altered after approval. | Approval is the plan's SHA-256 hash; apply recomputes and compares. |
| Evidence pack altered. | Manifest of SHA-256 hashes plus HMAC with the evidence key; copy stored in the immutable repository. |
| Rollback: attacker replays an older snapshot as latest. | Snapshot ids are time ordered and listed from immutable storage. Restore tests report snapshot age and fail when it exceeds the configured limit. |

### Repudiation

| Threat | Mitigation |
| --- | --- |
| Operator denies performing a restore. | Audit events in the immutable repository with operator, host, plan hash and results; Entra and Exchange audit logs for the restore app. |

### Information disclosure

| Threat | Mitigation |
| --- | --- |
| Storage provider reads backups. | Client-side encryption; key never leaves the host. |
| Object names reveal users or files. | Principal tags and blob ids are keyed HMACs; file names and paths live inside encrypted manifests. |
| Insider restores another user's mail into their own mailbox. | Restore disabled by default; allowlisted targets; hash approval, ideally by a second person; audit trail. |
| Secrets leak through logs or config files in git. | No inline secrets; redacted `Debug`; `.gitignore`; key file permission check. |
| Evidence packs leak personal data. | Packs hold principal tags, item counts, hashes and results; no bodies or file contents. Subjects are compared during verification but not written. |

### Denial of service

| Threat | Mitigation |
| --- | --- |
| Ransomware deletes backups with a stolen storage credential. | Banksia needs no delete permission; storage immutability and soft delete; separate storage account or bucket. |
| Graph throttling stops backups finishing. | Retry with `Retry-After`; bounded concurrency; checkpoints; run reports show duration. |
| Master key lost. | Documented offline key custody; restore tests fail loudly if the key is unavailable. |
| State directory lost. | Safe by design: falls back to full delta and tenant lookups. |

### Elevation of privilege

| Threat | Mitigation |
| --- | --- |
| Backup process used to write into the tenant. | Backup app has read-only permissions. |
| Malicious content in backed-up data exploits Banksia. | Content is treated as opaque bytes; only Graph JSON metadata is parsed; MIME parsing happens only during restore-test verification of restored items, with a memory-safe parser. |
| Compromised Banksia host. | Out of scope for prevention. Limit blast radius: separate apps, no delete permission on storage, immutability, KMS wrapping (planned). |

## Open gaps

- No KMS key wrapping or in-place key rotation.
- No built-in pruning; retention relies on storage policies.
- Approval is a hash supplied on the command line, not a signed approval from a separate identity. Signed approvals are a planned improvement.
- Restore idempotency relies on a local journal and tenant lookups; a restore marker extended property on messages is planned.
- MIME import through Graph v1.0 can mark restored mail as drafts (issue #10).
- Snapshot manifests are single documents and grow with mailbox size.
