// Serves the repository's annotated example config verbatim, so the docs'
// link to config.example.toml works on the site as it does on GitHub.
import example from '../../../../docs/config.example.toml?raw';

export function GET() {
  return new Response(example, {
    headers: { 'Content-Type': 'text/plain; charset=utf-8' },
  });
}
