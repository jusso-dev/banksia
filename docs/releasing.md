# Releasing Banksia

## Versioning

Banksia uses [semantic versioning](https://semver.org/). The public interface is:

- the `banksia` command line: commands, flags, JSON output and exit codes;
- the `banksia.toml` schema;
- the repository format (`format_version` in `repo info`);
- the evidence pack format.

Library crates in this workspace are internal (`publish = false`) and are not covered.

| Change | Bump |
| --- | --- |
| Removes or changes the meaning of anything above, or needs a repository migration | Major |
| Adds a command, flag, config key, workload or output field | Minor |
| Fixes a bug without changing the interface | Patch |

All crates share one version, set in `[workspace.package]` in the root `Cargo.toml`.

## Release checklist

1. Start from an up-to-date `main` on a `release/vX.Y.Z` branch.
2. Set `version` in the root `Cargo.toml` and run `cargo update --workspace` so `Cargo.lock` matches.
3. Move the `[Unreleased]` entries in `CHANGELOG.md` under a new `[X.Y.Z] - YYYY-MM-DD` heading and update the compare links.
4. Write `docs/releases/vX.Y.Z.md`: highlights, downloads, upgrade notes, permissions changes, known limitations and verification. Add version notes to [upgrading.md](upgrading.md) if anything changed.
5. Run the checks:

   ```sh
   cargo fmt --all --check
   cargo clippy --workspace --all-targets --locked -- -D warnings
   cargo test --workspace --locked
   cargo audit
   ```

6. Optionally run the [opt-in live test](testing.md#opt-in-live-test) against a throwaway tenant user, and record the result in the release notes.
7. Open a pull request, get it reviewed and merge it.
8. Tag the merge commit and push the tag:

   ```sh
   git switch main && git pull --ff-only
   git tag -a vX.Y.Z -m "Banksia X.Y.Z"
   git push origin vX.Y.Z
   ```

9. Build and publish the release (below).

## Building release artifacts

### On GitHub Actions

Pushing a `v*` tag runs `.github/workflows/release.yml` on GitHub-hosted runners. It runs the tests, builds every target, and creates a draft GitHub release with the archives, `SHA256SUMS` and the notes from `docs/releases/<tag>.md`. Check the draft, then publish it.

### Locally

`scripts/release.sh` builds the same archives on a macOS host. It needs `rustup` targets `aarch64-apple-darwin`, `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`, and [Zig](https://ziglang.org/) on the `PATH` for the Linux cross-compile:

```sh
scripts/release.sh v1.0.0            # builds into dist/v1.0.0/
scripts/release.sh v1.0.0 --publish  # also creates the GitHub release with gh
```

The script refuses to run if the tag and the `Cargo.toml` version differ or the working tree is dirty. `--publish` needs the tag to exist on `origin`.

Each archive is named `banksia-<tag>-<target>.tar.gz` and contains `banksia`, `README.md`, `LICENSE`, `CHANGELOG.md`, `banksia.example.toml` and `deploy/`. `SHA256SUMS` covers every archive.

## After the release

- Add a new empty `[Unreleased]` section to `CHANGELOG.md` if it is missing.
- Close or retarget any milestone.
