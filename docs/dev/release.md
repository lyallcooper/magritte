# Release a new version

This guide is for maintainers publishing a tagged Magritte release.

Release builds run in the public
[`lyallcooper/homebrew-magritte`](https://github.com/lyallcooper/homebrew-magritte)
repository. Pushing a version tag here sends a `repository_dispatch` event to
that repository. Its release workflow then:

1. Checks out this repository at the tag.
2. Builds the Apple silicon macOS app and x86_64 Linux archive.
3. Publishes the archives, checksums, and rendered Homebrew formula in a GitHub
   release on the public repository.
4. Updates and commits `Formula/magritte.rb`.

This repository does not create a GitHub release or run the release builds.
The in-app update check and Homebrew formula both use releases from the public
repository.

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
   git tag v0.5.0
   git push origin v0.5.0
   ```

5. Watch the public build, which usually takes about 10 minutes:

   ```sh
   gh run watch -R lyallcooper/homebrew-magritte
   ```

6. Confirm that `Formula/magritte.rb` points to the new version, then test the
   upgrade:

   ```sh
   brew update
   brew upgrade magritte
   magritte --version
   ```

## Retry a release

Retry the workflow in the public repository. Re-running the dispatcher here
would start a second release run.

The release workflow is idempotent and replaces existing assets for the tag:

```sh
gh workflow run release.yml \
  -R lyallcooper/homebrew-magritte \
  -f tag=v0.5.0
```

## Required secrets

- `HOMEBREW_TAP_TOKEN` belongs to this repository. It needs Contents write
  access to `lyallcooper/homebrew-magritte` so it can send the dispatch.
- `MAGRITTE_SOURCE_TOKEN` belongs to the public repository. It needs Contents
  read access to this repository so the build can check out the tagged source.

To replace an expired source token:

```sh
gh secret set MAGRITTE_SOURCE_TOKEN \
  -R lyallcooper/homebrew-magritte
```

Build logs in the public repository are visible to everyone. They can include
compiler output from this repository even though its source remains private.
