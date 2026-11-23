# Upgrading Banksia

## Compatibility promise

From 1.0.0, Banksia follows [semantic versioning](releasing.md#versioning). Within a major version:

- `banksia.toml` written for an earlier minor release keeps working. New keys are optional.
- Repositories keep format version 1 and stay readable and writable.
- Command names, flags, JSON output fields and exit codes are not removed or changed in meaning. New ones may be added.
- Evidence packs written by an earlier release still pass `banksia evidence verify`.

A change to any of these needs a new major version, an entry under "Upgrading" in the [changelog](../CHANGELOG.md), and a migration note in this file.

## Upgrade steps

1. Read the release notes in [`docs/releases/`](releases/) and the [changelog](../CHANGELOG.md).
2. Check the download against `SHA256SUMS`.
3. Wait for running backups to finish, or stop the timers: `systemctl stop banksia-backup.timer banksia-restore-test.timer`.
4. Replace `/usr/local/bin/banksia`. Keep the old binary until the new one is proven.
5. Run `banksia --version` and `banksia repo info`. `repo info` must show `"key": "ok"` and the expected `format_version`.
6. Run `banksia backup run --user <one user>` and `banksia restore-test run`, then start the timers again.

Downgrading within a major version is safe for the repository. Local state (checkpoints and journals) may be rebuilt. Banksia falls back to a full delta and tenant lookups when state is missing.

## Version notes

### 0.1.0 to 1.0.0

No changes to configuration, repository format or state. 1.0.0 reads repositories created by 0.1.0 source builds unchanged.
