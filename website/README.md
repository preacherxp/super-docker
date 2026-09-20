# super-docker landing page

Uses the Purecode site's stack and dependency versions: Astro 7, Vue 3 islands,
Tailwind CSS 4, TypeScript, and self-hosted JetBrains Mono. Astro produces a
static site; Vue handles the install clipboard and demo player.

## Develop

Requires Node.js 22.19+ and npm.

```sh
cd website
npm ci
npm run dev
```

Open the local URL printed by Astro. Edit the page in `src/pages/index.astro`,
styles in `src/styles/global.css`, and interactions in `src/components/`.
Section reveals live in `src/scripts/motion.ts`.

## Build and preview

```sh
npm run build
npm run preview
```

Publish `website/dist/` as a static site. The canonical URL and sitemap target
`https://purecode.sh/`. Deployment and DNS are managed separately.

The hero uses `../docs/demo.png`, a capture of the actual Rust TUI. It opens at
full size when clicked. The demo player loads `../docs/demo.gif` only when
requested. Both assets come from the same disposable Docker workload;
regenerate them from the repository root with `vhs demo.tape`.

## Browser checks

```sh
npx playwright install chromium
npm run build
npm test
```

Checks cover desktop, mobile, reduced motion, the real capture asset, copy
success and failure, demo playback and focus restoration, the compact workflow,
horizontal overflow, and automated accessibility checks. To use an existing
Chrome installation, set `PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH` to its executable.
