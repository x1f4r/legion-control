// Shared device-pairing controller owned by App so leaving a pairing screen mid-request
// never silently cancels pairing, scanned and pasted invites are kept across uncertain or
// failed attempts, and post-send cancellations are never swallowed.

/**
 * Extracts the target node name from a `legion-invite-1.<base64url>` string for display.
 */
export function inviteTargetNode(text) {
  const trimmed = String(text || '').trim();
  if (!trimmed.startsWith('legion-invite-1.')) return '';
  try {
    const b64 = trimmed.slice('legion-invite-1.'.length).replace(/-/g, '+').replace(/_/g, '/');
    const pad = (4 - (b64.length % 4)) % 4;
    let jsonText;
    if (typeof atob === 'function') {
      jsonText = atob(b64 + '='.repeat(pad));
    } else if (typeof Buffer !== 'undefined') {
      jsonText = Buffer.from(b64 + '='.repeat(pad), 'base64').toString('utf8');
    } else {
      return '';
    }
    const parsed = JSON.parse(jsonText);
    return typeof parsed?.node === 'string' && parsed.node.trim() ? parsed.node.trim() : '';
  } catch {
    return '';
  }
}

/**
 * Classifies a failed `link.pair` error into a structured pairing outcome.
 *
 * Pre-send cancellations (`code === 'cancelled' && details?.sent === false`) are quiet resets;
 * post-send cancellations (`sent !== false`) are NEVER swallowed.
 * Confirmed duplicate-key conflicts carrying `details.existing_principal` offer an explicit
 * restore flow via the pinned-key probe instead of asking for a new key.
 */
export function classifyPairError(err, nodeName = '', deviceNoun = 'device') {
  const target = nodeName || err?.machine || '';
  if (err?.code === 'cancelled') {
    if (err?.details?.sent === false) {
      return { kind: 'cancelled_before_send', message: '', retryable: false, restorable: false };
    }
    if (err?.details?.outcome === 'uncertain') {
      const message =
        (typeof err?.message === 'string' && err.message.trim()) ||
        (target
          ? `Pairing may have finished on ${target}. Try again to check.`
          : 'Pairing may have finished. Try again to check.');
      return { kind: 'uncertain', message, retryable: true, restorable: false };
    }
    const message =
      (typeof err?.message === 'string' && err.message.trim()) ||
      (target
        ? `Stopped waiting. ${target} had not finished pairing when Legion checked. If it finishes, pairing again picks it up.`
        : 'Stopped waiting. The machine had not finished pairing when Legion checked. If it finishes, pairing again picks it up.');
    return { kind: 'not_finished', message, retryable: true, restorable: false };
  }
  const existingPrincipal =
    typeof err?.details?.existing_principal === 'string'
      ? err.details.existing_principal.trim()
      : '';
  if (err?.code === 'conflict' && existingPrincipal) {
    const conflictNode =
      (typeof err?.details?.node === 'string' && err.details.node.trim()) ||
      target ||
      'this machine';
    const existingDevice =
      typeof err?.details?.existing_device === 'string' ? err.details.existing_device.trim() : '';
    const noun = String(deviceNoun || 'device').trim() || 'device';
    return {
      kind: 'already_paired',
      existingPrincipal,
      existingDevice,
      node: conflictNode,
      message: `This ${noun} is already paired with ${conflictNode} as ${existingPrincipal}.`,
      retryable: false,
      restorable: true,
    };
  }
  return { kind: 'error', error: err, retryable: true, restorable: false };
}

/**
 * Creates a pairing state controller that lives at App scope so leaving a view never aborts an
 * in-flight pairing request.
 */
export function createPairingController({
  link,
  getDeviceNoun = () => 'device',
  isOnPairingScreen = () => true,
  onPaired = null,
  onBackgroundOutcome = null,
} = {}) {
  const listeners = new Set();
  let abortController = null;
  let activePromise = null;

  let state = {
    inFlight: false,
    cancelling: false,
    invite: '',
    source: null, // 'paste' | 'scan' | null
    deviceName: '',
    node: '',
    screen: null, // 'start' | 'settings' | null
    error: null,
    notice: null, // { kind: 'warn' | 'neutral', title: string, retryable: boolean, node: string } | null
  };

  function getState() {
    return { ...state };
  }

  function emit() {
    const snap = getState();
    for (const fn of listeners) {
      try {
        fn(snap);
      } catch {
        // Ignore subscriber errors.
      }
    }
  }

  function update(patch) {
    state = { ...state, ...patch };
    emit();
  }

  function subscribe(listener) {
    if (typeof listener !== 'function') return () => {};
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  }

  function setInvite(value) {
    const text = String(value ?? '');
    if (state.inFlight) return;
    update({
      invite: text,
      source: text ? 'paste' : null,
      node: inviteTargetNode(text),
      error: null,
      notice: null,
    });
  }

  function setDeviceName(value) {
    const text = String(value ?? '');
    if (state.inFlight) return;
    update({ deviceName: text });
  }

  function clearOutcome() {
    update({ error: null, notice: null });
  }

  function startPair({
    invite,
    deviceName = state.deviceName,
    source = 'paste',
    screen = 'start',
  } = {}) {
    if (activePromise) return activePromise;
    const trimmed = String(invite ?? state.invite ?? '').trim();
    if (!trimmed) {
      return Promise.resolve({ ok: false, empty: true });
    }
    const resolvedName = String(deviceName ?? '').trim();
    const node = inviteTargetNode(trimmed);
    const controller = new AbortController();
    abortController = controller;

    update({
      inFlight: true,
      cancelling: false,
      invite: trimmed,
      source,
      deviceName: resolvedName || state.deviceName,
      node,
      screen,
      error: null,
      notice: null,
    });

    const run = (async () => {
      try {
        const next = await link.pair(trimmed, resolvedName || undefined, {
          signal: controller.signal,
        });
        const pairedNode =
          next?.last_paired?.node || node || next?.nodes?.[0]?.node || '';
        const wasCancelling = state.cancelling;
        update({
          inFlight: false,
          cancelling: false,
          invite: '',
          source: null,
          node: '',
          screen: null,
          error: null,
          notice: null,
        });
        if (typeof onPaired === 'function') {
          onPaired({
            next,
            node: pairedNode,
            startedScreen: screen,
            onPairingScreen: Boolean(isOnPairingScreen()),
            wasCancelling,
          });
        }
        return { ok: true, state: next, node: pairedNode };
      } catch (err) {
        const noun = typeof getDeviceNoun === 'function' ? getDeviceNoun() : 'device';
        const outcome = classifyPairError(err, node, noun);
        const onScreen = Boolean(isOnPairingScreen());

        if (outcome.kind === 'cancelled_before_send') {
          update({
            inFlight: false,
            cancelling: false,
            invite: source === 'scan' ? '' : state.invite,
            source: source === 'scan' ? null : state.source,
            error: null,
            notice: null,
          });
          return { ok: false, outcome };
        }

        if (outcome.kind === 'already_paired') {
          const canRestore = typeof link?.restorePairing === 'function';
          update({
            inFlight: false,
            cancelling: false,
            error: null,
            notice: {
              kind: 'warn',
              title: outcome.message,
              retryable: false,
              restorable: canRestore,
              existingPrincipal: outcome.existingPrincipal,
              node: outcome.node,
            },
          });
          if (!onScreen && typeof onBackgroundOutcome === 'function') {
            onBackgroundOutcome({
              kind: 'already_paired',
              message: outcome.message,
              node: outcome.node,
              existingPrincipal: outcome.existingPrincipal,
              retryable: false,
              restorable: canRestore,
              startedScreen: screen,
            });
          }
          return { ok: false, outcome };
        }

        if (outcome.kind === 'uncertain' || outcome.kind === 'not_finished') {
          // Keep the invite (even if scanned) so "Try again" checks the same invite.
          update({
            inFlight: false,
            cancelling: false,
            error: null,
            notice: {
              kind: 'warn',
              title: outcome.message,
              retryable: true,
              node: node || err?.machine || '',
            },
          });
          if (!onScreen && typeof onBackgroundOutcome === 'function') {
            onBackgroundOutcome({
              kind: outcome.kind,
              message: outcome.message,
              node: node || err?.machine || '',
              retryable: true,
              startedScreen: screen,
            });
          }
          return { ok: false, outcome };
        }

        // Ordinary pairing failure: keep the invite (including scanned invites) so retry works.
        update({
          inFlight: false,
          cancelling: false,
          error: err,
          notice: null,
        });
        if (!onScreen && typeof onBackgroundOutcome === 'function') {
          onBackgroundOutcome({
            kind: 'error',
            error: err,
            node: node || err?.machine || '',
            retryable: Boolean(trimmed),
            startedScreen: screen,
          });
        }
        return { ok: false, outcome };
      } finally {
        if (abortController === controller) {
          abortController = null;
        }
        if (activePromise === run) {
          activePromise = null;
        }
      }
    })();

    activePromise = run;
    return run;
  }

  function retryPair() {
    if (state.inFlight || !state.invite.trim()) {
      return Promise.resolve({ ok: false, empty: true });
    }
    return startPair({
      invite: state.invite,
      deviceName: state.deviceName,
      source: state.source || 'paste',
      screen: state.screen || 'start',
    });
  }

  function restorePair() {
    if (
      activePromise ||
      state.inFlight ||
      !state.invite.trim() ||
      !state.notice?.restorable ||
      !state.notice?.existingPrincipal ||
      typeof link?.restorePairing !== 'function'
    ) {
      return Promise.resolve({ ok: false, empty: true });
    }
    const trimmed = state.invite.trim();
    const existingPrincipal = state.notice.existingPrincipal;
    const resolvedName = String(state.deviceName ?? '').trim();
    const node = state.notice.node || state.node || inviteTargetNode(trimmed);
    const screen = state.screen || 'start';
    const controller = new AbortController();
    abortController = controller;

    update({
      inFlight: true,
      cancelling: false,
      node,
      error: null,
      notice: null,
    });

    const run = (async () => {
      try {
        const next = await link.restorePairing(
          trimmed,
          existingPrincipal,
          resolvedName || undefined,
          { signal: controller.signal },
        );
        const pairedNode =
          next?.last_paired?.node || node || next?.nodes?.[0]?.node || '';
        const wasCancelling = state.cancelling;
        update({
          inFlight: false,
          cancelling: false,
          invite: '',
          source: null,
          node: '',
          screen: null,
          error: null,
          notice: null,
        });
        if (typeof onPaired === 'function') {
          onPaired({
            next,
            node: pairedNode,
            startedScreen: screen,
            onPairingScreen: Boolean(isOnPairingScreen()),
            wasCancelling,
          });
        }
        return { ok: true, state: next, node: pairedNode, restored: true };
      } catch (err) {
        const noun = typeof getDeviceNoun === 'function' ? getDeviceNoun() : 'device';
        const outcome = classifyPairError(err, node, noun);
        const onScreen = Boolean(isOnPairingScreen());
        if (outcome.kind === 'cancelled_before_send') {
          update({
            inFlight: false,
            cancelling: false,
            error: null,
            notice: null,
          });
          return { ok: false, outcome };
        }
        update({
          inFlight: false,
          cancelling: false,
          error: err,
          notice: null,
        });
        if (!onScreen && typeof onBackgroundOutcome === 'function') {
          onBackgroundOutcome({
            kind: 'error',
            error: err,
            node: node || err?.machine || '',
            retryable: false,
            startedScreen: screen,
          });
        }
        return { ok: false, outcome };
      } finally {
        if (abortController === controller) {
          abortController = null;
        }
        if (activePromise === run) {
          activePromise = null;
        }
      }
    })();

    activePromise = run;
    return run;
  }

  function cancelPair() {
    if (!state.inFlight || state.cancelling) return;
    update({ cancelling: true });
    abortController?.abort();
  }

  return {
    getState,
    subscribe,
    setInvite,
    setDeviceName,
    clearOutcome,
    startPair,
    retryPair,
    restorePair,
    cancelPair,
  };
}
