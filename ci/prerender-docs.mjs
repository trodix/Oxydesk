// Writes the settings of site/settings.js into the static HTML of the documentation page, so that crawlers and
// readers without JavaScript see them. The page re-renders the same cards on load, which keeps search working.
// Usage: node ci/prerender-docs.mjs <path to the docs.html to rewrite>
import { createRequire } from "node:module";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { SETTINGS, card } = require(join(dirname(fileURLToPath(import.meta.url)), "../site/settings.js"));

const file = process.argv[2];
if (!file) {
  console.error("usage: node ci/prerender-docs.mjs <docs.html>");
  process.exit(2);
}
const marker = '<div id="list" aria-live="polite"></div>';
const html = readFileSync(file, "utf8");
if (!html.includes(marker)) {
  console.error(`${file}: the empty list container was not found (already prerendered?)`);
  process.exit(1);
}
const cards = SETTINGS.map((p) => card(p)).join("\n");
writeFileSync(file, html.replace(marker, `<div id="list" aria-live="polite">\n${cards}\n</div>`));
console.log(`prerendered ${SETTINGS.length} settings into ${file}`);
