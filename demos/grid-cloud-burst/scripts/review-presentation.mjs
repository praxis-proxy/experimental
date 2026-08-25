import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { chromium } from "@playwright/test";

const example = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const video = path.join(example, "output", "grid-cloud-burst-narrated.mp4");
const review = path.join(example, "output", "review.html");
const screenshots = path.join(example, "output", "review");
await fs.mkdir(screenshots, { recursive: true });
await fs.writeFile(review, `<!doctype html><meta charset="utf-8"><style>body{margin:0;background:#111;display:grid;place-items:center;height:100vh}video{width:1920px;height:1080px;object-fit:contain}</style><video></video>`);

const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1920, height: 1080 }, deviceScaleFactor: 1 });
await page.goto(pathToFileURL(review).href, { waitUntil: "load" });
await page.locator("video").evaluate((element, source) => { element.src = source; }, pathToFileURL(video).href);
const metadata = await page.locator("video").evaluate((element) => new Promise((resolve, reject) => {
  element.addEventListener("loadedmetadata", () => resolve({ duration: element.duration, width: element.videoWidth, height: element.videoHeight }), { once: true });
  element.addEventListener("error", () => reject(new Error("assembled video could not be loaded")), { once: true });
}));

for (const second of [0.5, 35, 100, 155, 215, 275, 330, 390, 433, Math.max(0, metadata.duration - 0.5)]) {
  await page.locator("video").evaluate((element, at) => { element.currentTime = at; }, second);
  await page.waitForTimeout(800);
  await page.screenshot({ path: path.join(screenshots, `at-${String(second).replace(".", "-")}.png`) });
}

console.log(JSON.stringify({ metadata, screenshots }, null, 2));
await browser.close();
