# magritte.lyall.co (landing page + docs)

Astro site for Magritte. The docs pages render the repository's `docs/*.md`
files directly (see `src/content.config.ts`); `docs/dev/` is excluded. Links
between the markdown files are rewritten to site routes at build time
(`src/lib/rehype-doc-links.mjs`), so the files stay browsable on GitHub.

```sh
pnpm install
pnpm dev           # local dev server with live reload (also on docs/ edits)
pnpm build         # static output in dist/ + internal link check
```

`pnpm build` finishes by validating every internal link and anchor in the
built output (`scripts/check-links.mjs`), so a renamed heading or moved asset
fails the build -- and the Cloudflare deploy -- instead of shipping.

## Toolchain

node and pnpm are pinned in the repository's `.mise.toml`, alongside rust.
The pins are chosen to match what Cloudflare builds with, so a green local
build means a green deploy:

- node: `.node-version` here is what the Pages build reads; keep it in step
  with the `.mise.toml` pin. Astro 7 needs >=22.12.
- pnpm: pinned to the Pages v3 build image's built-in version. That image
  does not read `packageManager`, so bumping pnpm past it also means setting
  `PNPM_VERSION` in the Pages project's environment variables.

`esbuild` and `sharp` are listed under `pnpm.onlyBuiltDependencies` in
`package.json` -- pnpm blocks install scripts by default, and those two are
the only dependencies allowed to run theirs.

## Deploying (Cloudflare Pages)

Connect the repository and set:

- Root directory: `site`
- Build command: `pnpm build`
- Build output directory: `dist`
- Build watch paths: `site/*`, `docs/*` -- doc edits must redeploy the site

The palette is Selenized Light/Black, copied from the app's bundled theme
(`crates/magritte/themes/selenized.json`). If the app's default theme changes,
update the variables at the top of `src/styles/global.css`.

## Screenshots

`public/screenshots/` holds four real captures of the status view (desktop and
mobile, light and dark), retaken by `scripts/site-shots.sh` (see its header
for what it stages and the invariants it maintains). Rerun it after visual
changes to the app, eyeball all four outputs, then rebuild the site.

The landing page couples to the capture geometry: `src/pages/index.astro`
hardcodes the `<img>` width/height and sizes the CSS traffic-light dots
against each capture's natural width (730pt desktop, 640pt mobile). If the
script's window sizes or crop heights change, update those too.
