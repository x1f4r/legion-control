// Pure formatting, cadence, and safe-idle helpers for Legion's Updates section.

export const RESUME_CHECK_INTERVAL_MS = 60 * 60 * 1000;
export const PERIODIC_CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;
export const IDLE_INSTALL_DELAY_MS = 60 * 1000;

/** Returns true when enough time (>= 1 hour) has passed to re-check on foreground resume. */
export function shouldCheckOnResume(lastCheckedAt, nowMs = Date.now()) {
  if (!lastCheckedAt) return true;
  const ts = typeof lastCheckedAt === 'number' ? lastCheckedAt : Date.parse(lastCheckedAt);
  if (!Number.isFinite(ts)) return true;
  return nowMs - ts >= RESUME_CHECK_INTERVAL_MS;
}

/**
 * Returns true only when it is safe to trigger an automatic update install without interrupting
 * anything the person started.
 */
export function isSafeForAutoInstall({
  updateState,
  hasOpenSheet = false,
  hasCommandDraft = false,
  isRunningCommand = false,
  isPairingOpen = false,
  lastInteractionMs = 0,
  nowMs = Date.now(),
  startup = false,
} = {}) {
  if (!updateState || !updateState.auto_update) return false;
  if (updateState.phase !== 'ready' || !updateState.can_self_install) return false;
  if (hasOpenSheet || hasCommandDraft || isRunningCommand || isPairingOpen) return false;
  // On Android, never trigger background auto-install if Android would pop an unexpected prompt.
  if (
    updateState.current?.os === 'android' &&
    !updateState.android?.supportsSilentSelfUpdate
  ) {
    return false;
  }
  if (startup) return true;
  return nowMs - lastInteractionMs >= IDLE_INSTALL_DELAY_MS;
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
 * Returns `{ line, detail, actionLabel, canAction, showCheck, showAutoSwitch, manualCommand, notes }`.
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
      notes: '',
    };
  }

  const avail = updateState.available || null;
  const phase = updateState.phase || 'up_to_date';
  const notes = avail?.notes || '';
  const isAndroid = updateState.current?.os === 'android';

  let androidNote = '';
  if (isAndroid && avail && (phase === 'ready' || phase === 'available' || phase === 'needs_permission')) {
    if (updateState.android?.supportsSilentSelfUpdate) {
      androidNote = 'Android can apply this update without asking again.';
    } else if (updateState.android && !updateState.android.canRequestPackageInstalls) {
      androidNote =
        'Android will ask you to allow Legion to install updates once, and to confirm this update.';
    } else {
      androidNote = 'Android may ask you to confirm this update.';
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
      notes,
    };
  }

  if (phase === 'manual') {
    return {
      line: avail ? `Version ${avail.version} is available` : 'A newer version is available',
      detail: 'This copy is managed by your system package manager. Run this command to update:',
      actionLabel: null,
      canAction: false,
      showCheck: true,
      showAutoSwitch: false,
      manualCommand: updateState.manual_command || null,
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
      notes,
    };
  }

  if (phase === 'ready') {
    const desktopAutoReady = !isAndroid && updateState.auto_update;
    return {
      line: desktopAutoReady
        ? 'Restart to finish'
        : avail
          ? `Version ${avail.version} is ready`
          : 'Update is ready',
      detail: desktopAutoReady
        ? avail
          ? `Version ${avail.version} is downloaded and verified.`
          : 'The update is downloaded and verified.'
        : androidNote,
      actionLabel: 'Update now',
      canAction: Boolean(updateState.can_self_install),
      showCheck: false,
      showAutoSwitch: true,
      manualCommand: null,
      notes,
    };
  }

  if (phase === 'available') {
    return {
      line: avail ? `Version ${avail.version} is available` : 'An update is available',
      detail: updateState.can_self_install
        ? androidNote
        : updateState.unsupported_reason || 'This copy cannot replace itself in place.',
      actionLabel: updateState.can_self_install ? 'Update now' : null,
      canAction: Boolean(updateState.can_self_install),
      showCheck: true,
      showAutoSwitch: Boolean(updateState.can_self_install),
      manualCommand: null,
      notes,
    };
  }

  if (phase === 'unsupported') {
    return {
      line: 'Managed outside Legion',
      detail: updateState.unsupported_reason || 'This platform installs updates through its store.',
      actionLabel: null,
      canAction: false,
      showCheck: false,
      showAutoSwitch: false,
      manualCommand: null,
      notes: '',
    };
  }

  if (phase === 'error') {
    return {
      line: updateState.last_error || 'Could not finish the update.',
      detail: avail ? `Version ${avail.version}` : '',
      actionLabel: avail && updateState.can_self_install ? 'Update now' : null,
      canAction: Boolean(avail && updateState.can_self_install),
      showCheck: true,
      showAutoSwitch: Boolean(updateState.can_self_install),
      manualCommand: null,
      notes,
    };
  }

  // up_to_date (note: unreachable fleet machines stay quiet per design)
  return {
    line: 'Up to date',
    detail: '',
    actionLabel: null,
    canAction: false,
    showCheck: true,
    showAutoSwitch: Boolean(updateState.can_self_install),
    manualCommand: null,
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
  rows.push({
    key: 'sequence',
    value:
      updateState.sequence_floor && updateState.sequence_floor > (cur.sequence || 0)
        ? `${cur.sequence || 0} (floor ${updateState.sequence_floor})`
        : String(cur.sequence || 0),
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
  if (Array.isArray(updateState.checked_nodes) && updateState.checked_nodes.length > 0) {
    rows.push({ key: 'checked', value: updateState.checked_nodes.join(', ') });
  }
  return rows;
}
