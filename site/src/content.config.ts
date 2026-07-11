import { defineCollection } from 'astro:content';
import { glob } from 'astro/loaders';

// The docs collection reads the repository's docs/ directory directly, so the
// markdown files on GitHub and the pages on the site share one source.
// docs/dev/ is contributor documentation and stays off the site.
export const collections = {
  docs: defineCollection({
    loader: glob({ pattern: ['**/*.md', '!dev/**'], base: '../docs' }),
  }),
};
