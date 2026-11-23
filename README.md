# Banksia

[![CI](https://github.com/jusso-dev/banksia/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/jusso-dev/banksia/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/jusso-dev/banksia)](https://github.com/jusso-dev/banksia/releases/latest)
[![Licence](https://img.shields.io/badge/licence-Apache--2.0-blue)](LICENSE)

Self-hosted backup for Microsoft 365 and Entra ID, with restore back into the tenant and scheduled restore tests.

Banksia stores mailbox and OneDrive data (and later SharePoint, Teams and Entra ID objects) in storage you own, such as Azure Blob or Cloudflare R2 in Australia. Data is encrypted on your host with a key you hold. The product is the in-tenant restore and the restore-test evidence, not only a copy of the data.

Banksia covers Microsoft 365 and Entra ID. It does not back up cloud infrastructure, virtual machines, disks or agent workspaces.

## Status

Version 1.0. The command line, configuration file, repository format (version 1) and exit codes are stable and follow [semantic versioning](docs/releasing.md#versioning). Banksia 1.0 is tested against a fake tenant only. Run the [opt-in live test](docs/testing.md#opt-in-live-test) against your own tenant, and keep another backup, until you have proven restores in your environment. See [CHANGELOG.md](CHANGELOG.md) and the [1.0 release notes](docs/releases/v1.0.0.md).

| Area | 1.0 |
| --- | --- |
| Exchange Online mailboxes | Incremental backup; restore a message or folder, in place or to a new folder |
| OneDrive | Incremental, hash-verified backup; restore a file or folder, in place or to a new folder |
| Restore tests | Sampled restore to a throwaway mailbox and OneDrive, read back and verified, signed evidence pack |
| Storage | Azure Blob, Cloudflare R2 (S3 API), local directory; client-side encryption; write once |
| SharePoint, Teams, Entra ID | Designed, not built ([#9](https://github.com/jusso-dev/banksia/issues/9)) |

## How it works

```
banksia backup run          Graph (read-only app)  ->  encrypt  ->  your storage
banksia restore plan        your storage  ->  plan file + SHA-256 hash        (no tenant writes)
banksia restore apply       plan + approved hash  ->  Graph (restore app)     (additive, journalled, audited)
banksia restore-test run    sample  ->  throwaway target  ->  read back  ->  evidence pack
```

## Quick start

Install a release binary from [GitHub Releases](https://github.com/jusso-dev/banksia/releases). Builds are provided for Linux x86_64 and aarch64 (static, musl) and macOS on Apple silicon:

```sh
V=v1.0.0 T=x86_64-unknown-linux-musl
curl -LO https://github.com/jusso-dev/banksia/releases/download/$V/banksia-$V-$T.tar.gz
curl -LO https://github.com/jusso-dev/banksia/releases/download/$V/SHA256SUMS
shasum -a 256 --check --ignore-missing SHA256SUMS
tar -xzf banksia-$V-$T.tar.gz
install banksia-$V-$T/banksia /usr/local/bin/
banksia --version
```

On macOS, use `T=aarch64-apple-darwin`. The binary is not notarised, so after the checksum passes, run `xattr -d com.apple.quarantine banksia-$V-$T/banksia`. The Linux binaries are statically linked and need no system libraries.

Or build from source (Rust 1.98 or later):

```sh
cargo build --release --locked
install target/release/banksia /usr/local/bin/
```

Prepare the tenant (see [docs/security.md](docs/security.md) for the full permission matrix):

1. Register `banksia-backup` with application permissions `Mail.Read`, `Files.Read.All` and `User.ReadBasic.All`. Upload a certificate. Grant admin consent. Scope mail access with Exchange RBAC for Applications.
2. Register `banksia-restore` with `Mail.ReadWrite`, `Files.ReadWrite.All` and `User.ReadBasic.All`, scoped to the users you will restore into.
3. Create a throwaway user for restore tests, and optionally a third app registration (`restore_test.auth`) with the same permissions, scoped only to it.

Configure:

```sh
cp banksia.example.toml /etc/banksia/banksia.toml   # edit; never put secret values in it
banksia key generate --out /etc/banksia/master.key  # keep an offline copy
banksia repo init
```

Back up:

```sh
banksia backup run
banksia snapshots list
banksia snapshots show --user adele@contoso.onmicrosoft.com --workload mail --items
```

Restore a folder to a new location in the same mailbox:

```sh
banksia restore plan --user adele@contoso.onmicrosoft.com --workload mail \
  --folder "Inbox/Projects" --reason "INC-1234" --out plan.json
# review the summary and plan.json, then:
banksia restore apply --plan plan.json --approve <hash from plan> --dry-run
banksia restore apply --plan plan.json --approve <hash from plan>
```

Use `--message <id>`, `--item <id>` or `--path <path>` for single items, `--target` to restore into another allowlisted user, and `--in-place` to recreate the original structure. Restores never overwrite or delete.

Run a restore test and check its evidence:

```sh
banksia restore-test run
banksia evidence verify /var/lib/banksia/evidence/<run id>
```

Evidence packs are written to `restore_test.evidence_dir` (`evidence/` relative to the working directory if unset), or to `--evidence-dir`. The command prints the path it wrote.

Example systemd timers and a cron file are in [deploy/](deploy/). They back up every six hours and run a restore test every Monday at 03:30.

## Environment variables

| Variable | Purpose |
| --- | --- |
| `BANKSIA_CONFIG` | Path to `banksia.toml`, instead of `--config`. Default `banksia.toml`. |
| `BANKSIA_LOG` | Log filter, for example `info`, `debug` or `banksia_core=debug`. Logs go to stderr. Default `info`. |
| `BANKSIA_OPERATOR` | Name recorded in audit events. Falls back to `$USER`, then `unknown`. |
| `BANKSIA_HOST` | Host name recorded in audit events. Falls back to `$HOSTNAME`, then `unknown`; set it for systemd and cron, which usually leave `HOSTNAME` unset. |

Secrets are supplied through environment variables or files named in `banksia.toml`, never inline. See [docs/security.md](docs/security.md#secrets-handling).

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Error (configuration, authentication, refused restore, and so on) |
| 2 | Backup or restore finished with failed items |
| 3 | Restore test ran and failed |

## Documentation

- [Architecture](docs/architecture.md)
- [Security](docs/security.md): permissions per operation, encryption, immutability, retention and restore authorisation
- [Threat model](docs/threat-model.md)
- [Testing](docs/testing.md), including the opt-in live test
- [Upgrading](docs/upgrading.md)
- [Releasing](docs/releasing.md)
- [Changelog](CHANGELOG.md)

## Reporting a vulnerability

Report security issues privately to the repository owner through GitHub. Do not open a public issue with exploit details. See [docs/security.md](docs/security.md#reporting-a-vulnerability).

## Licence

Copyright 2026 jusso-dev.

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
