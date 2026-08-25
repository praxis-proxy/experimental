import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { chromium } from "@playwright/test";

const example = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const output = path.join(example, "output", "slides");
const slides = ["title", "reactive-burst", "independent-policies", "soft-token-limits", "outro"];

await fs.mkdir(output, { recursive: true });
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1920, height: 1080 }, deviceScaleFactor: 1 });

for (const slide of slides) {
  const source = path.join(example, "slides", `${slide}.html`);
  await page.goto(pathToFileURL(source).href, { waitUntil: "load" });
  await page.screenshot({ path: path.join(output, `${slide}.png`), fullPage: false });
}

await browser.close();
console.log(`Rendered ${slides.length} slides to ${output}`);
