# Release a new version

This guide is for maintainers publishing a tagged Magritte release.

Pushing a version tag runs `release.yml` in this repository, which:

1. Builds the Apple silicon macOS app and x86_64 Linux archive.
2. Publishes the archives, checksums, and rendered Homebrew formula in a
   GitHub release on this repository.
3. Pushes the updated `Formula/magritte.rb` to
   [`lyallcooper/homebrew-magritte`](https://github.com/lyallcooper/homebrew-magritte).
4. Mirrors a bare release on the tap repository so binaries released before
   v0.8.0 (whose update checks query the tap) still see new versions.

The in-app update check uses this repository's releases; the Homebrew formula
downloads from them.

## Publish a release

1. Update `version` in `crates/magritte/Cargo.toml`.

2. Update the lockfile and verify the release candidate:

   ```sh
   cargo build
   cargo test
   cargo clippy --all-targets
   cargo fmt --check
   ```

3. Commit the version change. The Cargo version must match the tag because the
   binary uses it for `--version` and update checks.

4. Create and push the tag:

   ```sh
   git tag -a v0.8.0 -m "Magritte v0.8.0"
   git push origin v0.8.0
   ```

5. Watch the build, which usually takes about 10 minutes:

   ```sh
   gh run watch
   ```

6. Confirm that the tap's `Formula/magritte.rb` points to the new version,
   then test the upgrade:

   ```sh
   brew update
   brew upgrade magritte
   magritte --version
   ```

## Retry a release

The release workflow is idempotent and replaces existing assets for the tag:

```sh
gh workflow run release.yml -f tag=v0.8.0
```

## Required secrets

- `HOMEBREW_TAP_TOKEN` needs Contents write access to
  `lyallcooper/homebrew-magritte` so the workflow can push the formula update
  and mirror the release tag.
