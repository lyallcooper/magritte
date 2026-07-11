import { defineConfig } from 'astro/config';
import rehypeDocLinks from './src/lib/rehype-doc-links.mjs';

export default defineConfig({
  site: 'https://magritte.lyall.co',
  markdown: {
    rehypePlugins: [rehypeDocLinks],
    shikiConfig: {
      themes: { light: 'solarized-light', dark: 'solarized-dark' },
      defaultColor: false,
    },
  },
});
