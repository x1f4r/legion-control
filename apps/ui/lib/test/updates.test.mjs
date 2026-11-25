import assert from 'node:assert/strict';
import test from 'node:test';

import {
  ERROR_RETRY_INTERVAL_MS,
  RESUME_CHECK_INTERVAL_MS,
  countActiveTransfers,
  describeUpdateSummary,
  formatDownloadPercent,
  formatUpdateTechnicalRows,
  isSafeForAutoInstall,
  shouldCheckOnResume,
} from '../updates.js';

test('shouldCheckOnResume enforces a 1-hour interval normally and 5 minutes after an error', () => {
  const now = Date.UTC(2026, 10, 25, 10, 0, 0);
  assert.equal(shouldCheckOnResume(null, now), true);
  assert.equal(shouldCheckOnResume(now - RESUME_CHECK_INTERVAL_MS + 1000, now), false);
  assert.equal(shouldCheckOnResume(now - RESUME_CHECK_INTERVAL_MS, now), true);
  assert.equal(shouldCheckOnResume('2026-11-25T08:30:00Z', now), true);
  assert.equal(shouldCheckOnResume('2026-11-25T09:30:00Z', now), false);
  // Transient error retries much sooner (5 minutes) instead of waiting 1 hour.
  assert.equal(shouldCheckOnResume(now - ERROR_RETRY_INTERVAL_MS + 1000, now, { phase: 'error' }), false);
  assert.equal(shouldCheckOnResume(now - ERROR_RETRY_INTERVAL_MS, now, { phase: 'error' }), true);
});

test('isSafeForAutoInstall only allows startup before interaction or silent Android backgrounding', () => {
  const readyDesktop = {
    phase: 'ready',
    auto_update: true,
    can_self_install: true,
    current: { os: 'linux', install_kind: 'appimage', sequence: 202611250200 },
  };
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyDesktop,
      trigger: 'startup',
      userInteracted: false,
    }),
    true,
  );
  // Once the user interacts, startup auto-install must never interrupt them.
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyDesktop,
      trigger: 'startup',
      userInteracted: true,
    }),
    false,
  );
  // Once the first full screen has shown, startup auto-install must never interrupt the session.
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyDesktop,
      trigger: 'startup',
      userInteracted: false,
      firstScreenShown: true,
    }),
    false,
  );
  // Desktop leaving/blur does not auto-install via visibilitychange (window-close runs in Rust).
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyDesktop,
      trigger: 'background',
    }),
    false,
  );
  // Unknown triggers (like an old idle timer) are never allowed.
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyDesktop,
      trigger: 'idle',
    }),
    false,
  );
  // Active work or open sheets always block auto-install.
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyDesktop,
      trigger: 'startup',
      hasOpenSheet: true,
    }),
    false,
  );
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyDesktop,
      trigger: 'startup',
      hasCommandDraft: true,
    }),
    false,
  );
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyDesktop,
      trigger: 'startup',
      activeCalls: 1,
    }),
    false,
  );
  // Active foreground resume session recovery always blocks auto-install (startup and Android background).
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyDesktop,
      trigger: 'startup',
      isResuming: true,
    }),
    false,
  );
  assert.equal(
    isSafeForAutoInstall({
      updateState: {
        ...readyDesktop,
        current: { os: 'android', install_kind: 'apk', sequence: 202611250200 },
        android: { supportsSilentSelfUpdate: true },
      },
      trigger: 'background',
      isResuming: true,
    }),
    false,
  );
  // Sequence-0 builds and previously failed updates never auto-install.
  assert.equal(
    isSafeForAutoInstall({
      updateState: {
        ...readyDesktop,
        current: { ...readyDesktop.current, sequence: 0 },
      },
      trigger: 'startup',
    }),
    false,
  );
  assert.equal(
    isSafeForAutoInstall({
      updateState: {
        ...readyDesktop,
        failed_update: { version: '0.2.0', sequence: 202611250205, reason: 'failed' },
      },
      trigger: 'startup',
    }),
    false,
  );

  // Android without silent self-update must wait for user tap rather than popping an unexpected OS dialog.
  const readyAndroidPrompt = {
    phase: 'ready',
    auto_update: true,
    can_self_install: true,
    current: { os: 'android', install_kind: 'apk', sequence: 202611250200 },
    android: { supportsSilentSelfUpdate: false },
  };
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyAndroidPrompt,
      trigger: 'background',
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
      trigger: 'background',
    }),
    true,
  );
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyAndroidSilent,
      trigger: 'startup',
      userInteracted: false,
    }),
    true,
  );

  // Transfers in queued, running, OR cancelling state all count as active work blocking updates
  // until file removal finishes and the transfer reaches a terminal state.
  assert.equal(
    countActiveTransfers([
      { id: '1', state: 'done' },
      { id: '2', state: 'failed' },
      { id: '3', state: 'cancelled' },
      { id: '4', state: 'paused' },
    ]),
    0,
  );
  assert.equal(
    countActiveTransfers([{ id: '1', state: 'cancelling' }]),
    1,
  );
  assert.equal(
    countActiveTransfers([
      { id: '1', state: 'queued' },
      { id: '2', state: 'running' },
      { id: '3', state: 'cancelling' },
      { id: '4', state: 'cancelled' },
    ]),
    3,
  );
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyDesktop,
      trigger: 'startup',
      activeCalls: countActiveTransfers([{ id: '1', state: 'cancelling' }]),
    }),
    false,
  );
  assert.equal(
    isSafeForAutoInstall({
      updateState: readyAndroidSilent,
      trigger: 'background',
      activeCalls: countActiveTransfers([{ id: '1', state: 'cancelling' }]),
    }),
    false,
  );
});

test('describeUpdateSummary renders plain words, always-true Android copy, metered pause, deb checksum, and Try again', () => {
  const browser = describeUpdateSummary(null, { doorKind: 'http', nodeName: 'tower' });
  assert.equal(browser.line, 'Always up to date');
  assert.match(browser.detail, /always as new as tower/);
  assert.equal(browser.showCheck, false);

  const downloading = describeUpdateSummary({
    phase: 'downloading',
    auto_update: true,
    can_self_install: true,
    current: { os: 'android', sequence: 202611250200 },
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
    manual_command: 'sudo apt-get install /home/me/.local/share/legion/verified/legion_0.2.0.deb',
    manual_sha256: 'a'.repeat(64),
    current: { os: 'linux', install_kind: 'deb', sequence: 202611250200 },
    available: { version: '0.2.0', notes: 'Linux fixes.' },
  });
  assert.equal(deb.line, 'Version 0.2.0 is available');
  assert.equal(deb.canAction, false);
  assert.match(deb.manualCommand, /^sudo apt-get install /);
  assert.equal(deb.manualSha256, 'a'.repeat(64));
  assert.match(deb.detail, /private folder/);

  const desktopReady = describeUpdateSummary({
    phase: 'ready',
    auto_update: true,
    can_self_install: true,
    current: { os: 'linux', install_kind: 'appimage', sequence: 202611250200 },
    available: { version: '0.2.0', notes: 'New features.' },
  });
  assert.equal(desktopReady.line, 'Restart to finish');
  assert.equal(desktopReady.actionLabel, 'Update now');
  assert.equal(desktopReady.canAction, true);

  // Android copy never promises "without asking again" — it is always true across Android versions.
  const androidFirst = describeUpdateSummary({
    phase: 'ready',
    auto_update: true,
    can_self_install: true,
    current: { os: 'android', install_kind: 'apk', sequence: 202611250200 },
    android: { canRequestPackageInstalls: false, supportsSilentSelfUpdate: false },
    available: { version: '0.2.0', notes: 'Android update.' },
  });
  assert.equal(androidFirst.line, 'Version 0.2.0 is ready');
  assert.match(androidFirst.detail, /allow Legion to install updates/);
  assert.match(androidFirst.detail, /may occasionally ask you to confirm/);

  const androidSilent = describeUpdateSummary({
    phase: 'ready',
    auto_update: true,
    can_self_install: true,
    current: { os: 'android', install_kind: 'apk', sequence: 202611250200 },
    android: { canRequestPackageInstalls: true, supportsSilentSelfUpdate: true },
    available: { version: '0.2.0', notes: 'Android update.' },
  });
  assert.equal(androidSilent.detail, 'Android may occasionally ask you to confirm an update.');
  assert.ok(!androidSilent.detail.includes('without asking again'));

  const metered = describeUpdateSummary({
    phase: 'available',
    auto_update: true,
    can_self_install: true,
    metered_paused: true,
    current: { os: 'android', install_kind: 'apk', sequence: 202611250200 },
    available: { version: '0.2.0', notes: 'Big update.' },
  });
  assert.match(metered.detail, /metered connection/i);
  assert.equal(metered.actionLabel, 'Update now');

  const failed = describeUpdateSummary({
    phase: 'error',
    auto_update: true,
    can_self_install: true,
    current: { os: 'android', install_kind: 'apk', sequence: 202611250200 },
    available: { version: '0.2.0', notes: 'Retryable.' },
    failed_update: { version: '0.2.0', sequence: 202611250205, reason: 'Did not finish.' },
    last_error: 'Version 0.2.0 did not finish installing.',
    last_error_detail: 'RAW_OS_CODE_99',
  });
  assert.equal(failed.line, 'Version 0.2.0 did not finish installing.');
  assert.equal(failed.actionLabel, 'Try again');
  assert.ok(!failed.line.includes('RAW_OS_CODE_99'));
  assert.ok(!failed.detail.includes('RAW_OS_CODE_99'));

  const seqZero = describeUpdateSummary({
    phase: 'unsupported',
    auto_update: true,
    can_self_install: false,
    current: { os: 'linux', install_kind: 'appimage', sequence: 0 },
    unsupported_reason: 'This build has sequence 0, so self-updates are disabled.',
  });
  assert.equal(seqZero.line, 'Updates disabled in this build');
  assert.equal(seqZero.showAutoSwitch, false);
});

test('formatUpdateTechnicalRows includes channel, build, sequence (including seq 0), key, source, failed, and raw error detail', () => {
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
    failed_update: { version: '0.2.0', sequence: 202611250205, reason: 'failed' },
    last_error_detail: 'tower: connection reset by peer',
  });
  const byKey = Object.fromEntries(rows.map((r) => [r.key, r.value]));
  assert.equal(byKey.channel, 'preview + stable');
  assert.equal(byKey.build, 'abc123456789');
  assert.equal(byKey.sequence, '202611250200');
  assert.equal(byKey['signed by'], 'preview (SHA256:testkey)');
  assert.equal(byKey.from, 'tower');
  assert.equal(byKey.failed, '0.2.0 · seq 202611250205');
  assert.equal(byKey.checked, 'tower, pi');
  assert.equal(byKey['last error'], 'tower: connection reset by peer');

  const seqZeroRows = formatUpdateTechnicalRows({
    current: { sequence: 0, os: 'linux', arch: 'x86_64', install_kind: 'appimage' },
  });
  const seqRow = seqZeroRows.find((r) => r.key === 'sequence');
  assert.equal(seqRow?.value, '0 (updates disabled)');

  // Skipped after two rollbacks surfaces plain message + Try again in summary and attempt count +
  // captured startup output under Technical details.
  const skippedState = {
    phase: 'error',
    auto_update: true,
    can_self_install: true,
    current: { os: 'linux', arch: 'x86_64', install_kind: 'appimage', sequence: 202611250200 },
    available: { version: '0.2.0', sequence: 202611250205, channel: 'preview', build: 'b2', signer: 'preview', key_fingerprint: 'SHA256:k', source_node: 'tower' },
    failed_update: {
      version: '0.2.0',
      sequence: 202611250205,
      reason: 'The new version did not start cleanly, so Legion kept the previous copy.',
      attempts: 2,
      skipped: true,
      detail: 'new_pid=42 exited with rc=127: missing libfoo.so',
    },
  };
  const skippedSummary = describeUpdateSummary(skippedState);
  assert.equal(skippedSummary.line, 'The new version did not start cleanly, so Legion kept the previous copy.');
  assert.equal(skippedSummary.actionLabel, 'Try again');
  assert.ok(!skippedSummary.line.includes('libfoo.so'));
  const skippedRows = Object.fromEntries(formatUpdateTechnicalRows(skippedState).map((r) => [r.key, r.value]));
  assert.equal(skippedRows.failed, '0.2.0 · seq 202611250205 (2 attempts, skipped)');
  assert.equal(skippedRows['last error'], 'new_pid=42 exited with rc=127: missing libfoo.so');

  // First rollback (not yet skipped) indicates retry-once status in Technical details.
  const firstRollbackRows = Object.fromEntries(
    formatUpdateTechnicalRows({
      current: { os: 'linux', arch: 'x86_64', install_kind: 'appimage', sequence: 202611250200 },
      failed_update: {
        version: '0.2.0',
        sequence: 202611250205,
        reason: 'The new version did not start cleanly, so Legion kept the previous copy.',
        attempts: 1,
        skipped: false,
      },
    }).map((r) => [r.key, r.value]),
  );
  assert.equal(firstRollbackRows.failed, '0.2.0 · seq 202611250205 (attempt 1, will retry once)');
});
