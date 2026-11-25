// Pure formatting, cadence, and auto-install safety helpers for Legion's Updates section.

export const RESUME_CHECK_INTERVAL_MS = 60 * 60 * 1000;
export const PERIODIC_CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;
export const ERROR_RETRY_INTERVAL_MS = 5 * 60 * 1000;

/**
 * Returns true when enough time has passed to re-check on foreground resume (>= 1 hour normally,
 * or >= 5 minutes when the previous check ended in an error).
 */
export function shouldCheckOnResume(lastCheckedAt, nowMs = Date.now(), { phase } = {}) {
  if (!lastCheckedAt) return true;
  const ts = typeof lastCheckedAt === 'number' ? lastCheckedAt : Date.parse(lastCheckedAt);
  if (!Number.isFinite(ts)) return true;
  const interval = phase === 'error' ? ERROR_RETRY_INTERVAL_MS : RESUME_CHECK_INTERVAL_MS;
  return nowMs - ts >= interval;
}

/**
 * Counts device transfers that represent active work blocking an app update:
 * `queued`, `running`, and `cancelling` (still stopping workers or deleting leftover files).
 */
export function countActiveTransfers(transfers) {
  if (!Array.isArray(transfers)) return 0;
  return transfers.filter(
    (t) => t && (t.state === 'running' || t.state === 'queued' || t.state === 'cancelling'),
  ).length;
}

/**
 * Returns true only when it is safe to trigger an automatic update install without interrupting
 * anything the person started.
 *
 * Automatic installs only ever run at two moments:
 * - `trigger === 'startup'`: on launch within the native startup window, before the first full
 *   screen is shown and before the person has interacted with the app.
 * - `trigger === 'background'`: on Android when the app moves into the background (strictly silent;
 *   desktop window-close updates are triggered natively on `WindowEvent::CloseRequested`).
 */
export function isSafeForAutoInstall({
  updateState,
  trigger = 'startup',
  hasOpenSheet = false,
  hasCommandDraft = false,
  isRunningCommand = false,
  activeCalls = 0,
  isPairingOpen = false,
  isResuming = false,
  userInteracted = false,
  firstScreenShown = false,
} = {}) {
  if (!updateState || !updateState.auto_update) return false;
  if (updateState.phase !== 'ready' || !updateState.can_self_install) return false;
  if ((updateState.current?.sequence || 0) === 0) return false;
  if (updateState.failed_update) return false;
  if (
    hasOpenSheet ||
    hasCommandDraft ||
    isRunningCommand ||
    activeCalls > 0 ||
    isPairingOpen ||
    isResuming
  ) {
    return false;
  }
  const isAndroid = updateState.current?.os === 'android';
  // On Android, never trigger automatic install if Android would pop an unexpected prompt.
  if (isAndroid && !updateState.android?.supportsSilentSelfUpdate) {
    return false;
  }
  if (trigger === 'startup') {
    return !userInteracted && !firstScreenShown;
  }
  if (trigger === 'background') {
    return isAndroid;
  }
  return false;
}

/** Format byte progress cleanly as a percentage when total is known. */
export function formatDownloadPercent(progress) {
  if (!progress || !progress.total_bytes || progress.total_bytes <= 0) return null;
  const pct = Math.max(
    0,
    Math.min(100, Math.round((progress.downloaded_bytes / progress.total_bytes) * 100)),
  );
  return pct;
}

/**
 * Plain-language summary for Settings > About > Updates.
 *
 * Returns `{ line, detail, actionLabel, canAction, showCheck, showAutoSwitch, manualCommand, manualSha256, notes }`.
 */
export function describeUpdateSummary(updateState, { doorKind = 'app', nodeName = '' } = {}) {
  if (doorKind !== 'app' && !updateState) {
    const host = nodeName || 'the machine serving it';
    return {
      line: 'Always up to date',
      detail: `The browser interface is always as new as ${host}.`,
      actionLabel: null,
      canAction: false,
      showCheck: false,
      showAutoSwitch: false,
      manualCommand: null,
      manualSha256: null,
      notes: '',
    };
  }

  if (!updateState) {
    return {
      line: 'Up to date',
      detail: '',
      actionLabel: null,
      canAction: false,
      showCheck: true,
      showAutoSwitch: false,
      manualCommand: null,
      manualSha256: null,
      notes: '',
    };
  }

  const avail = updateState.available || null;
  const phase = updateState.phase || 'up_to_date';
  const notes = avail?.notes || '';
  const isAndroid = updateState.current?.os === 'android';

  let androidNote = '';
  if (isAndroid && avail && (phase === 'ready' || phase === 'available' || phase === 'needs_permission')) {
    if (updateState.android && !updateState.android.canRequestPackageInstalls) {
      androidNote =
        'Android asks once to allow Legion to install updates, and may occasionally ask you to confirm.';
    } else {
      androidNote = 'Android may occasionally ask you to confirm an update.';
    }
  }

  if (phase === 'checking') {
    return {
      line: 'Checking for updates…',
      detail: '',
      actionLabel: null,
      canAction: false,
      showCheck: false,
      showAutoSwitch: true,
      manualCommand: null,
      manualSha256: null,
      notes,
    };
  }

  if (phase === 'downloading') {
    const pct = formatDownloadPercent(updateState.progress);
    return {
      line: pct !== null ? `Downloading… ${pct}%` : 'Downloading…',
      detail: avail ? `Version ${avail.version}` : '',
      actionLabel: null,
      canAction: false,
      showCheck: false,
      showAutoSwitch: true,
      manualCommand: null,
      manualSha256: null,
      notes,
    };
  }

  if (phase === 'installing') {
    return {
      line: isAndroid ? 'Installing update…' : 'Restarting to finish…',
      detail: avail ? `Version ${avail.version}` : '',
      actionLabel: null,
      canAction: false,
      showCheck: false,
      showAutoSwitch: true,
      manualCommand: null,
      manualSha256: null,
      notes,
    };
  }

  if (phase === 'manual') {
    const hasVerifiedCopy = Boolean(updateState.manual_sha256);
    return {
      line: avail ? `Version ${avail.version} is available` : 'A newer version is available',
      detail: hasVerifiedCopy
        ? 'Legion saved and verified this package in its private folder. Run this command to install it:'
        : 'This copy is managed by your system package manager. Run this command to update:',
      actionLabel: null,
      canAction: false,
      showCheck: true,
      showAutoSwitch: false,
      manualCommand: updateState.manual_command || null,
      manualSha256: updateState.manual_sha256 || null,
      notes,
    };
  }

  if (phase === 'needs_permission') {
    return {
      line: avail ? `Version ${avail.version} is ready` : 'Permission needed to install updates',
      detail:
        updateState.last_error ||
        'Allow Legion to install updates in Android settings, then tap Update now again.',
      actionLabel: 'Update now',
      canAction: true,
      showCheck: false,
      showAutoSwitch: true,
      manualCommand: null,
      manualSha256: null,
      notes,
    };
  }

  if (phase === 'ready') {
    const desktopAutoReady = !isAndroid && updateState.auto_update;
    const readyDetail = updateState.last_error
      ? updateState.last_error
      : desktopAutoReady
        ? avail
          ? `Version ${avail.version} is downloaded and verified. It will install next time you open or close Legion.`
          : 'The update is downloaded and verified. It will install next time you open or close Legion.'
        : androidNote;
    return {
      line: desktopAutoReady
        ? 'Restart to finish'
        : avail
          ? `Version ${avail.version} is ready`
          : 'Update is ready',
      detail: readyDetail,
      actionLabel: 'Update now',
      canAction: Boolean(updateState.can_self_install),
      showCheck: false,
      showAutoSwitch: true,
      manualCommand: null,
      manualSha256: null,
      notes,
    };
  }

  if (phase === 'available') {
    const detail = updateState.metered_paused
      ? 'Paused on a metered connection because this update is over 5 MB. Tap Update now to download it anyway.'
      : updateState.can_self_install
        ? androidNote
        : updateState.unsupported_reason || 'This copy cannot replace itself in place.';
    return {
      line: avail ? `Version ${avail.version} is available` : 'An update is available',
      detail,
      actionLabel: updateState.can_self_install ? 'Update now' : null,
      canAction: Boolean(updateState.can_self_install),
      showCheck: true,
      showAutoSwitch: Boolean(updateState.can_self_install),
      manualCommand: null,
      manualSha256: null,
      notes,
    };
  }

  if (phase === 'unsupported') {
    const isSeqZero = (updateState.current?.sequence || 0) === 0;
    return {
      line: isSeqZero ? 'Updates disabled in this build' : 'Managed outside Legion',
      detail:
        updateState.unsupported_reason ||
        (isSeqZero
          ? 'This build has sequence 0, so self-updates are disabled.'
          : 'This platform installs updates through its store.'),
      actionLabel: null,
      canAction: false,
      showCheck: false,
      showAutoSwitch: false,
      manualCommand: null,
      manualSha256: null,
      notes: '',
    };
  }

  if (phase === 'error') {
    const failed = updateState.failed_update || null;
    const actionLabel =
      avail && updateState.can_self_install
        ? failed
          ? 'Try again'
          : 'Update now'
        : null;
    const line =
      updateState.last_error ||
      failed?.reason ||
      (failed
        ? 'The new version did not start cleanly, so Legion kept the previous copy.'
        : 'Could not finish the update.');
    return {
      line,
      detail: avail ? `Version ${avail.version}` : '',
      actionLabel,
      canAction: Boolean(actionLabel),
      showCheck: true,
      showAutoSwitch: Boolean(updateState.can_self_install),
      manualCommand: null,
      manualSha256: null,
      notes,
    };
  }

  // up_to_date (unreachable fleet machines stay quiet; rebuilt corrupt floor surfaces calmly)
  return {
    line: 'Up to date',
    detail: updateState.last_error || '',
    actionLabel: null,
    canAction: false,
    showCheck: true,
    showAutoSwitch: Boolean(updateState.can_self_install),
    manualCommand: null,
    manualSha256: null,
    notes: '',
  };
}

/** Technical details rows shown ONLY when Technical details is switched on. */
export function formatUpdateTechnicalRows(updateState) {
  if (!updateState) return [];
  const rows = [];
  const cur = updateState.current || {};
  const avail = updateState.available || null;

  rows.push({
    key: 'channel',
    value: cur.preview_enabled ? 'preview + stable' : 'stable',
  });
  if (cur.build) {
    rows.push({ key: 'build', value: cur.build });
  }
  const seq = cur.sequence || 0;
  rows.push({
    key: 'sequence',
    value:
      seq === 0
        ? '0 (updates disabled)'
        : updateState.sequence_floor && updateState.sequence_floor > seq
          ? `${seq} (floor ${updateState.sequence_floor})`
          : String(seq),
  });
  if (cur.install_kind) {
    rows.push({
      key: 'install',
      value: `${cur.install_kind} (${cur.os || '?'}/${cur.arch || '?'})`,
    });
  }
  if (avail) {
    rows.push({
      key: 'next',
      value: `${avail.version} · ${avail.channel} · seq ${avail.sequence} (${avail.build})`,
    });
    rows.push({
      key: 'signed by',
      value: `${avail.signer} (${avail.key_fingerprint})`,
    });
    rows.push({
      key: 'from',
      value: avail.source_node,
    });
  } else {
    if (cur.release_key_fingerprint) {
      rows.push({ key: 'release key', value: cur.release_key_fingerprint });
    }
    if (cur.preview_key_fingerprint) {
      rows.push({ key: 'preview key', value: cur.preview_key_fingerprint });
    }
  }
  if (updateState.failed_update) {
    const f = updateState.failed_update;
    let suffix = '';
    if (typeof f.attempts === 'number' && f.attempts > 0) {
      if (f.skipped) {
        suffix = ` (${f.attempts} ${f.attempts === 1 ? 'attempt' : 'attempts'}, skipped)`;
      } else {
        suffix = ` (attempt ${f.attempts}, will retry once)`;
      }
    }
    rows.push({
      key: 'failed',
      value: `${f.version} · seq ${f.sequence}${suffix}`,
    });
  }
  if (Array.isArray(updateState.checked_nodes) && updateState.checked_nodes.length > 0) {
    rows.push({ key: 'checked', value: updateState.checked_nodes.join(', ') });
  }
  const errorDetail =
    updateState.last_error_detail ||
    updateState.failed_update?.detail ||
    updateState.rolled_back?.detail ||
    null;
  if (errorDetail) {
    rows.push({ key: 'last error', value: errorDetail });
  }
  return rows;
}
