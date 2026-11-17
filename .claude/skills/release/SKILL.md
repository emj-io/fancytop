---
name: release
description: Cut a fancytop release: bump the version, update the changelog, tag, and let CI publish the Apple Silicon binary. Use when asked to release or tag a version.
disable-model-invocation: true
---

Pushing and tagging are outward-facing. Confirm the version number with the user before step 5.

1. Working tree clean and on `main`; `scripts/check.sh` passes.
2. Set `version` in `Cargo.toml`, run `cargo build` so `Cargo.lock` updates.
3. Move the unreleased notes in `CHANGELOG.md` under a new `## X.Y.Z` heading.
4. Commit: `Release vX.Y.Z`.
5. `git push origin main`, wait for CI to go green, then `git tag vX.Y.Z && git push origin vX.Y.Z`.
6. `.github/workflows/release.yml` checks the tag matches `Cargo.toml`, builds `aarch64-apple-darwin`, and creates the GitHub release with the tarball and sha256. Watch it with `gh run watch`.
7. Never move or delete a published tag; ship a new patch version instead.
