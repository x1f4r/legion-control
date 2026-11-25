// First-start screen: bearer token entry for HTTP, or device pairing for the native app.

import { html, useEffect, useMemo, useRef, useState } from '../vendor/preact-htm.js';
import { Icon } from '../icons.js';
import { createPairingController } from '../lib/pairing.js';
import { CopyButton, Notice } from './common.js';

export function StartView({
  link,
  linkState,
  onConnected,
  onCancel,
  pairingController = null,
  pairingState = null,
}) {
  const [token, setToken] = useState('');
  const [pasteOpen, setPasteOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [localError, setLocalError] = useState(linkState?.error || null);
  const [archivedInfo, setArchivedInfo] = useState(null);
  const inviteInputRef = useRef(null);

  const fallbackController = useMemo(
    () =>
      pairingController
        ? null
        : createPairingController({
            link,
            onPaired: ({ next }) => {
              window.location.hash = '#/machines';
              onConnected?.(next);
            },
          }),
    [pairingController, link, onConnected],
  );
  const ctrl = pairingController || fallbackController;
  const [fallbackState, setFallbackState] = useState(() => ctrl.getState());
  useEffect(() => {
    if (pairingController) return undefined;
    setFallbackState(ctrl.getState());
    return ctrl.subscribe(setFallbackState);
  }, [pairingController, ctrl]);

  const pState = pairingState || fallbackState;
  const invite = pState.invite;
  const deviceName = pState.deviceName;
  const pairing = pState.inFlight;
  const cancelling = pState.cancelling;
  const pairingNode = pState.node;
  const pairNotice = pState.notice;
  const error = pState.error || localError;
  const controlsDisabled = busy || pairing;

  function cancelPairing() {
    ctrl.cancelPair();
  }

  useEffect(() => {
    if (pasteOpen && inviteInputRef.current) {
      inviteInputRef.current.focus();
      inviteInputRef.current.scrollIntoView({ block: 'nearest', behavior: 'auto' });
    }
  }, [pasteOpen]);

  const nodeName = linkState?.health?.node || 'this machine';
  const needsRepair = linkState?.needs === 'repair';
  const needsPairing = !needsRepair && (linkState?.needs === 'pairing' || link.kind === 'app');

  useEffect(() => {
    if (!needsPairing || typeof link.deviceName !== 'function') return;
    let alive = true;
    link
      .deviceName()
      .then((name) => {
        if (alive && name && !ctrl.getState().deviceName) {
          ctrl.setDeviceName(name);
        }
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [link, needsPairing, ctrl]);

  async function submitToken(e) {
    e.preventDefault();
    setLocalError(null);
    setBusy(true);
    try {
      const next = await link.setToken(token);
      setToken('');
      window.location.hash = '#/machines';
      onConnected(next);
    } catch (err) {
      setLocalError(err);
    } finally {
      setBusy(false);
    }
  }

  async function onScan() {
    setLocalError(null);
    ctrl.clearOutcome();
    setBusy(true);
    try {
      const scanned = await link.scanInvite();
      if (scanned) {
        setBusy(false);
        await ctrl.startPair({
          invite: scanned,
          deviceName: ctrl.getState().deviceName,
          source: 'scan',
          screen: 'start',
        });
      }
    } catch (err) {
      if (err?.code !== 'cancelled') {
        setLocalError(err);
      }
    } finally {
      setBusy(false);
    }
  }

  async function submitInvite(e) {
    e.preventDefault();
    setLocalError(null);
    const trimmed = invite.trim();
    if (!trimmed) return;
    await ctrl.startPair({
      invite: trimmed,
      deviceName: ctrl.getState().deviceName,
      source: 'paste',
      screen: 'start',
    });
  }

  async function onRepairStore() {
    setLocalError(null);
    setBusy(true);
    try {
      const res = await link.repairStore();
      setArchivedInfo({
        archivedTo: res?.archived_to || '',
        message: res?.message || '',
        nextState: res?.state || null,
      });
      if (res?.state && !res.state.ready) {
        onConnected(res.state);
      }
    } catch (err) {
      setLocalError(err);
    } finally {
      setBusy(false);
    }
  }

  if (needsRepair) {
    const repair = linkState?.repair || {};
    return html`
      <div class="app standalone">
        <header class="top">
          <h1>Saved state</h1>
        </header>
        <main class="main">
          <div class="start-column">
            <h2 class="start-heading">Saved device state could not be read.</h2>
            <p class="lede">
              Legion stopped before writing anything so your existing device key and pairings are
              not overwritten.
            </p>
            <div class="notice bad">
              <strong>${repair.message || 'The saved state file is damaged or from a newer version.'}</strong>
              ${repair.path ? html`<span>File: <code>${repair.path}</code></span>` : null}
            </div>
            ${error ? html`<${Notice} error=${error} />` : null}
            <div class="actions">
              <button type="button" class="btn primary" disabled=${busy} onClick=${onRepairStore}>
                ${busy ? 'Moving file aside…' : 'Move file aside and start fresh'}
              </button>
            </div>
            <div class="notice spaced-lg">
              <strong>What happens next</strong>
              <span>
                The unreadable file is moved to a timestamped backup in the same folder and never
                deleted. Any machines already paired with this device will still list it until you
                remove it there with <code>legion device.remove</code>.
              </span>
            </div>
          </div>
        </main>
      </div>
    `;
  }

  if (archivedInfo && archivedInfo.nextState?.ready) {
    return html`
      <div class="app standalone">
        <header class="top">
          <h1>Saved state</h1>
        </header>
        <main class="main">
          <div class="start-column">
            <h2 class="start-heading">Damaged state file moved aside.</h2>
            <div class="notice">
              <strong>Saved backup</strong>
              <span>Moved to <code>${archivedInfo.archivedTo}</code>.</span>
            </div>
            <p class="lede spaced">
              Any machines already paired with this device still list it until you remove it there
              with <code>legion device.remove</code>.
            </p>
            <div class="actions">
              <button
                type="button"
                class="btn primary"
                onClick=${() => {
                  const next = archivedInfo.nextState;
                  setArchivedInfo(null);
                  onConnected(next);
                }}
              >
                Continue
              </button>
            </div>
          </div>
        </main>
      </div>
    `;
  }

  if (needsPairing) {
    const canScan = linkState?.can_scan !== false;
    const isDesktopFirstStart = !canScan && !onCancel && !linkState?.ready;
    return html`
      <div class="app standalone">
        <header class=${onCancel ? 'top sub' : 'top'}>
          ${onCancel
            ? html`
                <button type="button" class="btn icon back" aria-label="Back" onClick=${onCancel}>
                  <${Icon} name="back" />
                </button>
              `
            : null}
          <h1>Pair this device</h1>
        </header>
        <main class="main">
          <div class="start-column">
            ${archivedInfo?.archivedTo
              ? html`
                  <div class="notice">
                    <strong>Damaged state file moved aside</strong>
                    <span>
                      Saved at <code>${archivedInfo.archivedTo}</code>. Previously paired machines
                      still list this device until removed there with <code>legion device.remove</code>.
                    </span>
                  </div>
                `
              : null}
            <h2 class="start-heading">Connect to one of your machines.</h2>
            ${isDesktopFirstStart
              ? html`
                  <p class="lede">
                    No local <code>legion</code> installation was found on this computer — see
                    <code>docs/legion/getting-started.md</code> to install it here, or pair with
                    another machine below.
                  </p>
                `
              : null}
            <p class="lede">One is enough: through it you reach the others. On that machine, run</p>
            <div class="code">
              <code>legion pair</code>
              <${CopyButton} text="legion pair" label="Copy command" iconOnly=${true} />
            </div>
            <p class="lede spaced">
              ${canScan
                ? 'It shows a code. Scan it here, or paste the line under it.'
                : 'It prints an invite line. Paste it below.'}
            </p>
            <label class="field">
              <span>Name this device</span>
              <input
                type="text"
                value=${deviceName}
                onInput=${(e) => ctrl.setDeviceName(e.target.value)}
                placeholder=${canScan ? 'phone' : 'computer'}
                disabled=${controlsDisabled}
              />
            </label>
            ${pairing
              ? html`
                  <div class="notice" role="status" aria-live="polite">
                    <strong>
                      ${cancelling
                        ? pairingNode
                          ? `Checking ${pairingNode}…`
                          : 'Checking…'
                        : pairingNode
                          ? `Pairing with ${pairingNode}…`
                          : 'Pairing…'}
                    </strong>
                    <span>
                      ${cancelling
                        ? `Checking whether ${pairingNode || 'the machine'} finished pairing before stopping.`
                        : `Waiting for ${pairingNode || 'the machine'} to confirm. This can take up to 45 seconds on a slow connection.`}
                    </span>
                  </div>
                `
              : null}
            ${pairNotice
              ? html`
                  <div
                    class=${pairNotice.kind === 'neutral' ? 'notice neutral' : 'notice'}
                    role="status"
                  >
                    <strong>${pairNotice.title}</strong>
                    ${pairNotice.restorable && invite && typeof ctrl.restorePair === 'function'
                      ? html`
                          <div class="actions">
                            <button
                              type="button"
                              class="btn small primary"
                              disabled=${controlsDisabled}
                              onClick=${() => ctrl.restorePair()}
                            >
                              Restore pairing
                            </button>
                          </div>
                        `
                      : pairNotice.retryable && invite
                        ? html`
                            <div class="actions">
                              <button
                                type="button"
                                class="btn small primary"
                                disabled=${controlsDisabled}
                                onClick=${() => ctrl.retryPair()}
                              >
                                Try again
                              </button>
                            </div>
                          `
                        : null}
                  </div>
                `
              : null}
            ${error ? html`<${Notice} error=${error} />` : null}
            ${error && pState.source === 'scan' && invite && !pairing
              ? html`
                  <div class="actions">
                    <button
                      type="button"
                      class="btn primary"
                      disabled=${controlsDisabled}
                      onClick=${() => ctrl.retryPair()}
                    >
                      Try again
                    </button>
                  </div>
                `
              : null}
            <div class="actions">
              ${canScan
                ? html`
                    <button type="button" class="btn primary" disabled=${controlsDisabled} onClick=${onScan}>
                      ${pairing && !pasteOpen ? 'Pairing…' : 'Scan the code'}
                    </button>
                  `
                : null}
              <button
                type="button"
                class=${canScan ? 'btn' : 'btn primary'}
                disabled=${controlsDisabled}
                onClick=${() => setPasteOpen((v) => !v)}
              >
                Paste invite
              </button>
              ${pairing && !pasteOpen
                ? html`
                    <button type="button" class="btn" disabled=${cancelling} onClick=${cancelPairing}>
                      ${cancelling ? 'Stopping…' : 'Cancel'}
                    </button>
                  `
                : null}
            </div>
            ${pasteOpen
              ? html`
                  <form onSubmit=${submitInvite}>
                    <label class="field">
                      <span>Invite</span>
                      <textarea
                        ref=${inviteInputRef}
                        value=${invite}
                        onInput=${(e) => ctrl.setInvite(e.target.value)}
                        placeholder="Paste the invite line from legion pair"
                        disabled=${controlsDisabled}
                        required
                      ></textarea>
                    </label>
                    <div class="actions">
                      <button type="submit" class="btn primary" disabled=${controlsDisabled}>
                        ${pairing ? 'Pairing…' : 'Pair device'}
                      </button>
                      ${pairing
                        ? html`
                            <button type="button" class="btn" disabled=${cancelling} onClick=${cancelPairing}>
                              ${cancelling ? 'Stopping…' : 'Cancel'}
                            </button>
                          `
                        : null}
                    </div>
                  </form>
                `
              : null}
            <div class="notice spaced-lg">
              <strong>What pairing allows</strong>
              <span>
                This device gets its own key. It can only use Legion on that machine, as the role you
                name, and you can remove it there at any time.
              </span>
            </div>
          </div>
        </main>
      </div>
    `;
  }

  return html`
    <div class="app standalone">
      <header class="top">
        <h1>Connect</h1>
      </header>
      <main class="main">
        <div class="start-column">
          <h2 class="start-heading">Connect this browser to ${nodeName}.</h2>
          <p class="lede">On that machine, make a token for this browser and paste it here.</p>
          <div class="code">
            <code>legion token.add phone phone</code>
            <${CopyButton} text="legion token.add phone phone" label="Copy command" iconOnly=${true} />
          </div>
          <form onSubmit=${submitToken}>
            <div class="field">
              <label htmlFor="start-token" class="field-label">
                <span>Token</span>
              </label>
              <input
                id="start-token"
                type="password"
                autocomplete="off"
                spellcheck="false"
                value=${token}
                onInput=${(e) => setToken(e.target.value)}
                placeholder="lgn_…"
                required
              />
              <small>Kept in this browser only. You can remove it anytime under Settings.</small>
            </div>
            ${error ? html`<${Notice} error=${error} />` : null}
            <div class="actions">
              <button type="submit" class="btn primary" disabled=${busy}>
                ${busy ? 'Connecting…' : 'Connect'}
              </button>
            </div>
          </form>
          <div class="notice spaced-lg">
            <strong>What a token allows</strong>
            <span>
              Everything this browser does runs as the role you gave the token, and that machine
              keeps a record. Remove the token there and this browser is locked out at once.
            </span>
          </div>
        </div>
      </main>
    </div>
  `;
}
