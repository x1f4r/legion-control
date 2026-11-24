// This device screen: identity, role summary, link management, theme, and versions.

import { html, useState } from '../vendor/preact-htm.js';
import { Notice } from './common.js';

export const UI_VERSION = '0.1.0';

function roleSummary(whoami, tools) {
  if (!whoami) return 'Permissions are decided by the node policy.';
  const allowed = new Set(whoami.tools || []);
  const effects = new Set();
  for (const t of tools || []) {
    if (allowed.has(t.name)) effects.add(t.effect);
  }
  const ordered = ['read', 'write', 'exec', 'destroy', 'power', 'admin'].filter((e) =>
    effects.has(e),
  );
  const parts = [];
  if (ordered.length > 0) {
    parts.push(`May run ${allowed.size} tools (${ordered.join(', ')}).`);
  } else {
    parts.push('May not run any tools.');
  }
  if (Array.isArray(whoami.paths) && whoami.paths.length > 0) {
    parts.push(`File tools are confined to ${whoami.paths.join(', ')}.`);
  } else if (whoami.paths === null) {
    parts.push('File tools have no extra path limit.');
  }
  if (whoami.delegate) {
    parts.push('May act as another principal.');
  }
  return parts.join(' ');
}

export function DeviceView({
  link,
  linkState,
  nodeInfo,
  tools,
  theme,
  onChangeTheme,
  onDisconnected,
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const whoami = nodeInfo?.whoami;

  async function onForget() {
    setBusy(true);
    setError(null);
    try {
      await link.forget();
      onDisconnected();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  async function onUnpair(node) {
    setBusy(true);
    setError(null);
    try {
      await link.unpair(node);
      onDisconnected();
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
          <dd>${roleSummary(whoami, tools)}</dd>
        </dl>
      </div>

      <p class="label">Link</p>
      ${link.kind === 'app'
        ? html`
            <div class="list">
              ${(linkState?.nodes || []).map(
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
        : html`
            <div class="list">
              <dl class="kv">
                <dt>origin</dt>
                <dd>${nodeInfo?.origin || window.location.origin}</dd>
                <dt>mode</dt>
                <dd>${link.kind}</dd>
              </dl>
            </div>
            <div class="actions">
              <button type="button" class="btn danger" disabled=${busy} onClick=${onForget}>
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
