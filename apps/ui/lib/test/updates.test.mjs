import assert from 'node:assert/strict';
import test from 'node:test';

import {
  IDLE_INSTALL_DELAY_MS,
  RESUME_CHECK_INTERVAL_MS,
  describeUpdateSummary,
  formatDownloadPercent,
  formatUpdateTechnicalRows,
  isSafeForAutoInstall,
  shouldCheckOnResume,
} from '../updates.js';

test('shouldCheckOnResume enforces a 1-hour interval', () => {
  const now = Date.UTC(2026, 10, 25, 10, 0, 0);
  assert.equal(shouldCheckOnResume(null, now), true);
  assert.equal(shouldCheckOnResume(now - RESUME_CHECK_INTERVAL_MS + 1000, now), false);
  assert.equal(shouldCheckOnResume(now - RESUME_CHECK_INTERVAL_MS, now), true);
  assert.equal(shouldCheckOnResume('2026-11-25T08:30:00Z', now), true);
  assert.equal(shouldCheckOnResume('2026-11-25T09:30:00Z', now), false);
});

test('isSafeForAutoInstall never interrupts active user work or Android prompt-required installs', () => {
  const now = 1_000_000;
  const readyDesktop = {
    phase: 'ready',
    auto_update: true,
    can_self_install: true,
    current: { os: 'linux', install_kind: 'appimage' },
  };
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyDesktop,
      lastInteractionMs: now - IDLE_INSTALL_DELAY_MS - 1,
      nowMs: now,
    }),
    true,
  );
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyDesktop,
      lastInteractionMs: now - 5_000,
      nowMs: now,
    }),
    false,
  );
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyDesktop,
      hasOpenSheet: true,
      lastInteractionMs: 0,
      nowMs: now,
    }),
    false,
  );
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyDesktop,
      hasCommandDraft: true,
      lastInteractionMs: 0,
      nowMs: now,
    }),
    false,
  );
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyDesktop,
      startup: true,
    }),
    true,
  );

  // Android without silent self-update must wait for user tap rather than popping an unexpected OS dialog.
  const readyAndroidPrompt = {
    phase: 'ready',
    auto_update: true,
    can_self_install: true,
    current: { os: 'android', install_kind: 'apk' },
    android: { supportsSilentSelfUpdate: false },
  };
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyAndroidPrompt,
      lastInteractionMs: 0,
      nowMs: now,
    }),
    false,
  );

  const readyAndroidSilent = {
    ...readyAndroidPrompt,
    android: { supportsSilentSelfUpdate: true },
  };
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyAndroidSilent,
      lastInteractionMs: 0,
      nowMs: now,
    }),
    true,
  );
});

test('describeUpdateSummary renders plain words for browser door, app states, and Linux deb', () => {
  const browser = describeUpdateSummary(null, { doorKind: 'http', nodeName: 'tower' });
  assert.equal(browser.line, 'Always up to date');
  assert.match(browser.detail, /always as new as tower/);
  assert.equal(browser.showCheck, false);

  const downloading = describeUpdateSummary({
    phase: 'downloading',
    auto_update: true,
    can_self_install: true,
    current: { os: 'android' },
    available: { version: '0.2.0', notes: 'Faster startup.' },
    progress: { downloaded_bytes: 512, total_bytes: 1024 },
  });
  assert.equal(downloading.line, 'Downloading… 50%');
  assert.equal(downloading.notes, 'Faster startup.');
  assert.equal(formatDownloadPercent({ downloaded_bytes: 512, total_bytes: 1024 }), 50);

  const deb = describeUpdateSummary({
    phase: 'manual',
    auto_update: true,
    can_self_install: false,
    manual_command: 'sudo apt-get install /home/me/.local/share/legion/legion_0.2.0.deb',
    current: { os: 'linux', install_kind: 'deb' },
    available: { version: '0.2.0', notes: 'Linux fixes.' },
  });
  assert.equal(deb.line, 'Version 0.2.0 is available');
  assert.equal(deb.canAction, false);
  assert.match(deb.manualCommand, /^sudo apt-get install /);

  const desktopReady = describeUpdateSummary({
    phase: 'ready',
    auto_update: true,
    can_self_install: true,
    current: { os: 'linux', install_kind: 'appimage' },
    available: { version: '0.2.0', notes: 'New features.' },
  });
  assert.equal(desktopReady.line, 'Restart to finish');
  assert.equal(desktopReady.actionLabel, 'Update now');
  assert.equal(desktopReady.canAction, true);
});

test('formatUpdateTechnicalRows includes channel, build, sequence, key, and source node', () => {
  const rows = formatUpdateTechnicalRows({
    sequence_floor: 202611250200,
    checked_nodes: ['tower', 'pi'],
    current: {
      build: 'abc123456789',
      sequence: 202611250200,
      os: 'android',
      arch: 'aarch64',
      install_kind: 'apk',
      preview_enabled: true,
    },
    available: {
      channel: 'preview',
      version: '0.2.0',
      build: 'def987654321',
      sequence: 202611250205,
      signer: 'preview',
      key_fingerprint: 'SHA256:testkey',
      source_node: 'tower',
    },
  });
  const byKey = Object.fromEntries(rows.map((r) => [r.key, r.value]));
  assert.equal(byKey.channel, 'preview + stable');
  assert.equal(byKey.build, 'abc123456789');
  assert.equal(byKey.sequence, '202611250200');
  assert.equal(byKey['signed by'], 'preview (SHA256:testkey)');
  assert.equal(byKey.from, 'tower');
  assert.equal(byKey.checked, 'tower, pi');
});
