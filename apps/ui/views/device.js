// This device screen: identity, role summary, link management, theme, and versions.

import { html, useState } from '../vendor/preact-htm.js';
import { describeRoleAccess } from '../lib/schema.js';
import { Notice } from './common.js';

export const UI_VERSION = '0.1.0';

export function DeviceView({
  link,
  linkState,
  nodeInfo,
  tools,
  theme,
  onChangeTheme,
  onRequestConfirm,
  onDisconnected,
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const whoami = nodeInfo?.whoami;
  const allowLines = describeRoleAccess(whoami, tools);

  function askForget() {
    onRequestConfirm({
      title: 'Forget this browser?',
      summary: 'This browser will need a token again.',
      confirmLabel: 'Forget this browser',
      danger: false,
      async onConfirm() {
        setBusy(true);
        setError(null);
        try {
          await link.forget();
          await onDisconnected();
        } catch (err) {
          setError(err);
        } finally {
          setBusy(false);
        }
      },
    });
  }

  async function onUnpair(node) {
    setBusy(true);
    setError(null);
    try {
      await link.unpair(node);
      await onDisconnected();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  return html`
    <div>
      ${error ? html`<${Notice} error=${error} />` : null}

      <p class="label">Identity</p>
      <div class="list">
        <dl class="kv">
          <dt>principal</dt>
          <dd>${whoami?.principal || nodeInfo?.principal || 'unknown'}</dd>
          <dt>role</dt>
          <dd>${whoami?.role || nodeInfo?.role || 'unknown'}</dd>
          <dt>node</dt>
          <dd>${whoami?.node || nodeInfo?.node || 'unknown'}</dd>
          <dt>door</dt>
          <dd>${whoami?.door || link.kind}</dd>
          <dt>allows</dt>
          <dd class="allows-lines">
            ${allowLines.map((line, i) => html`<span key=${i}>${line}</span>`)}
          </dd>
        </dl>
      </div>

      <p class="label">Link</p>
      ${link.kind === 'app'
        ? (linkState?.nodes || []).length > 0
          ? html`
              <div class="list">
                ${linkState.nodes.map(
                  (n) => html`
                    <div class="row" key=${n.node}>
                      <div class="body">
                        <div class="title mono">${n.node}</div>
                        <div class="sub">${n.principal} · ${n.role} · ${n.via}</div>
                      </div>
                      <button
                        type="button"
                        class="btn small"
                        disabled=${busy}
                        onClick=${() => onUnpair(n.node)}
                      >
                        Remove
                      </button>
                    </div>
                  `,
                )}
              </div>
            `
          : html`<div class="empty">No paired machines.</div>`
        : html`
            <div class="list">
              <dl class="kv">
                <dt>origin</dt>
                <dd>${nodeInfo?.origin || window.location.origin}</dd>
                <dt>mode</dt>
                <dd>${link.kind}</dd>
              </dl>
            </div>
            <div class="spaced">
              <button type="button" class="btn" disabled=${busy} onClick=${askForget}>
                Forget this browser
              </button>
            </div>
          `}

      <p class="label">Appearance</p>
      <div class="chips" role="group" aria-label="Theme">
        ${[
          ['system', 'Follow system'],
          ['light', 'Light'],
          ['dark', 'Dark'],
        ].map(
          ([value, label]) => html`
            <button
              type="button"
              key=${value}
              class="chip-btn"
              aria-pressed=${theme === value ? 'true' : 'false'}
              onClick=${() => onChangeTheme(value)}
            >
              ${label}
            </button>
          `,
        )}
      </div>

      <p class="label">About</p>
      <div class="list">
        <dl class="kv">
          <dt>interface</dt>
          <dd>${UI_VERSION}</dd>
          <dt>node</dt>
          <dd>
            ${nodeInfo?.version || 'unknown'}
            ${nodeInfo?.build ? ` (${nodeInfo.build})` : ''}
          </dd>
          <dt>protocol</dt>
          <dd>${nodeInfo?.protocol ?? 1}</dd>
        </dl>
      </div>
    </div>
  `;
}
