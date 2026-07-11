import { visit } from 'unist-util-visit';

/**
 * Rewrites the docs' repo-relative links to site routes, so the same markdown
 * works when browsed on GitHub and when rendered here:
 *
 *   config.md            -> /docs/config/
 *   config.md#keymap     -> /docs/config/#keymap
 *   config.example.toml  -> /docs/config.example.toml (served by an endpoint)
 *
 * Absolute URLs and bare #fragments pass through untouched. Only same-directory
 * targets are handled; the docs don't link outside docs/ today.
 */
export default function rehypeDocLinks() {
  return (tree) => {
    visit(tree, 'element', (node) => {
      if (node.tagName !== 'a') return;
      const href = node.properties?.href;
      if (typeof href !== 'string' || /^([a-z][a-z0-9+.-]*:|[/#])/i.test(href)) return;
      const target = href.replace(/^\.\//, '');
      const md = target.match(/^([\w-]+)\.md(#.*)?$/);
      node.properties.href = md
        ? `/docs/${md[1]}/${md[2] ?? ''}`
        : `/docs/${target}`;
    });
  };
}
