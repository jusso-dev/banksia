# Changelog

All notable changes to Banksia are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and Banksia uses [semantic versioning](docs/releasing.md#versioning).

## [Unreleased]

### Changed

- README: status badges, macOS install note, evidence pack location, environment variables and vulnerability reporting.

## [1.0.0] - 2026-11-23

First stable release. Full notes: [docs/releases/v1.0.0.md](docs/releases/v1.0.0.md).

### Added

- `banksia` command line: `key generate`, `repo init`, `repo info`, `backup run`, `snapshots list`, `snapshots show`, `restore plan`, `restore apply`, `restore-test run`, `evidence verify` and `audit list`.
- Encrypted, write-once repository (format version 1) on Azure Blob, Cloudflare R2 or any S3-compatible store, or a local directory. XChaCha20-Poly1305 with keys derived from a customer-held master key.
- Microsoft Graph client: app-only auth with a certificate (PS256 assertion) or a secret, token caching, retry with `Retry-After` on 429 and 5xx, delta paging, resumable upload sessions, and redirect handling that never forwards the bearer token to download hosts.
- Exchange Online mailbox backup: full folder tree including hidden folders, per-folder message delta with immutable ids, MIME content, and a full resync when delta state expires.
- OneDrive backup: root delta, streamed chunked downloads checked against size and quickXorHash, and content reuse by cTag.
- Incremental snapshots carried forward from their parent. Failed items keep the last good version and mark the snapshot partial, so the next run retries them.
- Item-level restore of messages, mail folders, files, drive folders or a whole snapshot. Restores go to `Banksia Restore/<plan id>` by default or recreate the original structure with `--in-place`. They are additive only, never overwrite and are idempotent.
- Restore authorisation: a separate restore app, `restore.enabled`, a target allowlist, approval by the full SHA-256 plan hash, plans checked against their source snapshot, `--dry-run`, an encrypted journal and append-only audit events.
- Restore tests: a seeded sample restored to throwaway mail and OneDrive targets, read back and compared with the backup, and a signed evidence pack (`evidence.json`, `report.md`, `manifest.json`, `manifest.hmac`) that contains no message or file content.
- Exit codes: 0 success, 1 error, 2 finished with failed items, 3 restore test failed.
- systemd unit and timer examples and a cron example in `deploy/`.
- Architecture, security, threat model, testing, upgrading and release documents.
- Release builds for Linux x86_64 and aarch64 (static, musl) and macOS on Apple silicon, with SHA-256 checksums.

### Known limitations

- Tested against the in-process fake tenant only. No production tenant run is part of this release.
- Restored mail is imported as MIME through Graph v1.0. Exchange can show it as a draft, and categories, flags and read state are not restored ([#10](https://github.com/jusso-dev/banksia/issues/10)).
- No built-in pruning. Retention is set with storage policies.
- No in-place key rotation and no KMS key wrapping.
- SharePoint, Teams and Entra ID are designed, not built ([#9](https://github.com/jusso-dev/banksia/issues/9)).

[Unreleased]: https://github.com/jusso-dev/banksia/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/jusso-dev/banksia/releases/tag/v1.0.0
