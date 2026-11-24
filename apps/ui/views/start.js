// First-start screen: bearer token entry for HTTP, or device pairing for the native app.

import { html, useEffect, useRef, useState } from '../vendor/preact-htm.js';
import { Icon } from '../icons.js';
import { CopyButton, Notice } from './common.js';

export function StartView({ link, linkState, onConnected, onCancel }) {
  const [token, setToken] = useState('');
  const [invite, setInvite] = useState('');
  const [deviceName, setDeviceName] = useState('');
  const [pasteOpen, setPasteOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(linkState?.error || null);
  const [archivedInfo, setArchivedInfo] = useState(null);
  const inviteInputRef = useRef(null);

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
        if (alive && name) {
          setDeviceName((prev) => (prev ? prev : name));
        }
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [link, needsPairing]);

  async function submitToken(e) {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      const next = await link.setToken(token);
      setToken('');
      window.location.hash = '#/machines';
      onConnected(next);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  async function onScan() {
    setError(null);
    setBusy(true);
    try {
      const scanned = await link.scanInvite();
      if (scanned) {
        const next = await link.pair(scanned, deviceName.trim() || undefined);
        window.location.hash = '#/machines';
        onConnected(next);
      }
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  async function submitInvite(e) {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      const next = await link.pair(invite.trim(), deviceName.trim() || undefined);
      setInvite('');
      window.location.hash = '#/machines';
      onConnected(next);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  async function onRepairStore() {
    setError(null);
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
      setError(err);
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
                onInput=${(e) => setDeviceName(e.target.value)}
                placeholder=${canScan ? 'phone' : 'computer'}
              />
            </label>
            ${error ? html`<${Notice} error=${error} />` : null}
            <div class="actions">
              ${canScan
                ? html`
                    <button type="button" class="btn primary" disabled=${busy} onClick=${onScan}>
                      Scan the code
                    </button>
                  `
                : null}
              <button
                type="button"
                class=${canScan ? 'btn' : 'btn primary'}
                disabled=${busy}
                onClick=${() => setPasteOpen((v) => !v)}
              >
                Paste invite
              </button>
            </div>
            ${pasteOpen
              ? html`
                  <form onSubmit=${submitInvite}>
                    <label class="field">
                      <span>Invite</span>
                      <textarea
                        ref=${inviteInputRef}
                        value=${invite}
                        onInput=${(e) => setInvite(e.target.value)}
                        placeholder="Paste the invite line from legion pair"
                        required
                      ></textarea>
                    </label>
                    <div class="actions">
                      <button type="submit" class="btn primary" disabled=${busy}>
                        ${busy ? 'Pairing…' : 'Pair device'}
                      </button>
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
