# Banksia security

Status: MVP design, November 2026. Tracks issues #1, #4 and #7. See also [threat-model.md](threat-model.md).

## Summary

- Backup and restore use **separate Entra app registrations** with application permissions. The backup app can only read.
- Data is **encrypted on the Banksia host** before it reaches storage, with a key the customer holds.
- The repository is **write once**. Banksia never overwrites or deletes stored objects, so storage immutability can be turned on.
- Restores are **planned, approved by hash, allowlisted, journalled and audited**. They only add data to the tenant.
- No secrets in configuration files, logs, the repository or error messages.

## Identities and consent

Create two app registrations in the tenant. A third, for restore tests, is recommended.

| App | Used by | Purpose |
| --- | --- | --- |
| `banksia-backup` | `banksia backup run` | Read mail and files. |
| `banksia-restore` | `banksia restore apply` | Write mail and files into approved targets. |
| `banksia-restore-test` | `banksia restore-test run` | Write and read back, limited to throwaway targets. |

Each app needs application permissions and a tenant admin to grant consent. Use certificate credentials. Client secrets are supported for evaluation only.

### Graph permissions per operation

| Operation | Permission (application) | Why | How to narrow |
| --- | --- | --- | --- |
| Resolve users | `User.ReadBasic.All` | Map a UPN or object id to the user and check it still exists. Needed by backup, restore plan, restore apply and restore tests. | None in the MVP: Graph needs this permission to read `/users/{id}` even when `{id}` is an object id. |
| Mailbox backup | `Mail.Read` | Folders, message delta, MIME. | Exchange RBAC for Applications: assign `Application Mail.Read` with a management scope covering only backed-up mailboxes. |
| OneDrive backup | `Files.Read.All` | Drive delta and content. | `Sites.Selected` granted per OneDrive site, where your tenant's process allows it. |
| Mail restore | `Mail.ReadWrite` | Create folders and import MIME. | Exchange RBAC for Applications: `Application Mail.ReadWrite` scoped to the mailboxes you allow restores into. |
| OneDrive restore | `Files.ReadWrite.All` | Create folders, upload files. | `Sites.Selected` with write on approved OneDrive sites. |
| Restore test | `Mail.ReadWrite`, `Files.ReadWrite.All` | Restore to and read back from safe targets. | Scope both to the throwaway mailbox and OneDrive only. |

Tokens are requested with scope `https://graph.microsoft.com/.default`, so the token carries exactly what was consented. Banksia does not request delegated permissions.

Exchange RBAC for Applications replaces application access policies. Example, for the backup app:

```powershell
New-ManagementScope -Name "Banksia backup" -RecipientRestrictionFilter "MemberOfGroup -eq 'CN=Banksia Backup,...'"
New-ManagementRoleAssignment -App <backup app id> -Role "Application Mail.Read" -CustomResourceScope "Banksia backup"
```

If you scope with RBAC for Applications, do not also grant the unscoped Entra `Mail.Read` permission, as Entra grants apply tenant wide.

### Storage permissions

| Backend | Backup and restore-test | Restore and plan (plan also writes an audit event) | Never needed |
| --- | --- | --- | --- |
| Azure Blob | Read, list, create (for example `Storage Blob Data Contributor` on the container). | Read and list (`Storage Blob Data Reader`), plus create for audit events. | Delete, account keys. |
| Cloudflare R2 | API token with object read and write on the bucket only. | Same token or a read-only token plus audit writes. | Account-wide tokens, admin tokens. |
| Local | A directory writable by the Banksia service user only. | Same. | |

Prefer managed identity or workload identity for Azure Blob where Banksia runs in Azure. Banksia never needs permission to delete objects. If the identity cannot delete, a stolen credential cannot destroy backups.

## Secrets handling

- `banksia.toml` names where secrets live (`*_env` for an environment variable, `*_file` for a file path). Inline secret values are rejected at load time.
- Supported secret sources: environment variables (for example from systemd `LoadCredential`, Kubernetes secrets or a vault agent) and files with restrictive permissions. On Unix, Banksia refuses a key file that is readable by group or others.
- Secrets are held in zeroising wrappers and redacted in `Debug` output. Tokens are never logged. Error messages from Graph and storage are passed through without request headers.
- `.gitignore` excludes `banksia.toml`, `*.key`, `*.pem`, `*.pfx` and the state directory.

## Encryption

- **Algorithm.** XChaCha20-Poly1305 (authenticated encryption) with a random 192-bit nonce per object. Random nonces at this size are safe for the object counts a backup repository will hold.
- **Associated data.** The object's repository path is authenticated with the ciphertext, so objects cannot be swapped or renamed without detection.
- **Key hierarchy.** A 256-bit master key is generated by `banksia key generate`. HKDF-SHA256 derives independent keys for data encryption, object identifiers (HMAC) and evidence signing (HMAC).
- **Customer held.** The master key exists only on the Banksia host (key file or environment). It is never sent to Microsoft or the storage provider.
- **Integrity.** Every read is authenticated. Snapshot manifests list blob ids, and blob ids are HMACs of plaintext, so restore verifies content end to end.
- **In transit.** TLS for Graph and storage, with certificate verification always on (rustls with the platform verifier).

### Key loss and rotation

- **Loss.** Losing the master key makes the repository unreadable. This is the cost of customer-held keys. Keep an offline copy (for example a sealed printout or a hardware-backed secret store) under your normal key-custody process.
- **Rotation.** The MVP does not rotate keys in place. Rotate by creating a new repository with a new key and keeping the old one, read-only, until its retention ends.
- **Gap.** Key wrapping with Azure Key Vault or another KMS is planned, not built.

## Immutability and retention

Banksia's repository layout is write once by design: content blobs are named by keyed content hash, snapshots and audit events have unique names, and every write is a create-only conditional put. Banksia never deletes. This makes it safe to enable provider immutability:

- **Azure Blob.** Use a container-level or version-level time-based retention policy (immutable storage for Azure Blob) and lock it once tested. Enable soft delete for containers as a further guard.
- **Cloudflare R2.** Use bucket lock rules with a retention period on the repository prefix. Choose the Oceania (`oc`) location hint or an Australian jurisdiction where offered.
- **Local.** Use a filesystem or appliance with snapshots or WORM support.

Retention is applied by storage, not by Banksia, in the MVP. Pruning old snapshots needs a careful design (deleting unreferenced blobs is unsafe while backups run) and is not built. A retention period should be set so that storage cost and recovery needs balance. Banksia reports snapshot ages in restore-test evidence.

## Restore authorisation

Writes into the tenant follow these rules:

1. **Disabled by default.** `restore.enabled` must be `true` in configuration.
2. **Allowlist.** The target user must be listed in `restore.allowed_targets`. Restore tests may only write to `restore_test.mail_target` and `restore_test.drive_target`.
3. **Plan, then approve.** `restore plan` writes a plan and prints its SHA-256 hash. `restore apply` requires `--approve` with the full 64-character hash. Prefixes are not accepted, because a short prefix can be matched by a different plan with enough computing effort. Any change to the plan changes the hash. This makes approval an explicit act and lets a second person approve a plan prepared by someone else.
4. **Revalidation.** Apply re-resolves the target user and refuses if its object id differs from the plan.
5. **Additive only.** Restore never overwrites or deletes tenant data.
6. **Journal and audit.** Every action is journalled locally, and audit events (plan created, apply started, apply finished with counts and failures, operator and host) are written append-only to the repository.
7. **Separate credentials.** Restore uses a different app registration from backup.
8. **Dry run.** `restore apply --dry-run` performs every check without writing.

For two-person control, keep the restore app's certificate in a secret store that only approvers can release, and have the operator prepare the plan without it.

## Live tenant testing

CI never touches a real tenant. All tests run against `banksia-fakegraph`, a local fake of the Graph endpoints Banksia uses. The opt-in live test (see [testing.md](testing.md)) requires explicit environment variables, a throwaway mailbox and a throwaway OneDrive, and refuses to run otherwise.

## Reporting a vulnerability

Contact the repository owner privately through GitHub. Do not open a public issue with exploit details.
