# Release process

Releases are built and published in the public tap repo,
[`lyallcooper/homebrew-magritte`](https://github.com/lyallcooper/homebrew-magritte),
whose Actions minutes are unmetered. This repo never builds in CI: a tag push
here runs a seconds-long job that fires a `repository_dispatch` at the tap,
and the tap's `release.yml` does the rest — it checks out this repo at the
tag, builds the macOS `.app` archive (`scripts/dist-macos.sh`, arm64) and the
Linux tarball (`scripts/dist-linux.sh`, x86_64), publishes the GitHub release
*on the tap* with the tarballs, checksums, and rendered formula, and commits
`Formula/magritte.rb`.

The in-app update check and the formula both point at the tap's releases;
no release objects are created in this repo.

## Cutting a release

1. Bump `version` in `crates/magritte/Cargo.toml` (and `Cargo.lock`, via
   `cargo build`) to match the tag you're about to create, and commit. The
   binary reports `CARGO_PKG_VERSION` and compares it against the tap's
   latest release, so a tag whose Cargo version lags will nag its own users
   to update.
2. Tag and push:

   ```sh
   git tag v0.5.0
   git push origin v0.5.0
   ```

3. Watch the build (about 10 minutes):

   ```sh
   gh run watch -R lyallcooper/homebrew-magritte
   ```

4. Verify: `brew update && brew upgrade magritte`, or check that
   `Formula/magritte.rb` on the tap points at the new version.

## Retries and manual builds

The real workflow lives on the tap, so retry there — re-running the
dispatcher here just starts a duplicate run. To rebuild any tag (idempotent;
existing release assets are clobbered):

```sh
gh workflow run release.yml -R lyallcooper/homebrew-magritte -f tag=v0.5.0
```

## Tokens

- `HOMEBREW_TAP_TOKEN` (secret in this repo): PAT with contents write on the
  tap; fires the dispatch.
- `MAGRITTE_SOURCE_TOKEN` (secret in the tap): fine-grained PAT with
  contents read on this repo; lets the tap's workflow check out the source.
  After regenerating an expired token: `gh secret set MAGRITTE_SOURCE_TOKEN
  -R lyallcooper/homebrew-magritte`.

Build logs on the tap are public — compiler output from this private repo is
visible to anyone, though the source itself is not.
