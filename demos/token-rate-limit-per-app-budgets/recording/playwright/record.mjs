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

const outputDir = process.env.OUTPUT_DIR || path.join(exampleDir, 'output', 'raw');
const { browser, context, page } = await openRecordingBrowser({ videoDir: outputDir });
try {
  // proof-agenda.html is a self-advancing 3-section deck (title/cards ->
  // architecture -> animated topology preview) timed to narration.srt's
  // first 7 cues; 53991ms is that segment's measured duration with the
  // Ava (Premium) narration voice (see RECORDING.md's "Intro deck"
  // section) plus no extra padding, since the deck's own last caption
  // already holds through its final frame.
  await page.goto(`file://${path.join(exampleDir, 'slides', 'proof-agenda.html')}`, { waitUntil: 'load' });
  await page.waitForTimeout(53991);

  await page.goto(baseUrl, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('#run-scenario');
  await page.waitForTimeout(1500);
  await page.click('#run-scenario');

  // The re-paced scenario (see dashboard/index.html) now spreads its 8
  // requests across ~79s to track narration.srt's cues instead of firing
  // them all in the first ~15s, so this deadline needs enough headroom
  // above that, not just above a single request's round-trip time.
  const deadline = Date.now() + 100000;
  let results = null;
  while (Date.now() < deadline) {
    results = await page.evaluate(() => (window.__scenarioDone ? window.__scenarioResults : null));
    if (results) break;
    await page.waitForTimeout(500);
  }
  if (!results) throw new Error('scenario did not complete within timeout');

  const [appAOnA, appAOnB, appAOnADenied, appCOnA, appBOnA, appBOnB, appBRecovered, appARecovered] = results;
  requireGate(
    appAOnA.status === 200,
    'app-a (gold-tier, sliding_window) admitted on gateway A -- 15/40 reserved, 25 remaining',
    appAOnA,
  );
  requireGate(
    appAOnB.status === 200,
    'app-a admitted again on gateway B, the OTHER process -- the same shared Valkey budget keeps accumulating: 30/40 reserved, 10 remaining',
    appAOnB,
  );
  requireGate(
    // X-RateLimit-Remaining-Tokens is a hardcoded "0" on every 429 regardless
    // of algorithm or actual usage (an MVP shortcut, not computed from real
    // remaining capacity -- see filters/src/token_rate_limit/mod.rs's
    // HEADER_RATELIMIT_REMAINING_TOKENS doc comment), so this only proves
    // "denied", not the specific 10-tokens-short-of-15 shortfall.
    appAOnADenied.status === 429 && appAOnADenied.rate_limit?.remaining_tokens === '0',
    'app-a denied on a third request, back on gateway A -- gold-tier now exhausted (needs 15, only 10 remain), proven consistent across both gateways',
    appAOnADenied,
  );
  requireGate(
    appCOnA.status === 200,
    'app-c (gold-tier, sliding_window) admitted on gateway A on its own untouched budget',
    appCOnA,
  );
  requireGate(
    appBOnA.status === 200,
    'app-b (silver-tier, token_bucket) admitted on gateway A -- 7/10 reserved, 3 remaining',
    appBOnA,
  );
  requireGate(
    appBOnB.status === 429 && appBOnB.rate_limit?.remaining_tokens === '0',
    'app-b denied on gateway B immediately after -- a single burst already leaves too little (3 remaining) for a second 7-token call',
    appBOnB,
  );
  requireGate(
    appBRecovered.status === 200,
    "app-b admitted again on gateway B once silver-tier's token_bucket refilled continuously -- just 2 seconds (4 tokens) would already have been enough, with no manual reset",
    appBRecovered,
  );
  requireGate(
    appARecovered.status === 200,
    'app-a admitted again on gateway A once the sliding window aged its earlier reservations out of the full 10s window -- a much longer wait than silver-tier needed, with no manual reset',
    appARecovered,
  );

  await holdRemaining(page);
} finally {
  await page.close();
  await context.close();
  await browser.close();
}
