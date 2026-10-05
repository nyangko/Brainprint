// #72: a static single-page build. `bundleStrategy: 'inline'` puts all JS
// and CSS into one index.html, which `brainprint web` embeds -- no Node at
// runtime and no asset files to serve.
import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
	plugins: [
		sveltekit({
			adapter: adapter({ pages: 'build', assets: 'build', fallback: undefined, strict: true }),
			output: { bundleStrategy: 'inline' },
			// A fixed name keeps the build reproducible (the default is a timestamp).
			version: { name: 'brainprint-web' }
		})
	]
});
