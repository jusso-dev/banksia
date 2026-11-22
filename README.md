# Banksia

Self-hosted backup for Microsoft 365 and Entra ID, with restore back into the tenant and scheduled restore tests.

Banksia stores mailbox and OneDrive data (and later SharePoint, Teams and Entra ID objects) in storage you own, such as Azure Blob or Cloudflare R2 in Australia. Data is encrypted on your host with a key you hold. The product is the in-tenant restore and the restore-test evidence, not only a copy of the data.

Banksia covers Microsoft 365 and Entra ID. It does not back up cloud infrastructure, virtual machines, disks or agent workspaces.

## Status

MVP in development. Do not use this in production.

| Area | MVP |
| --- | --- |
| Exchange Online mailboxes | Backup, item and folder restore |
| OneDrive | Backup, file and folder restore |
| Restore tests and evidence packs | Yes |
| SharePoint, Teams, Entra ID | Designed, not built |

## Documentation

- [Architecture](docs/architecture.md)
- [Security](docs/security.md): permissions per operation, encryption, immutability, retention and restore authorisation
- [Threat model](docs/threat-model.md)
- [Testing](docs/testing.md)

## Licence

Copyright 2026 jusso-dev.

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
