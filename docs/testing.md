# Testing Banksia

## Principles

- CI never touches a real tenant or real storage account.
- Every Graph behaviour Banksia depends on (auth, paging, delta, throttling, redirects, uploads) is exercised against `banksia-fakegraph`, an in-process fake tenant.
- Storage is tested with in-memory and local-directory stores. The Azure and S3 backends are thin configuration over the `object_store` crate.
- Fixtures are synthetic. No real mail, files or identifiers are committed.

## Running tests

```sh
cargo test --workspace
```

## The fake tenant

`banksia-fakegraph` serves the token endpoint and the Graph v1.0 paths Banksia uses, backed by in-memory users, mail folders, messages and drive items. Tests can:

- seed users, folders, messages and files;
- change data between backup runs to exercise delta;
- inject 429 and 503 responses with `Retry-After`;
- expire delta tokens (HTTP 410);
- corrupt content or hashes to test verification failures.

Downloads redirect to a separate path, and the fake rejects any download request that carries an `Authorization` header, so tests prove the bearer token is not forwarded.

## Opt-in live test

A live restore test against a real tenant is never run in CI. It is documented here for operators who want to prove Banksia against their own tenant before relying on it.

Requirements:

- A throwaway test mailbox and its OneDrive, containing no real data.
- Separate backup and restore-test app registrations scoped to that user only (see [security.md](security.md)).
- A throwaway storage container or bucket.

Run it explicitly:

```sh
export BANKSIA_LIVE_TEST=1
export BANKSIA_LIVE_THROWAWAY_USER=restore-test@example.onmicrosoft.com
banksia --config banksia.live.toml backup run
banksia --config banksia.live.toml restore-test run
```

`restore-test run` only writes to the configured `restore_test` targets, and refuses to run if those targets are not also listed in `restore.allowed_targets`.
