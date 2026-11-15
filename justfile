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

# Pin every action reference, actions/* included, and annotate each with its release
pin *args='':
    hashpinner --pin --no-allow {{args}}

# Fail if a reference is unpinned or a version comment does not match its commit
pin-check:
    hashpinner --check --deep --no-allow

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

# ---------------------------------------------------------------------------
# Release recipes.
#
# The tag is the version. One workspace member inherits [workspace.package];
# `cargo pkgid` is what the tag is compared against. Linux musl only: diskfoot
# uses statx and Linux filesystem APIs, so there is no macOS target.
# ---------------------------------------------------------------------------

crate := "diskfoot"
release_repo := "sshine/diskfoot"
release_targets := "x86_64-unknown-linux-musl aarch64-unknown-linux-musl"

# The CI workflow whose latest run on main must be green before tagging.
ci_workflow := "ci.yml"

# The URL the `tag` recipe prints when it cannot read CI state itself.
ci_url := "https://github.com/sshine/diskfoot/actions?query=branch%3Amain"

# Assert the release tag names the version cargo would publish
check-version version:
    @pkgid="$(cargo pkgid -p {{crate}})"; crate="v${pkgid##*#}"; \
    if [ "$crate" != "{{version}}" ]; then \
        echo "tag {{version}} does not match crate version $crate" >&2; exit 1; \
    fi

# Build one target into dist/ without clearing it
dist-target version target: (check-version version)
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p dist
    cargo build --release --locked --target "{{target}}" -p {{crate}}
    name="{{crate}}-{{version}}-{{target}}"
    tar -czf "dist/$name.tar.gz" -C "target/{{target}}/release" {{crate}}
    ( cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256" )

# Build the static release artifacts and their checksums into dist/
dist-linux version: (check-version version)
    #!/usr/bin/env bash
    set -euo pipefail
    rm -rf dist && mkdir -p dist
    for target in {{release_targets}}; do
        just dist-target "{{version}}" "$target"
    done
    ls -1 dist

# Print the SRI hashes nix/binary.nix pins, read from the published release
release-hashes version:
    #!/usr/bin/env bash
    set -euo pipefail
    for target in {{release_targets}}; do
        case "$target" in
            x86_64-*) system="x86_64-linux" ;;
            aarch64-*) system="aarch64-linux" ;;
            *) echo "no Nix system name for $target" >&2; exit 1 ;;
        esac
        url="https://github.com/{{release_repo}}/releases/download/{{version}}/{{crate}}-{{version}}-$target.tar.gz"
        hash="$(nix store prefetch-file --json --hash-type sha256 "$url" | jq -r .hash)"
        printf '"%s" = "%s";\n' "$system" "$hash"
    done

# Print the same SRI lines from a local dist/, for an offline check. Only correct
# when dist/ holds the bytes the release published.
dist-hashes:
    #!/usr/bin/env bash
    set -euo pipefail
    for target in {{release_targets}}; do
        case "$target" in
            x86_64-*) system="x86_64-linux" ;;
            aarch64-*) system="aarch64-linux" ;;
            *) echo "no Nix system name for $target" >&2; exit 1 ;;
        esac
        archive=(dist/{{crate}}-*-"$target".tar.gz)
        if [ ! -e "${archive[0]}" ]; then
            echo "no tarball for $target in dist/" >&2
            exit 1
        fi
        hash="$(nix hash file --sri --type sha256 "${archive[0]}")"
        printf '"%s" = "%s";\n' "$system" "$hash"
    done

# Refuses unless the tree is clean, HEAD is exactly origin/main, the tag exists
# neither locally nor on origin, the manifest carries the tagged version, and
# the CI workflow's latest run on main is green.
tag version: (check-version version)
    #!/usr/bin/env bash
    set -euo pipefail
    tag="{{version}}"

    if [ -n "$(git status --porcelain)" ]; then
        echo "working tree is dirty; commit or stash first" >&2
        exit 1
    fi
    if [ "$(git rev-parse --abbrev-ref HEAD)" != "main" ]; then
        echo "not on main; a release tag names a commit on main" >&2
        exit 1
    fi

    git fetch --quiet origin main
    git fetch --quiet --tags origin
    if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
        echo "HEAD is not origin/main; push main, or check out the commit main points at" >&2
        exit 1
    fi
    if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
        echo "tag $tag already exists locally" >&2
        exit 1
    fi
    if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1; then
        echo "tag $tag already exists on origin" >&2
        exit 1
    fi

    if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
        conclusion="$(gh run list --branch main --workflow "{{ci_workflow}}" --limit 1 --json conclusion --jq '.[0].conclusion // ""')"
        if [ "$conclusion" != "success" ]; then
            echo "the latest run on main concluded '${conclusion:-none}'; wait for green:" >&2
            gh run list --branch main --limit 5
            exit 1
        fi
    elif [ "${CONFIRM:-}" != "green" ]; then
        echo "gh is not available, so main's CI state cannot be read." >&2
        echo "Check {{ci_url}} and rerun with CONFIRM=green." >&2
        exit 1
    fi

    git tag -a "$tag" -m "$tag"
    git push origin "$tag"
