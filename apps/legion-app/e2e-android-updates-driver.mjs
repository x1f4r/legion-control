#!/usr/bin/env node
// UIAutomator + adb driver for apps/legion-app/e2e-android-updates.sh.
// Drives signed release APKs (android:debuggable=false) end-to-end on a real Android emulator:
//   1. Pairs seq1 with the throwaway loopback node `tower`.
//   2. Publishes seq2, navigates to Settings > About, grants unknown-sources permission via Android
//      Settings, and applies seq1 -> seq2.
//   3. Verifies the quiet unbordered post-update row ("Updated to … · What is new"), release notes
//      sheet, and dismisses it, capturing a screenshot.
//   4. Publishes seq3, downloads it, waits out Android's 30-second SilentUpdatePolicy throttle,
//      backgrounds Legion (KEYCODE_HOME), and asserts seq2 -> seq3 completes silently in the
//      background without ever bringing Legion or an installer prompt back to the foreground.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

function requiredEnv(name) {
  const value = process.env[name];
  if (!value) throw new Error(`Missing required environment variable: ${name}`);
  return value;
}

const SERIAL = requiredEnv('E2E_ADB_SERIAL');
const APP_ID = requiredEnv('E2E_APP_ID');
const ACTIVITY = `${APP_ID}/com.x1f4r.legion.MainActivity`;
const SHOTS_DIR = requiredEnv('E2E_SHOTS_DIR');
const LEGION_BIN = requiredEnv('E2E_LEGION_BIN');
const NODE_HOME = requiredEnv('E2E_NODE_HOME');
const INVITE = requiredEnv('E2E_INVITE');
const SET2_DIR = requiredEnv('E2E_SET2_DIR');
const SET3_DIR = requiredEnv('E2E_SET3_DIR');
const VC1 = Number(requiredEnv('E2E_VC1'));
const VC2 = Number(requiredEnv('E2E_VC2'));
const VC3 = Number(requiredEnv('E2E_VC3'));

function log(msg) {
  process.stdout.write(`[e2e-updates] ${msg}\n`);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function adb(args, opts = {}) {
  return execFileSync('adb', ['-s', SERIAL, ...args], {
    encoding: opts.encoding ?? 'utf8',
    maxBuffer: 32 * 1024 * 1024,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
}

function legionNode(args) {
  return execFileSync(LEGION_BIN, args, {
    encoding: 'utf8',
    env: { ...process.env, LEGION_HOME: NODE_HOME },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
}

function screenshot(name) {
  fs.mkdirSync(SHOTS_DIR, { recursive: true });
  const png = adb(['exec-out', 'screencap', '-p'], { encoding: 'buffer' });
  const out = path.join(SHOTS_DIR, `${name}.png`);
  fs.writeFileSync(out, png);
  log(`captured ${out}`);
}

function installedVersionCode() {
  const out = adb(['shell', 'dumpsys', 'package', APP_ID]);
  const m = out.match(/versionCode=(\d+)/);
  return m ? Number(m[1]) : null;
}

function focusedWindowLine() {
  try {
    const out = adb(['shell', 'dumpsys', 'window']);
    const lines = out
      .split('\n')
      .map((l) => l.trim())
      .filter((l) => l.startsWith('mCurrentFocus=') || l.startsWith('mFocusedApp='));
    return lines.join(' | ');
  } catch {
    return '';
  }
}

function launchApp() {
  adb(['logcat', '-c']);
  adb(['shell', 'am', 'start', '-W', '-n', ACTIVITY]);
}

async function waitForReady(timeoutMs = 45_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const logs = adb(['logcat', '-d', '-s', 'legion-app:I']);
    if (logs.includes('legion-app: interface ready')) {
      return;
    }
    await sleep(500);
  }
  throw new Error('Timed out waiting for legion-app: interface ready in logcat');
}

function parseUiNodes(xml) {
  const nodes = [];
  const tagRe = /<node\s+([^>]+?)\/?>/g;
  const attrRe = /([\w-]+)="([^"]*)"/g;
  let m;
  while ((m = tagRe.exec(xml)) !== null) {
    const attrs = {};
    let am;
    while ((am = attrRe.exec(m[1])) !== null) {
      attrs[am[1]] = am[2]
        .replaceAll('&quot;', '"')
        .replaceAll('&amp;', '&')
        .replaceAll('&lt;', '<')
        .replaceAll('&gt;', '>');
    }
    const bm = (attrs.bounds || '').match(/^\[(\d+),(\d+)\]\[(\d+),(\d+)\]$/);
    if (bm) {
      const x1 = Number(bm[1]);
      const y1 = Number(bm[2]);
      const x2 = Number(bm[3]);
      const y2 = Number(bm[4]);
      attrs._cx = Math.round((x1 + x2) / 2);
      attrs._cy = Math.round((y1 + y2) / 2);
      attrs._w = x2 - x1;
      attrs._h = y2 - y1;
    }
    nodes.push(attrs);
  }
  return nodes;
}

function dumpUiNodes() {
  try {
    adb(['shell', 'uiautomator', 'dump', '/sdcard/legion-ui.xml']);
    const xml = adb(['shell', 'cat', '/sdcard/legion-ui.xml']);
    return parseUiNodes(xml);
  } catch {
    return [];
  }
}

function tap(x, y) {
  adb(['shell', 'input', 'tap', String(x), String(y)]);
}

function nodeMatches(node, matcher) {
  const text = node.text || '';
  const desc = node['content-desc'] || '';
  if (typeof matcher === 'string') {
    return text === matcher || desc === matcher;
  }
  if (matcher instanceof RegExp) {
    return matcher.test(text) || matcher.test(desc);
  }
  return matcher(node);
}

async function waitForNode(matcher, { timeoutMs = 30_000, label = String(matcher) } = {}) {
  const deadline = Date.now() + timeoutMs;
  let lastNodes = [];
  while (Date.now() < deadline) {
    lastNodes = dumpUiNodes();
    const hit = lastNodes.find((n) => n._w > 0 && n._h > 0 && nodeMatches(n, matcher));
    if (hit) return hit;
    await sleep(500);
  }
  const sample = lastNodes
    .map((n) => `${n.class || ''}[text=${JSON.stringify(n.text || '')},desc=${JSON.stringify(n['content-desc'] || '')}]`)
    .filter((s) => !s.includes('text="",desc=""'))
    .slice(0, 40)
    .join('\n  ');
  throw new Error(`Timed out waiting for UI node (${label}). Visible nodes:\n  ${sample}`);
}

async function tapNode(matcher, opts = {}) {
  const node = await waitForNode(matcher, opts);
  log(
    `tap(${node._cx}, ${node._cy}) ${opts.label || ''} class=${node.class || ''} text=${JSON.stringify(node.text || '')} desc=${JSON.stringify(node['content-desc'] || '')}`,
  );
  tap(node._cx, node._cy);
  await sleep(350);
  return node;
}

async function inputAsciiText(str) {
  // Send in small ASCII chunks, never starting a chunk with '-' (which `input text` treats as a flag).
  let i = 0;
  while (i < str.length) {
    if (str[i] === '-') {
      adb(['shell', 'input', 'keyevent', 'KEYCODE_MINUS']);
      i += 1;
      await sleep(40);
      continue;
    }
    let end = Math.min(i + 24, str.length);
    const chunk = str.slice(i, end);
    adb(['shell', 'input', 'text', chunk]);
    i = end;
    await sleep(100);
  }
}

async function dismissKeyboardIfShown() {
  try {
    const out = adb(['shell', 'dumpsys', 'input_method']);
    if (/mInputShown=true/.test(out)) {
      adb(['shell', 'input', 'keyevent', 'KEYCODE_BACK']);
      await sleep(400);
    }
  } catch {
    // Best-effort keyboard dismissal.
  }
}

async function openSettingsAbout() {
  // Tap top-right Settings gear button.
  await tapNode(/^Settings$/i, { label: 'Settings button' });
  await waitForNode(/^Appearance$/i, { label: 'Settings list loaded', timeoutMs: 15_000 });
  // Scroll down in Settings if About is off-screen.
  const nodes = dumpUiNodes();
  const about = nodes.find((n) => n._w > 0 && n._h > 0 && /^About$/i.test(n.text || n['content-desc'] || ''));
  if (!about) {
    adb(['shell', 'input', 'swipe', '540', '1600', '540', '500', '250']);
    await sleep(400);
  }
  await tapNode(/^About$/i, { label: 'About row' });
  await waitForNode(/^Updates$/i, { label: 'Updates section header' });
}

async function main() {
  assert.equal(installedVersionCode(), VC1, `Expected initial versionCode=${VC1}`);

  log('Launching seq1 release APK and waiting for ready marker');
  launchApp();
  await waitForReady();

  log('Pairing seq1 with throwaway tower over 10.0.2.2');
  await tapNode(/^Paste invite$/i, { label: 'Paste invite button', timeoutMs: 20_000 });
  await waitForNode(/^Invite$/i, { label: 'Invite field label', timeoutMs: 10_000 });
  await sleep(600);
  // Scroll the form up so the Invite textarea and Pair device button sit comfortably in view.
  adb(['shell', 'input', 'swipe', '540', '1650', '540', '700', '250']);
  await sleep(500);

  // Locate the Invite textarea EditText below the "Invite" label (or the last EditText).
  let inviteField = null;
  const editDeadline = Date.now() + 10_000;
  while (Date.now() < editDeadline) {
    const nodes = dumpUiNodes();
    const inviteLabel = nodes.find((n) => n._w > 0 && n._h > 0 && /^Invite$/i.test(n.text || ''));
    const allEdits = nodes.filter(
      (n) => (n.class || '').includes('EditText') && n._w > 0 && n._h > 0,
    );
    if (allEdits.length >= 2) {
      inviteField =
        (inviteLabel && allEdits.find((n) => n._cy >= inviteLabel._cy)) ||
        allEdits[allEdits.length - 1];
      break;
    }
    await sleep(300);
  }
  assert.ok(inviteField, 'Expected Invite EditText to appear after tapping Paste invite');
  log(
    `Invite EditText bounds=${inviteField.bounds} focused=${inviteField.focused} hint=${JSON.stringify(inviteField.hint || '')}`,
  );

  // Ensure the textarea actually receives focus before typing.
  if (inviteField.focused !== 'true') {
    tap(inviteField._cx, inviteField._cy);
    await sleep(500);
  }
  const focusCheck = dumpUiNodes().filter(
    (n) => (n.class || '').includes('EditText') && n._w > 0 && n._h > 0,
  );
  const focusedEdit = focusCheck[focusCheck.length - 1];
  if (focusedEdit && focusedEdit.focused !== 'true') {
    // Re-tap slightly inset from top-left of the textarea bounds.
    const bm = (focusedEdit.bounds || '').match(/^\[(\d+),(\d+)\]\[(\d+),(\d+)\]$/);
    if (bm) {
      tap(Number(bm[1]) + 40, Number(bm[2]) + 40);
      await sleep(500);
    }
  }

  // Try Android clipboard paste first if supported (fast and lossless), otherwise type ASCII chunks.
  let pasted = false;
  try {
    adb(['shell', 'cmd', 'clipboard', 'set', 'text', INVITE]);
    await sleep(200);
    adb(['shell', 'input', 'keyevent', 'KEYCODE_PASTE']);
    await sleep(400);
    const editsAfterPaste = dumpUiNodes().filter(
      (n) => (n.class || '').includes('EditText') && n._w > 0 && n._h > 0,
    );
    const val = editsAfterPaste[editsAfterPaste.length - 1]?.text || '';
    if (val.startsWith('legion-invite-')) {
      pasted = true;
      log('Entered invite via Android clipboard paste');
    }
  } catch {
    // Fall back to chunked `input text`.
  }

  if (!pasted) {
    await inputAsciiText(INVITE);
    await sleep(400);
  }

  // Confirm the invite text actually entered the textarea before tapping Pair device.
  const afterTypeEdits = dumpUiNodes().filter(
    (n) => (n.class || '').includes('EditText') && n._w > 0 && n._h > 0,
  );
  const typedText = afterTypeEdits[afterTypeEdits.length - 1]?.text || '';
  if (!typedText.startsWith('legion-invite-')) {
    log(
      `Debug EditTexts after input: ${JSON.stringify(
        afterTypeEdits.map((n) => ({
          bounds: n.bounds,
          focused: n.focused,
          text: (n.text || '').slice(0, 60),
          hint: n.hint || '',
        })),
      )}`,
    );
  }
  assert.ok(
    typedText.startsWith('legion-invite-'),
    `Expected Invite textarea to hold invite text, got ${JSON.stringify(typedText.slice(0, 60))}`,
  );

  await dismissKeyboardIfShown();
  await tapNode(/^Pair (device|machine)$/i, { label: 'Pair submit button' });

  await waitForNode(/^tower$/i, { label: 'tower machine card after pairing', timeoutMs: 30_000 });
  log('Paired successfully with tower');

  log(`Publishing preview update set seq2 (${SET2_DIR})`);
  const pub2 = JSON.parse(legionNode(['update.publish', `from=${SET2_DIR}`, '--json']));
  assert.equal(pub2.channel, 'preview');

  await openSettingsAbout();
  // Trigger a check if not already ready.
  const nowNodes = dumpUiNodes();
  if (!nowNodes.some((n) => /^Update now$/i.test(n.text || n['content-desc'] || ''))) {
    const checkBtn = nowNodes.find((n) => /^Check now$/i.test(n.text || n['content-desc'] || ''));
    if (checkBtn) {
      tap(checkBtn._cx, checkBtn._cy);
      await sleep(500);
    }
  }
  await waitForNode(/^Update now$/i, { label: 'Update now button for seq2', timeoutMs: 45_000 });
  log('seq2 downloaded and verified; tapping Update now');
  await tapNode(/^Update now$/i, { label: 'Update now for seq2' });

  // Handle unknown-sources Settings screen and/or PackageInstaller confirmation until versionCode == VC2.
  const update2Deadline = Date.now() + 90_000;
  let grantedUnknownSources = false;
  while (Date.now() < update2Deadline) {
    if (installedVersionCode() === VC2) {
      break;
    }
    const focus = focusedWindowLine();
    const nodes = dumpUiNodes();

    if (focus.includes('com.android.settings') && !grantedUnknownSources) {
      const toggle = nodes.find(
        (n) =>
          n._w > 0 &&
          n._h > 0 &&
          ((n.class || '').includes('Switch') || /Allow from this source/i.test(n.text || n['content-desc'] || '')),
      );
      if (toggle) {
        log('Granting Install unknown apps permission in Android Settings');
        tap(toggle._cx, toggle._cy);
        grantedUnknownSources = true;
        await sleep(600);
        adb(['shell', 'input', 'keyevent', 'KEYCODE_BACK']);
        await sleep(800);
        // After returning from Settings, tap Update now again.
        const retryNodes = dumpUiNodes();
        const retryBtn = retryNodes.find((n) => n._w > 0 && /^Update now$/i.test(n.text || n['content-desc'] || ''));
        if (retryBtn) {
          tap(retryBtn._cx, retryBtn._cy);
        }
        await sleep(800);
        continue;
      }
    }

    if (focus.includes('packageinstaller')) {
      const confirmBtn = nodes.find(
        (n) =>
          n._w > 0 &&
          n._h > 0 &&
          /^(Update|Install)$/i.test((n.text || n['content-desc'] || '').trim()),
      );
      if (confirmBtn) {
        log(`Confirming Android PackageInstaller prompt (${confirmBtn.text || confirmBtn['content-desc']})`);
        tap(confirmBtn._cx, confirmBtn._cy);
        await sleep(1000);
        continue;
      }
    }

    await sleep(750);
  }
  assert.equal(installedVersionCode(), VC2, `Expected Android package versionCode to advance to ${VC2}`);
  const seq2InstalledAt = Date.now();
  log(`Verified seq1 -> seq2 installed (versionCode=${VC2})`);

  // Interactive seq1 -> seq2 auto-relaunches via MY_PACKAGE_REPLACED; ensure Legion is foreground
  // and verify the quiet post-update notice + release notes sheet.
  adb(['shell', 'am', 'start', '-W', '-n', ACTIVITY]);
  await waitForNode(/Updated to 0\.1\.0/i, { label: 'Quiet post-update row for seq2', timeoutMs: 30_000 });
  screenshot('android-updated-seq2');

  await tapNode(/What is new|Updated to 0\.1\.0/i, { label: 'Open What is new sheet' });
  await waitForNode(/Android E2E preview update to seq2\./i, { label: 'seq2 release notes' });
  await tapNode(/^Done$/i, { label: 'Dismiss What is new sheet' });
  log('Verified quiet post-update row and release notes sheet for seq2');

  // Publish seq3, download it, wait out Android 17's 30s silent-update throttle, then background Legion.
  log(`Publishing preview update set seq3 (${SET3_DIR})`);
  const pub3 = JSON.parse(legionNode(['update.publish', `from=${SET3_DIR}`, '--json']));
  assert.equal(pub3.channel, 'preview');

  await openSettingsAbout();
  const check3 = dumpUiNodes().find((n) => n._w > 0 && /^Check now$/i.test(n.text || n['content-desc'] || ''));
  if (check3) {
    tap(check3._cx, check3._cy);
  }
  await waitForNode(/^Update now$/i, { label: 'Update now ready for seq3', timeoutMs: 45_000 });
  log('seq3 downloaded and verified; returning to Machines before backgrounding');

  // Return to Machines so no Settings sheet is open.
  adb(['shell', 'input', 'keyevent', 'KEYCODE_BACK']);
  await sleep(400);
  adb(['shell', 'input', 'keyevent', 'KEYCODE_BACK']);
  await sleep(400);

  const throttleWaitMs = Math.max(0, 36_000 - (Date.now() - seq2InstalledAt));
  if (throttleWaitMs > 0) {
    log(`Waiting ${Math.ceil(throttleWaitMs / 1000)}s for Android SilentUpdatePolicy 30s throttle`);
    await sleep(throttleWaitMs);
  }

  log('Sending Legion to background (KEYCODE_HOME) to trigger silent background self-update');
  adb(['shell', 'input', 'keyevent', 'KEYCODE_HOME']);
  await sleep(1000);

  const bgDeadline = Date.now() + 50_000;
  let seq3Applied = false;
  while (Date.now() < bgDeadline) {
    const focus = focusedWindowLine();
    assert.ok(
      !focus.includes(`${APP_ID}/`),
      `Background silent update unexpectedly brought Legion to foreground: ${focus}`,
    );
    assert.ok(
      !focus.includes('packageinstaller'),
      `Background silent update unexpectedly opened PackageInstaller dialog: ${focus}`,
    );
    if (installedVersionCode() === VC3) {
      seq3Applied = true;
      break;
    }
    await sleep(500);
  }
  assert.ok(seq3Applied, `Timed out waiting for background silent self-update to reach versionCode=${VC3}`);
  // Confirm Legion STILL stays in the background for 3 seconds after replacement.
  for (let i = 0; i < 6; i++) {
    await sleep(500);
    const focus = focusedWindowLine();
    assert.ok(
      !focus.includes(`${APP_ID}/`),
      `Legion unexpectedly jumped to foreground after background silent install: ${focus}`,
    );
  }
  log(`Verified seq2 -> seq3 installed silently in background (versionCode=${VC3}) without foregrounding`);

  // Launch seq3 and confirm post-update notice + seq3 notes.
  launchApp();
  await waitForReady();
  await waitForNode(/Updated to 0\.1\.0/i, { label: 'Quiet post-update row for seq3', timeoutMs: 25_000 });
  screenshot('android-updated-seq3-silent');
  await tapNode(/What is new|Updated to 0\.1\.0/i, { label: 'Open seq3 What is new sheet' });
  await waitForNode(/Android E2E background silent update to seq3\./i, { label: 'seq3 release notes' });
  await tapNode(/^Done$/i, { label: 'Dismiss seq3 What is new sheet' });
  log('All Android self-update E2E assertions passed');
}

main().catch((err) => {
  try {
    screenshot('android-updates-failure');
  } catch {
    // Ignore screenshot capture failure during teardown.
  }
  try {
    const pid = adb(['shell', 'pidof', APP_ID]).trim();
    log(`On failure: versionCode=${installedVersionCode()} pid=${pid || '(none)'} focus=${focusedWindowLine()}`);
    const logs = adb([
      'logcat',
      '-d',
      '-t',
      '120',
      'legion-app:V',
      'AndroidRuntime:E',
      'DEBUG:F',
      'ActivityManager:I',
      '*:S',
    ]);
    process.stderr.write(`--- Recent logcat ---\n${logs}\n--- End logcat ---\n`);
  } catch {
    // Ignore logcat capture failure during teardown.
  }
  console.error(err);
  process.exit(1);
});
