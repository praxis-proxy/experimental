import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';
import { openRecordingBrowser } from '../../../src/browser/live.js';
import { requireGate } from '../../../src/evidence/gates.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const exampleDir = path.resolve(here, '..');
const baseUrl = process.env.DEMO_URL || 'http://127.0.0.1:3000';
const narrationWav = path.join(exampleDir, 'narration', 'narration.wav');

function narrationSeconds() {
  try {
    const out = execFileSync(
      'ffprobe',
      ['-v', 'error', '-show_entries', 'format=duration', '-of', 'default=noprint_wrappers=1:nokey=1', narrationWav],
      { encoding: 'utf8' },
    );
    return Number(out.trim());
  } catch {
    return 0;
  }
}

// Total on-screen time must be >= narration length, or scripts/assemble.sh's
// `-shortest` ffmpeg flag truncates the audio to match a shorter video.
const targetMs = Math.max(narrationSeconds() + 3, 30) * 1000;
const start = Date.now();
const holdRemaining = async page => {
  const remaining = targetMs - (Date.now() - start);
  if (remaining > 0) await page.waitForTimeout(remaining);
};

const { browser, context, page } = await openRecordingBrowser({ videoDir: process.env.OUTPUT_DIR || 'output/raw' });
try {
  await page.goto(`file://${path.join(exampleDir, 'slides', 'proof-agenda.html')}`, { waitUntil: 'load' });
  await page.waitForTimeout(12000);

  await page.goto(baseUrl, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('#run-scenario');
  await page.waitForTimeout(1500);
  await page.click('#run-scenario');

  const deadline = Date.now() + 30000;
  let results = null;
  while (Date.now() < deadline) {
    results = await page.evaluate(() => (window.__scenarioDone ? window.__scenarioResults : null));
    if (results) break;
    await page.waitForTimeout(500);
  }
  if (!results) throw new Error('scenario did not complete within timeout');

  const [appAOnA, appAOnB, appBOnB, appCOnA, appARecovered] = results;
  requireGate(appAOnA.status === 200, 'app-a admitted on gateway A', appAOnA);
  requireGate(
    appAOnB.status === 429 && appAOnB.remaining === '0',
    'app-a denied on gateway B via shared Valkey budget',
    appAOnB,
  );
  requireGate(appBOnB.status === 200, 'app-b admitted on gateway B, unaffected by app-a', appBOnB);
  requireGate(appCOnA.status === 200, 'app-c admitted on gateway A on its own untouched budget', appCOnA);
  requireGate(
    appARecovered.status === 200,
    "app-a admitted again on gateway A once the sliding window aged its earlier reservation out, with no manual reset",
    appARecovered,
  );

  await holdRemaining(page);
} finally {
  await page.close();
  await context.close();
  await browser.close();
}
