// Validate every internal link in the built site: page routes, asset files,
// and #fragment anchors (same-page and cross-page), checked against the ids
// actually present in each rendered page. Runs as part of `npm run build`, so
// a renamed heading or moved asset fails the deploy instead of shipping a
// dead link. External URLs are not checked.
import { readFileSync, readdirSync, statSync, existsSync } from "node:fs";
import { join, relative, basename } from "node:path";

const root = new URL("../dist/", import.meta.url).pathname;

const htmlFiles = [];
(function walk(dir) {
    for (const name of readdirSync(dir)) {
        const p = join(dir, name);
        if (statSync(p).isDirectory()) walk(p);
        else if (name.endsWith(".html")) htmlFiles.push(p);
    }
})(root);

// Route URL for each page ("/docs/config/"), plus its anchor ids.
const pages = new Map();
for (const path of htmlFiles) {
    const rel = relative(root, path);
    const url =
        basename(path) === "index.html"
            ? "/" + rel.slice(0, -"index.html".length)
            : "/" + rel;
    const ids = new Set(
        [...readFileSync(path, "utf8").matchAll(/id="([^"]+)"/g)].map(
            (m) => m[1],
        ),
    );
    pages.set(url, { path, ids });
}

const routeOf = (target) => (target.endsWith("/") ? target : target + "/");
const problems = [];
for (const [url, { path }] of pages) {
    const html = readFileSync(path, "utf8");
    const refs = [...html.matchAll(/(?:href|src|srcset)="([^"]+)"/g)].flatMap(
        (m) => m[1].split(",").map((s) => s.trim().split(" ")[0]),
    );
    for (const ref of refs) {
        if (!ref || /^(https?:|mailto:|data:)/.test(ref)) continue;
        const [target, frag] = ref.split("#");
        if (target === "") {
            // Same-page fragment.
            if (frag && !pages.get(url).ids.has(frag))
                problems.push(`${url} -> #${frag} (missing on page)`);
            continue;
        }
        if (!target.startsWith("/")) {
            problems.push(`${url} -> ${ref} (relative link)`);
            continue;
        }
        const asFile = join(root, decodeURIComponent(target).slice(1));
        const page = pages.get(routeOf(target));
        if (!page && !existsSync(asFile)) {
            problems.push(`${url} -> ${ref} (missing target)`);
            continue;
        }
        if (frag && page && !page.ids.has(frag))
            problems.push(`${url} -> ${ref} (missing anchor)`);
    }
}

if (problems.length > 0) {
    console.error(`check-links: ${problems.length} broken internal link(s):`);
    for (const p of problems) console.error(`  ${p}`);
    process.exit(1);
}
console.log(`check-links: ${pages.size} pages OK`);
