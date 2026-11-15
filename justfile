# Default recipe: list available commands
default:
    @just --list

# Format all code (Rust + Nix + Markdown)
fmt:
    treefmt

# Check formatting (Rust + Nix + Markdown)
fmt-check:
    treefmt --fail-on-change --no-cache

# Run clippy lints
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Find dead Nix code
lint-nix:
    deadnix --fail nix/ flake.nix

# Run all tests
test:
    cargo test --all-features

# Run tests with verbose output
test-verbose:
    cargo test --all-features -- --nocapture

# Build release
build:
    cargo build --release --all-features

# Generate documentation
doc *args='':
    cargo doc --no-deps --all-features {{args}}

readme_args := "--project-root crates/diskfoot --input crates/diskfoot/src/main.rs --template README.tpl"

# Regenerate README.md from README.tpl and the CLI docs, then normalise it
readme:
    cargo readme {{readme_args}} | mdformat - > README.md

# Check README.md is in sync with README.tpl and the CLI docs
readme-check:
    cargo readme {{readme_args}} | mdformat - | diff - README.md

# Verify Cargo.lock is in step with the manifests
lock-check:
    cargo metadata --locked --format-version 1 > /dev/null

# Takes an explicit semver (`just bump 0.2.0`) or a release kind (`just bump
# minor`). The version has one home, [workspace.package]; the single member
# inherits it.
#
# This does not tag. Review, commit and push main, let the main CI run go green,
# and only then run the tag recipe. A tag pushed first starts a release job that
# competes with CI for the same Nix cache key, so both run cold.
#
# Move the workspace to a new version and refresh everything derived from it
bump version:
    #!/usr/bin/env bash
    set -euo pipefail

    manifest=Cargo.toml
    request="{{version}}"

    current="$(awk '
      /^\[workspace\.package\]$/ { in_package = 1; next }
      /^\[/ { in_package = 0 }
      in_package && /^version[[:space:]]*=/ {
        sub(/^[^=]*=[[:space:]]*"/, "")
        sub(/".*$/, "")
        print
        exit
      }
    ' "$manifest")"

    if [[ -z "$current" ]]; then
      echo "no version line in [workspace.package] of $manifest" >&2
      exit 1
    fi

    if [[ "$current" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+) ]]; then
      major="${BASH_REMATCH[1]}"
      minor="${BASH_REMATCH[2]}"
      patch="${BASH_REMATCH[3]}"
    else
      echo "current version is not semver: $current" >&2
      exit 1
    fi

    case "$request" in
      major) new="$((major + 1)).0.0" ;;
      minor) new="$major.$((minor + 1)).0" ;;
      patch) new="$major.$minor.$((patch + 1))" ;;
      *)
        if [[ ! "$request" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
          echo "not semver and not major/minor/patch: $request" >&2
          exit 1
        fi
        new="$request"
        ;;
    esac

    tmp="$(mktemp)"
    trap 'rm -f "$tmp"' EXIT
    awk -v version="$new" '
      /^\[workspace\.package\]$/ { in_package = 1; print; next }
      /^\[/ { in_package = 0 }
      in_package && /^version[[:space:]]*=/ && !done {
        sub(/=.*/, "= \"" version "\"")
        done = 1
      }
      { print }
    ' "$manifest" > "$tmp"

    if ! grep -qx "version = \"$new\"" "$tmp"; then
      echo "the version line in [workspace.package] was not rewritten" >&2
      exit 1
    fi
    mv "$tmp" "$manifest"
    trap - EXIT

    cargo update --workspace

    printf '\nBumped %s -> %s.\n\nNext:\n' "$current" "$new"
    printf '  1. Review:     git diff\n'
    printf '  2. Stage:      git add -A\n'
    printf '  3. Commit:     git commit -m "chore(release): v%s"\n' "$new"
    printf '  4. Push main:  git push origin main\n'
    printf '  5. Wait for the main CI run to go green.\n'
    printf '  6. Tag:        run the tag recipe with v%s\n' "$new"
    printf '\nTag only after main is green. A tag pushed earlier starts a release job\n'
    printf 'that competes with CI for the same Nix cache key.\n'

# Run CI checks locally
ci: fmt-check lint lint-nix test doc readme-check lock-check build
    @echo "All CI checks passed!"

# Clean build artifacts
clean:
    cargo clean
