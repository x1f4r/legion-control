// First-start screen: bearer token entry for HTTP, or device pairing for the native app.

import { html, useState } from '../vendor/preact-htm.js';
import { CopyButton, Notice } from './common.js';

export function StartView({ link, linkState, onConnected }) {
  const [token, setToken] = useState('');
  const [invite, setInvite] = useState('');
  const [deviceName, setDeviceName] = useState('');
  const [pasteOpen, setPasteOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(linkState?.error || null);

  const nodeName = linkState?.health?.node || 'this machine';
  const needsPairing = linkState?.needs === 'pairing' || link.kind === 'app';

  async function submitToken(e) {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      const next = await link.setToken(token);
      setToken('');
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
        const next = await link.pair(scanned, deviceName || undefined);
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
      onConnected(next);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  if (needsPairing) {
    return html`
      <div class="app">
        <header class="bar">
          <h1 class="wordmark"><b>▍</b>Legion</h1>
        </header>
        <main class="main">
          <h2 class="start-heading">Pair this device with one of your machines.</h2>
          <p class="lede">One is enough: through it you reach the others. On that machine, run</p>
          <div class="code">
            <code>legion pair --as phone</code>
            <${CopyButton} text="legion pair --as phone" label="Copy command" iconOnly=${true} />
          </div>
          <p class="lede spaced">It shows a code. Scan it here, or paste the line under it.</p>
          ${error ? html`<${Notice} error=${error} />` : null}
          <div class="actions">
            <button type="button" class="btn primary" disabled=${busy} onClick=${onScan}>
              Scan the code
            </button>
            <button
              type="button"
              class="btn"
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
                      value=${invite}
                      onInput=${(e) => setInvite(e.target.value)}
                      placeholder="Paste the invite line from legion pair"
                      required
                    ></textarea>
                  </label>
                  <label class="field">
                    <span>Device name <em>optional</em></span>
                    <input
                      type="text"
                      value=${deviceName}
                      onInput=${(e) => setDeviceName(e.target.value)}
                      placeholder="phone"
                    />
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
        </main>
      </div>
    `;
  }

  return html`
    <div class="app">
      <header class="bar">
        <h1 class="wordmark"><b>▍</b>Legion</h1>
      </header>
      <main class="main">
        <h2 class="start-heading">Connect this browser to ${nodeName}.</h2>
        <p class="lede">
          Make a bearer token on that machine for a principal in policy.toml, then paste it below:
        </p>
        <div class="code">
          <code>legion token.add phone phone</code>
          <${CopyButton} text="legion token.add phone phone" label="Copy command" iconOnly=${true} />
        </div>
        <form onSubmit=${submitToken}>
          <label class="field">
            <span>Bearer token</span>
            <input
              type="password"
              autocomplete="off"
              spellcheck="false"
              value=${token}
              onInput=${(e) => setToken(e.target.value)}
              placeholder="lgn_…"
              required
            />
            <small>Kept in this browser only. You can remove it anytime under This device.</small>
          </label>
          ${error ? html`<${Notice} error=${error} />` : null}
          <div class="actions">
            <button type="submit" class="btn primary" disabled=${busy}>
              ${busy ? 'Connecting…' : 'Connect'}
            </button>
          </div>
        </form>
        <div class="notice spaced-lg">
          <strong>What this token allows</strong>
          <span>
            Every call runs as the token's principal and obeys that machine's policy.toml. Revoking
            the token on the node disconnects this browser on the next request.
          </span>
        </div>
      </main>
    </div>
  `;
}
