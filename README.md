# Banksia

Self-hosted backup for Microsoft 365 and Entra ID, with restore back into the tenant and scheduled restore tests.

Banksia stores mailbox and OneDrive data (and later SharePoint, Teams and Entra ID objects) in storage you own, such as Azure Blob or Cloudflare R2 in Australia. Data is encrypted on your host with a key you hold. The product is the in-tenant restore and the restore-test evidence, not only a copy of the data.

Banksia covers Microsoft 365 and Entra ID. It does not back up cloud infrastructure, virtual machines, disks or agent workspaces.

## Status

MVP. Not yet proven against a production tenant. Do not rely on it as your only backup yet.

| Area | MVP |
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

Build:

```sh
cargo build --release
install target/release/banksia /usr/local/bin/
```

Prepare the tenant (see [docs/security.md](docs/security.md) for the full permission matrix):

1. Register `banksia-backup` with application permissions `Mail.Read`, `Files.Read.All` and `User.ReadBasic.All`. Upload a certificate. Grant admin consent. Scope mail access with Exchange RBAC for Applications.
2. Register `banksia-restore` with `Mail.ReadWrite` and `Files.ReadWrite.All`, scoped to the users you will restore into.
3. Create a throwaway user for restore tests, and optionally a third app registration scoped only to it.

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
banksia evidence verify evidence/<run id>
```

Example schedules are in [deploy/](deploy/).

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

## Licence

Copyright 2026 jusso-dev.

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
