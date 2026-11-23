#!/usr/bin/env bash
# Build Banksia release archives on a macOS host.
#
#   scripts/release.sh vX.Y.Z [--publish]
#
# Builds aarch64-apple-darwin natively and the Linux musl targets with Zig as
# the C compiler and linker, then writes archives and SHA256SUMS to
# dist/<tag>/. With --publish, creates a draft GitHub release for the tag
# using gh, with notes from docs/releases/<tag>.md.
set -euo pipefail

tag="${1:-}"
publish="${2:-}"
[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "usage: $0 vX.Y.Z [--publish]" >&2; exit 1; }
[[ -z "$publish" || "$publish" == "--publish" ]] || { echo "unknown option: $publish" >&2; exit 1; }

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)"
[[ "v$version" == "$tag" ]] || { echo "tag $tag does not match Cargo.toml version $version" >&2; exit 1; }
[[ -z "$(git status --porcelain)" ]] || { echo "working tree is dirty" >&2; exit 1; }
notes="docs/releases/$tag.md"
[[ -f "$notes" ]] || { echo "missing $notes" >&2; exit 1; }
command -v zig >/dev/null || { echo "zig is required for the Linux targets" >&2; exit 1; }

targets=(aarch64-apple-darwin x86_64-unknown-linux-musl aarch64-unknown-linux-musl)
dist="$root/dist/$tag"
rm -rf "$dist"
mkdir -p "$dist"

# Zig wrappers. cc-rs passes --target=<rust triple>, which Zig cannot parse,
# and rustc passes an aarch64 erratum flag that Zig's linker rejects.
shim="$(mktemp -d)"
trap 'rm -rf "$shim"' EXIT
for zt in x86_64-linux-musl aarch64-linux-musl; do
  for tool in cc c++; do
    cat >"$shim/$tool-$zt" <<EOF
#!/usr/bin/env bash
out=()
for a in "\$@"; do
  case "\$a" in --target=*|-Wl,--fix-cortex-a53-843419|--fix-cortex-a53-843419) ;; *) out+=("\$a") ;; esac
done
exec zig $tool -target $zt "\${out[@]}"
EOF
    chmod +x "$shim/$tool-$zt"
  done
done
printf '#!/bin/sh\nexec zig ar "$@"\n' >"$shim/ar"
chmod +x "$shim/ar"

cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked

for target in "${targets[@]}"; do
  echo "==> $target"
  rustup target add "$target" >/dev/null
  # Start non-empty: macOS bash 3.2 treats an empty array as unset under -u.
  envs=("BANKSIA_RELEASE_TARGET=$target")
  if [[ "$target" == *-linux-musl ]]; then
    zt="${target%%-*}-linux-musl"
    t="${target//-/_}"
    tu="$(echo "$t" | tr '[:lower:]' '[:upper:]')"
    envs+=(
      "CC_$t=$shim/cc-$zt"
      "CXX_$t=$shim/c++-$zt"
      "AR_$t=$shim/ar"
      "CARGO_TARGET_${tu}_LINKER=$shim/cc-$zt"
      # Zig supplies the musl start files; Rust's copies would collide.
      "CARGO_TARGET_${tu}_RUSTFLAGS=-C link-self-contained=no"
    )
  fi
  env "${envs[@]}" cargo build --release --locked --target "$target" -p banksia-cli

  name="banksia-$tag-$target"
  stage="$dist/$name"
  mkdir -p "$stage"
  cp "target/$target/release/banksia" "$stage/"
  cp README.md LICENSE CHANGELOG.md banksia.example.toml "$stage/"
  cp -R deploy "$stage/"
  tar -C "$dist" -czf "$dist/$name.tar.gz" "$name"
  rm -rf "$stage"
done

native="$dist/check"
mkdir -p "$native"
tar -C "$native" -xzf "$dist/banksia-$tag-aarch64-apple-darwin.tar.gz"
got="$("$native/banksia-$tag-aarch64-apple-darwin/banksia" --version)"
rm -rf "$native"
[[ "$got" == "banksia $version" ]] || { echo "unexpected version output: $got" >&2; exit 1; }

(cd "$dist" && shasum -a 256 ./*.tar.gz | sed 's| \./| |' >SHA256SUMS)
echo "==> built $dist"
cat "$dist/SHA256SUMS"

if [[ "$publish" == "--publish" ]]; then
  git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null \
    || { echo "push the tag first: git push origin $tag" >&2; exit 1; }
  gh release create "$tag" --verify-tag --draft --title "Banksia ${tag#v}" \
    --notes-file "$notes" "$dist"/*.tar.gz "$dist/SHA256SUMS"
  echo "==> draft release created; review it on GitHub, then publish"
fi
