// Settings pages: This device, Connections (paired + invite QR + fleet.add + incoming devices),
// Appearance, Technical details, Refreshing, and About.

import { html, useEffect, useMemo, useRef, useState } from '../vendor/preact-htm.js';
import { CopyButton, Notice, QrCode, SwipeRow } from './common.js';
import { ToolDetail } from './tools.js';
import {
  describePlainPermissions,
  describeRolePlain,
  formatInviteValidity,
} from '../lib/activity.js';
import { formatRelativeTime } from '../lib/format.js';
import { routeHref } from '../lib/route.js';
import { describeRoleAccess } from '../lib/schema.js';
import {
  ACCENT_SWATCHES,
  describeAppearance,
  describeRefreshing,
} from '../lib/settings.js';

export const UI_VERSION = '0.1.0';

function capitalize(word) {
  const s = String(word || '');
  return s ? s.charAt(0).toUpperCase() + s.slice(1) : '';
}

// ── This device page ─────────────────────────────────────────────────────────────────────────

function ThisDevicePage({
  deviceNoun,
  link,
  nodeInfo,
  tools,
  technical,
  onRequestConfirm,
  onDisconnected,
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const whoami = nodeInfo?.whoami;
  const plainPerms = describePlainPermissions(whoami, tools);
  const roleLines = describeRoleAccess(whoami, tools);

  function askForget() {
    onRequestConfirm({
      title: `Forget this ${deviceNoun}?`,
      summary: `This ${deviceNoun} will need a token again to connect.`,
      confirmLabel: `Forget this ${deviceNoun}`,
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

  return html`
    <div>
      ${error ? html`<${Notice} error=${error} />` : null}

      <p class="label">Identity</p>
      <div class="list">
        <dl class=${technical ? 'kv' : 'kv plain'}>
          <dt>${technical ? 'principal' : 'Name'}</dt>
          <dd>${whoami?.principal || nodeInfo?.principal || 'Unknown'}</dd>
          <dt>${technical ? 'node' : 'Connected to'}</dt>
          <dd>${whoami?.node || nodeInfo?.node || 'Unknown'}</dd>
          <dt>${technical ? 'access' : 'What it can do'}</dt>
          <dd>${plainPerms}</dd>
          ${technical
            ? html`
                <dt>role</dt>
                <dd>${whoami?.role || nodeInfo?.role || 'unknown'}</dd>
                <dt>door</dt>
                <dd>${whoami?.door || link.kind}</dd>
                <dt>allows</dt>
                <dd class="allows-lines">
                  ${roleLines.map((line, i) => html`<span key=${i}>${line}</span>`)}
                </dd>
              `
            : null}
        </dl>
      </div>

      ${link.kind !== 'app'
        ? html`
            <div class="list spaced">
              <dl class=${technical ? 'kv' : 'kv plain'}>
                <dt>${technical ? 'origin' : 'Address'}</dt>
                <dd>${nodeInfo?.origin || window.location.origin}</dd>
              </dl>
            </div>
            <div class="actions">
              <button type="button" class="btn" disabled=${busy} onClick=${askForget}>
                Forget this ${deviceNoun}
              </button>
            </div>
          `
        : html`
            <div class="list spaced">
              <a class="row" href=${routeHref({ section: 'settings', page: 'connections' })}>
                <span class="body">
                  <span class="title">Manage connections</span>
                  <span class="sub">Switch machines, pair another, or let devices in</span>
                </span>
                <span class="chev" aria-hidden="true">›</span>
              </a>
            </div>
          `}
    </div>
  `;
}

// ── Connections subpages ─────────────────────────────────────────────────────────────────────

function PairMachineSubpage({ link, linkState, onConnected }) {
  const [invite, setInvite] = useState('');
  const [deviceName, setDeviceName] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const canScan = linkState?.can_scan !== false;

  useEffect(() => {
    if (typeof link.deviceName !== 'function') return;
    let alive = true;
    link
      .deviceName()
      .then((name) => {
        if (alive && name) setDeviceName((prev) => prev || name);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [link]);

  async function onScan() {
    setError(null);
    setBusy(true);
    try {
      const scanned = await link.scanInvite();
      if (scanned) {
        const next = await link.pair(scanned, deviceName.trim() || undefined);
        window.location.hash = '#/settings/connections';
        onConnected(next);
      }
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  async function onSubmit(e) {
    e.preventDefault();
    if (!invite.trim()) return;
    setError(null);
    setBusy(true);
    try {
      const next = await link.pair(invite.trim(), deviceName.trim() || undefined);
      setInvite('');
      window.location.hash = '#/settings/connections';
      onConnected(next);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  return html`
    <div>
      <p class="lede">
        On the other machine, create an invite (or run <code>legion pair --as phone</code>), then
        scan its code or paste the invite below.
      </p>
      <label class="field">
        <span>Name for this device</span>
        <input
          type="text"
          value=${deviceName}
          onInput=${(e) => setDeviceName(e.target.value)}
          placeholder="phone"
        />
      </label>
      ${error ? html`<${Notice} error=${error} />` : null}
      ${canScan
        ? html`
            <div class="actions">
              <button type="button" class="btn primary full" disabled=${busy} onClick=${onScan}>
                Scan QR code
              </button>
            </div>
          `
        : null}
      <form onSubmit=${onSubmit}>
        <label class="field">
          <span>Invite</span>
          <textarea
            value=${invite}
            onInput=${(e) => setInvite(e.target.value)}
            placeholder="Paste the invite line"
            required
          ></textarea>
        </label>
        <div class="actions">
          <button type="submit" class=${canScan ? 'btn full' : 'btn primary full'} disabled=${busy}>
            ${busy ? 'Pairing…' : 'Pair machine'}
          </button>
        </div>
      </form>
    </div>
  `;
}

function InviteDeviceSubpage({ link, tools, nodeInfo, technical }) {
  const inviteSpec = useMemo(() => (tools || []).find((t) => t.name === 'device.invite'), [tools]);
  const roleProp = inviteSpec?.input_schema?.properties?.role || null;
  const roleChoices = Array.isArray(roleProp?.enum) ? roleProp.enum : null;
  const defaultIdentity = nodeInfo?.whoami?.principal || 'phone';

  const [principal, setPrincipal] = useState(defaultIdentity);
  const [validMinutes, setValidMinutes] = useState(15);
  const [role, setRole] = useState(roleProp?.default || (roleChoices ? roleChoices[0] : ''));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const [inviteRes, setInviteRes] = useState(null);

  async function onCreate(e) {
    e.preventDefault();
    const who = principal.trim();
    if (!who) return;
    setBusy(true);
    setError(null);
    try {
      const args = { principal: who, valid_minutes: validMinutes };
      if (roleProp && role) args.role = role;
      const res = await link.call('device.invite', args);
      setInviteRes({ ...res, validMinutes });
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  return html`
    <div>
      <p class="lede">
        Create a one-time invite on ${nodeInfo?.node || 'this machine'} so a phone, tablet or
        another computer can pair with it.
      </p>

      <form onSubmit=${onCreate}>
        <label class="field">
          <span>${technical ? 'Principal' : 'Use the identity'}</span>
          <input
            type="text"
            value=${principal}
            onInput=${(e) => setPrincipal(e.target.value)}
            placeholder=${defaultIdentity}
            required
          />
          <small>
            ${technical
              ? 'Matches a principal in policy.toml (for example phone or tablet).'
              : 'A name this machine already knows, such as the one this device uses.'}
          </small>
        </label>

        ${roleProp
          ? html`
              <label class="field">
                <span>Role</span>
                ${roleChoices
                  ? html`
                      <select value=${role} onChange=${(e) => setRole(e.target.value)}>
                        ${roleChoices.map(
                          (opt) => html`<option key=${opt} value=${opt}>${opt}</option>`,
                        )}
                      </select>
                    `
                  : html`
                      <input
                        type="text"
                        value=${role}
                        onInput=${(e) => setRole(e.target.value)}
                        placeholder="operator"
                      />
                    `}
              </label>
            `
          : null}

        <p class="label">Valid for</p>
        <div class="seg" role="group" aria-label="Invite validity">
          ${[
            [15, '15 min'],
            [60, '1 hour'],
            [1440, '1 day'],
          ].map(
            ([mins, label]) => html`
              <button
                type="button"
                key=${mins}
                aria-pressed=${validMinutes === mins ? 'true' : 'false'}
                onClick=${() => setValidMinutes(mins)}
              >
                ${label}
              </button>
            `,
          )}
        </div>

        ${error ? html`<${Notice} error=${error} />` : null}

        <div class="actions">
          <button type="submit" class="btn primary full" disabled=${busy || !principal.trim()}>
            ${busy ? 'Making invite…' : inviteRes ? 'Make a new invite' : 'Make invite'}
          </button>
        </div>
      </form>

      ${inviteRes?.invite
        ? html`
            <div class="spaced-lg">
              <p class="label">Scan on the other device</p>
              <${QrCode} text=${inviteRes.invite} label="Pairing invite QR code" />
              <p class="label">Or copy the invite line</p>
              <div class="code">
                <code>${inviteRes.invite}</code>
                <${CopyButton} text=${inviteRes.invite} label="Copy invite" iconOnly=${true} />
              </div>
              <p class="lede spaced">
                ${technical
                  ? html`
                      Works once, for the next ${formatInviteValidity(inviteRes.validMinutes)}, as
                      <code>${inviteRes.principal}</code>
                      ${inviteRes.role ? ` (${inviteRes.role})` : ''}.
                    `
                  : `Works once, for the next ${formatInviteValidity(inviteRes.validMinutes)}. The device will be able to: ${describeRolePlain(inviteRes.role)}.`}
              </p>
            </div>
          `
        : null}
    </div>
  `;
}

function PairedNodeSubpage({
  node,
  isActive,
  tools,
  technical,
  busy,
  onSelectNode,
  onForgetNode,
}) {
  if (!node) return html`<div class="empty">Connection not found.</div>`;
  const plainPerms = describePlainPermissions(node.whoami, tools);

  return html`
    <div>
      <div class="list">
        <dl class=${technical ? 'kv' : 'kv plain'}>
          <dt>${technical ? 'node' : 'Machine'}</dt>
          <dd>${node.node}</dd>
          <dt>${technical ? 'principal' : 'As'}</dt>
          <dd>${node.principal || 'Unknown'}</dd>
          <dt>${technical ? 'access' : 'What it can do'}</dt>
          <dd>${plainPerms}</dd>
          ${technical
            ? html`
                <dt>role</dt>
                <dd>${node.role || 'unknown'}</dd>
                <dt>via</dt>
                <dd>${node.via || 'unknown'}</dd>
                ${node.path
                  ? html`
                      <dt>path</dt>
                      <dd>${node.path}</dd>
                    `
                  : null}
              `
            : null}
        </dl>
      </div>

      <div class="actions">
        ${!isActive && onSelectNode
          ? html`
              <button
                type="button"
                class="btn primary full"
                disabled=${busy}
                onClick=${() => onSelectNode(node.node)}
              >
                Use this machine first
              </button>
            `
          : null}
        ${node.via !== 'local' && onForgetNode
          ? html`
              <button
                type="button"
                class="btn danger full"
                disabled=${busy}
                onClick=${() => onForgetNode(node.node)}
              >
                Forget connection
              </button>
            `
          : null}
      </div>
    </div>
  `;
}

function IncomingDeviceSubpage({
  device,
  tools,
  allowedSet,
  technical,
  link,
  onRequestConfirm,
  onRemoved,
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const canSet = Boolean((tools || []).find((t) => t.name === 'device.set')) &&
    (!allowedSet || allowedSet.has('device.set'));
  const canRemove = Boolean((tools || []).find((t) => t.name === 'device.remove')) &&
    (!allowedSet || allowedSet.has('device.remove'));

  if (!device) return html`<div class="empty">Device not found.</div>`;

  function askRemove() {
    onRequestConfirm({
      title: `Remove ${device.name}?`,
      summary: `${device.name} will lose access to this machine right away.`,
      confirmLabel: 'Remove access',
      danger: true,
      async onConfirm() {
        setBusy(true);
        setError(null);
        try {
          await link.call('device.remove', { name: device.name });
          onRemoved();
        } catch (err) {
          setError(err);
        } finally {
          setBusy(false);
        }
      },
    });
  }

  return html`
    <div>
      ${error ? html`<${Notice} error=${error} />` : null}
      <div class="list">
        <dl class=${technical ? 'kv' : 'kv plain'}>
          <dt>${technical ? 'name' : 'Device'}</dt>
          <dd>${device.name}</dd>
          <dt>${technical ? 'principal' : 'Identity'}</dt>
          <dd>${device.principal || 'Unknown'}</dd>
          ${device.added
            ? html`
                <dt>${technical ? 'added' : 'Paired'}</dt>
                <dd>${formatRelativeTime(device.added)}</dd>
              `
            : null}
          ${technical && device.fingerprint
            ? html`
                <dt>fingerprint</dt>
                <dd>${device.fingerprint}</dd>
              `
            : null}
        </dl>
      </div>

      ${canSet
        ? html`
            <div class="spaced">
              <${ToolDetail}
                tool=${(tools || []).find((t) => t.name === 'device.set')}
                allowed=${true}
                hideTargetPicker=${true}
                link=${link}
                technical=${technical}
                onRequestConfirm=${onRequestConfirm}
              />
            </div>
          `
        : null}

      ${canRemove
        ? html`
            <div class="actions">
              <button
                type="button"
                class="btn danger full"
                disabled=${busy}
                onClick=${askRemove}
              >
                Remove access
              </button>
            </div>
          `
        : null}
    </div>
  `;
}

function ConnectionsPage({
  route,
  link,
  linkState,
  nodeInfo,
  tools,
  allowedSet,
  snapshot,
  technical,
  onRequestConfirm,
  onSelectNode,
  onDisconnected,
  onConnected,
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const [notice, setNotice] = useState(null);
  const [incoming, setIncoming] = useState(null);

  const canInvite =
    Boolean((tools || []).find((t) => t.name === 'device.invite')) &&
    (!allowedSet || allowedSet.has('device.invite'));
  const canFleetAdd =
    Boolean((tools || []).find((t) => t.name === 'fleet.add')) &&
    (!allowedSet || allowedSet.has('fleet.add'));
  const canListDevices =
    Boolean((tools || []).find((t) => t.name === 'device.list')) &&
    (!allowedSet || allowedSet.has('device.list'));

  async function loadIncoming() {
    if (!canListDevices) return;
    try {
      const res = await link.call('device.list', {});
      setIncoming(res);
    } catch {
      setIncoming(null);
    }
  }

  useEffect(() => {
    loadIncoming();
  }, [canListDevices, nodeInfo?.node, route.sub]);

  async function forgetNode(nodeName) {
    onRequestConfirm({
      title: `Forget ${nodeName}?`,
      summary: `This device will disconnect from ${nodeName} until you pair it again.`,
      confirmLabel: 'Forget connection',
      danger: true,
      async onConfirm() {
        setBusy(true);
        setError(null);
        setNotice(null);
        try {
          if (link.kind === 'app' && typeof link.unpair === 'function') {
            const res = await link.unpair(nodeName);
            if (res?.message) setNotice(res.message);
          } else if (typeof link.forget === 'function') {
            await link.forget();
          }
          window.location.hash = '#/settings/connections';
          await onDisconnected();
        } catch (err) {
          setError(err);
        } finally {
          setBusy(false);
        }
      },
    });
  }

  async function handleSelectNode(nodeName) {
    if (!onSelectNode) return;
    setBusy(true);
    setError(null);
    try {
      await onSelectNode(nodeName);
      window.location.hash = '#/settings/connections';
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  if (route.sub === 'pair' && link.kind === 'app') {
    return html`
      <${PairMachineSubpage}
        link=${link}
        linkState=${linkState}
        onConnected=${onConnected}
      />
    `;
  }

  if (route.sub === 'invite' && canInvite) {
    return html`
      <${InviteDeviceSubpage}
        link=${link}
        tools=${tools}
        nodeInfo=${nodeInfo}
        technical=${technical}
      />
    `;
  }

  if (route.sub === 'add' && canFleetAdd) {
    const spec = (tools || []).find((t) => t.name === 'fleet.add');
    return html`
      <${ToolDetail}
        tool=${spec}
        allowed=${true}
        hideTargetPicker=${true}
        nodeInfo=${nodeInfo}
        machines=${snapshot?.machines || []}
        runningJobs=${snapshot?.runningJobs || []}
        link=${link}
        technical=${technical}
        onRequestConfirm=${onRequestConfirm}
      />
    `;
  }

  if (route.sub === 'node' && route.item) {
    const nodes = linkState?.nodes || [];
    const idx = nodes.findIndex((n) => n.node === route.item);
    return html`
      <${PairedNodeSubpage}
        node=${idx >= 0 ? nodes[idx] : null}
        isActive=${idx === 0}
        tools=${tools}
        technical=${technical}
        busy=${busy}
        onSelectNode=${onSelectNode ? handleSelectNode : null}
        onForgetNode=${forgetNode}
      />
    `;
  }

  if (route.sub === 'incoming' && route.item) {
    const dev = (incoming?.devices || []).find((d) => d.name === route.item) || null;
    return html`
      <${IncomingDeviceSubpage}
        device=${dev}
        tools=${tools}
        allowedSet=${allowedSet}
        technical=${technical}
        link=${link}
        onRequestConfirm=${onRequestConfirm}
        onRemoved=${() => {
          loadIncoming();
          window.location.hash = '#/settings/connections';
        }}
      />
    `;
  }

  const nodes = linkState?.nodes || [];
  const hasMakeSection = link.kind === 'app' || canInvite || canFleetAdd;
  const incomingDevices = incoming?.devices || [];

  return html`
    <div>
      ${error ? html`<${Notice} error=${error} />` : null}
      ${notice ? html`<${Notice} title=${notice} kind="warn" />` : null}

      <p class="label">Paired machines</p>
      ${nodes.length > 0
        ? html`
            <div class="list">
              ${nodes.map((n, idx) => {
                const sub = technical
                  ? `${n.principal} · ${n.role} · ${n.via}`
                  : `${idx === 0 ? 'Using now · ' : ''}${describePlainPermissions(n.whoami, tools)}`;
                const row = html`
                  <a
                    class="row"
                    href=${routeHref({
                      section: 'settings',
                      page: 'connections',
                      sub: 'node',
                      item: n.node,
                    })}
                  >
                    <span class=${idx === 0 ? 'dot' : 'dot off'} aria-hidden="true"></span>
                    <span class="body">
                      <span class="title mono">${n.node}</span>
                      <span class="sub">${sub}</span>
                    </span>
                    <span class="chev" aria-hidden="true">›</span>
                  </a>
                `;
                if (n.via !== 'local') {
                  return html`
                    <${SwipeRow}
                      key=${`${n.node}:${n.via}`}
                      actionLabel="Forget"
                      disabled=${busy}
                      onAction=${() => forgetNode(n.node)}
                    >
                      ${row}
                    <//>
                  `;
                }
                return html`<div key=${`${n.node}:${n.via}`}>${row}</div>`;
              })}
            </div>
          `
        : html`<div class="empty">No paired machines.</div>`}

      ${hasMakeSection
        ? html`
            ${nodes.length === 1
              ? html`
                  <p class="lede spaced">
                    Pair with a second machine so this device still works when ${nodes[0].node} is off.
                  </p>
                `
              : null}
            <p class="label">Make a connection</p>
            <div class="list">
              ${link.kind === 'app'
                ? html`
                    <a
                      class="row"
                      href=${routeHref({
                        section: 'settings',
                        page: 'connections',
                        sub: 'pair',
                      })}
                    >
                      <span class="body">
                        <span class="title">Pair with another machine</span>
                        <span class="sub">Scan a QR code or paste an invite</span>
                      </span>
                      <span class="chev" aria-hidden="true">›</span>
                    </a>
                  `
                : null}
              ${canInvite
                ? html`
                    <a
                      class="row"
                      href=${routeHref({
                        section: 'settings',
                        page: 'connections',
                        sub: 'invite',
                      })}
                    >
                      <span class="body">
                        <span class="title">Let another device in</span>
                        <span class="sub">Show a QR code for a phone or tablet</span>
                      </span>
                      <span class="chev" aria-hidden="true">›</span>
                    </a>
                  `
                : null}
              ${canFleetAdd
                ? html`
                    <a
                      class="row"
                      href=${routeHref({
                        section: 'settings',
                        page: 'connections',
                        sub: 'add',
                      })}
                    >
                      <span class="body">
                        <span class="title">Add a machine to the fleet</span>
                        <span class="sub">Connect another computer to ${nodeInfo?.node || 'this node'}</span>
                      </span>
                      <span class="chev" aria-hidden="true">›</span>
                    </a>
                  `
                : null}
            </div>
          `
        : null}

      ${canListDevices
        ? html`
            <p class="label">Devices that may come in</p>
            ${incomingDevices.length > 0
              ? html`
                  <div class="list">
                    ${incomingDevices.map(
                      (dev) => html`
                        <a
                          key=${dev.name}
                          class="row"
                          href=${routeHref({
                            section: 'settings',
                            page: 'connections',
                            sub: 'incoming',
                            item: dev.name,
                          })}
                        >
                          <span class="body">
                            <span class="title">${dev.name}</span>
                            <span class="sub">
                              ${dev.principal}
                              ${dev.added ? ` · paired ${formatRelativeTime(dev.added)}` : ''}
                            </span>
                          </span>
                          <span class="chev" aria-hidden="true">›</span>
                        </a>
                      `,
                    )}
                  </div>
                `
              : html`<div class="empty">No other devices paired with ${nodeInfo?.node || 'this machine'}.</div>`}
          `
        : null}
    </div>
  `;
}

// ── Appearance page ──────────────────────────────────────────────────────────────────────────

function AppearancePage({ deviceNoun, settings, onUpdateSettings }) {
  return html`
    <div>
      <p class="label">Theme</p>
      <div class="seg" role="group" aria-label="Theme">
        ${[
          ['system', `Like the ${deviceNoun}`],
          ['dark', 'Dark'],
          ['light', 'Light'],
        ].map(
          ([value, label]) => html`
            <button
              type="button"
              key=${value}
              aria-pressed=${settings.theme === value ? 'true' : 'false'}
              onClick=${() => onUpdateSettings({ theme: value })}
            >
              ${label}
            </button>
          `,
        )}
      </div>

      <p class="label">Accent</p>
      <div class="list">
        <div class="swatches" role="group" aria-label="Accent colour">
          ${ACCENT_SWATCHES.map(
            (sw) => html`
              <button
                type="button"
                key=${sw.id}
                class=${`swatch ${sw.id}`}
                aria-label=${sw.label}
                aria-pressed=${settings.accent === sw.id ? 'true' : 'false'}
                onClick=${() => onUpdateSettings({ accent: sw.id })}
              ></button>
            `,
          )}
        </div>
      </div>

      <p class="label">Text size</p>
      <div class="seg" role="group" aria-label="Text size">
        ${[
          ['small', 'Small'],
          ['medium', 'Medium'],
          ['large', 'Large'],
        ].map(
          ([value, label]) => html`
            <button
              type="button"
              key=${value}
              aria-pressed=${settings.textSize === value ? 'true' : 'false'}
              onClick=${() => onUpdateSettings({ textSize: value })}
            >
              ${label}
            </button>
          `,
        )}
      </div>

      <p class="label">Spacing</p>
      <div class="seg" role="group" aria-label="Spacing">
        ${[
          ['roomy', 'Roomy'],
          ['compact', 'Compact'],
        ].map(
          ([value, label]) => html`
            <button
              type="button"
              key=${value}
              aria-pressed=${settings.density === value ? 'true' : 'false'}
              onClick=${() => onUpdateSettings({ density: value })}
            >
              ${label}
            </button>
          `,
        )}
      </div>

      <p class="label">Machines list</p>
      <div class="list">
        <label class="row switch row-switch">
          <span class="body">
            <span class="title">Processor, memory and disk</span>
            <span class="sub">Show the three bars on each machine</span>
          </span>
          <input
            type="checkbox"
            checked=${Boolean(settings.showMetrics)}
            onChange=${(e) => onUpdateSettings({ showMetrics: e.target.checked })}
          />
        </label>
        <label class="row switch row-switch">
          <span class="body">
            <span class="title">Show machines that are off</span>
            <span class="sub">List off machines below the ones that are on</span>
          </span>
          <input
            type="checkbox"
            checked=${Boolean(settings.showOffline)}
            onChange=${(e) => onUpdateSettings({ showOffline: e.target.checked })}
          />
        </label>
      </div>
    </div>
  `;
}

// ── Refreshing page ──────────────────────────────────────────────────────────────────────────

function RefreshingPage({ settings, onUpdateSettings }) {
  return html`
    <div>
      <p class="label">Refresh automatically</p>
      <div class="seg" role="group" aria-label="Refresh interval">
        ${[
          [15, '15 s'],
          [30, '30 s'],
          [60, '1 min'],
          [0, 'Manual'],
        ].map(
          ([sec, label]) => html`
            <button
              type="button"
              key=${sec}
              aria-pressed=${settings.refreshSeconds === sec ? 'true' : 'false'}
              onClick=${() => onUpdateSettings({ refreshSeconds: sec })}
            >
              ${label}
            </button>
          `,
        )}
      </div>
      <p class="lede spaced">
        ${settings.refreshSeconds === 0
          ? 'Machines only update when you pull down to refresh.'
          : 'Updates only run while this window is open and visible.'}
      </p>

      <p class="label">Off machines</p>
      <div class="list">
        <label class="row switch row-switch">
          <span class="body">
            <span class="title">Check off machines quietly</span>
            <span class="sub">Check at most every 2 minutes in the background</span>
          </span>
          <input
            type="checkbox"
            checked=${Boolean(settings.checkOffline)}
            onChange=${(e) => onUpdateSettings({ checkOffline: e.target.checked })}
          />
        </label>
      </div>
      <p class="lede spaced">
        Pulling down on the Machines list always checks every machine, including ones that are off.
      </p>
    </div>
  `;
}

// ── About page ───────────────────────────────────────────────────────────────────────────────

function AboutPage({ nodeInfo }) {
  return html`
    <div>
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

// ── SettingsView shell ───────────────────────────────────────────────────────────────────────

export function SettingsView({
  route,
  wide,
  deviceNoun,
  link,
  linkState,
  nodeInfo,
  tools,
  allowedSet,
  snapshot,
  settings,
  onUpdateSettings,
  onRequestConfirm,
  onSelectNode,
  onDisconnected,
  onConnected,
}) {
  const technical = Boolean(settings?.technical);
  const whoami = nodeInfo?.whoami;
  const plainPerms = describePlainPermissions(whoami, tools);
  const nodeCount = (linkState?.nodes || []).length;
  const selectedPage = wide ? route.page || 'device' : route.page || null;

  const indexPane = html`
    <section>
      <div class="list">
        <a
          class=${['row', wide && selectedPage === 'device' ? 'selected' : '']
            .filter(Boolean)
            .join(' ')}
          href=${routeHref({ section: 'settings', page: 'device' })}
          aria-current=${wide && selectedPage === 'device' ? 'page' : undefined}
        >
          <span class="body">
            <span class="title">This ${deviceNoun}</span>
            <span class="sub">
              ${whoami?.principal || nodeInfo?.principal || capitalize(deviceNoun)} · ${plainPerms}
            </span>
          </span>
          <span class="chev" aria-hidden="true">›</span>
        </a>
        <a
          class=${['row', wide && selectedPage === 'connections' ? 'selected' : '']
            .filter(Boolean)
            .join(' ')}
          href=${routeHref({ section: 'settings', page: 'connections' })}
          aria-current=${wide && selectedPage === 'connections' ? 'page' : undefined}
        >
          <span class="body">
            <span class="title">Connections</span>
            <span class="sub">
              ${`${nodeCount} ${nodeCount === 1 ? 'machine' : 'machines'}`}
            </span>
          </span>
          <span class="chev" aria-hidden="true">›</span>
        </a>
      </div>

      <div class="list spaced">
        <a
          class=${['row', wide && selectedPage === 'appearance' ? 'selected' : '']
            .filter(Boolean)
            .join(' ')}
          href=${routeHref({ section: 'settings', page: 'appearance' })}
          aria-current=${wide && selectedPage === 'appearance' ? 'page' : undefined}
        >
          <span class="body">
            <span class="title">Appearance</span>
            <span class="sub">${describeAppearance(settings, deviceNoun)}</span>
          </span>
          <span class="chev" aria-hidden="true">›</span>
        </a>
        <label class="row switch row-switch">
          <span class="body">
            <span class="title">Technical details</span>
            <span class="sub wrap">Show tool names, raw commands and exit codes</span>
          </span>
          <input
            type="checkbox"
            checked=${technical}
            onChange=${(e) => onUpdateSettings({ technical: e.target.checked })}
          />
        </label>
        <a
          class=${['row', wide && selectedPage === 'refreshing' ? 'selected' : '']
            .filter(Boolean)
            .join(' ')}
          href=${routeHref({ section: 'settings', page: 'refreshing' })}
          aria-current=${wide && selectedPage === 'refreshing' ? 'page' : undefined}
        >
          <span class="body">
            <span class="title">Refreshing</span>
            <span class="sub">${describeRefreshing(settings)}</span>
          </span>
          <span class="chev" aria-hidden="true">›</span>
        </a>
      </div>

      <div class="list spaced">
        <a
          class=${['row', wide && selectedPage === 'about' ? 'selected' : '']
            .filter(Boolean)
            .join(' ')}
          href=${routeHref({ section: 'settings', page: 'about' })}
          aria-current=${wide && selectedPage === 'about' ? 'page' : undefined}
        >
          <span class="body">
            <span class="title">About</span>
            <span class="sub">Legion ${UI_VERSION}</span>
          </span>
          <span class="chev" aria-hidden="true">›</span>
        </a>
      </div>
    </section>
  `;

  function renderPage(page) {
    if (page === 'device') {
      return html`
        <${ThisDevicePage}
          deviceNoun=${deviceNoun}
          link=${link}
          nodeInfo=${nodeInfo}
          tools=${tools}
          technical=${technical}
          onRequestConfirm=${onRequestConfirm}
          onDisconnected=${onDisconnected}
        />
      `;
    }
    if (page === 'connections') {
      return html`
        <${ConnectionsPage}
          route=${route}
          link=${link}
          linkState=${linkState}
          nodeInfo=${nodeInfo}
          tools=${tools}
          allowedSet=${allowedSet}
          snapshot=${snapshot}
          technical=${technical}
          onRequestConfirm=${onRequestConfirm}
          onSelectNode=${onSelectNode}
          onDisconnected=${onDisconnected}
          onConnected=${onConnected}
        />
      `;
    }
    if (page === 'appearance') {
      return html`
        <${AppearancePage}
          deviceNoun=${deviceNoun}
          settings=${settings}
          onUpdateSettings=${onUpdateSettings}
        />
      `;
    }
    if (page === 'refreshing') {
      return html`
        <${RefreshingPage} settings=${settings} onUpdateSettings=${onUpdateSettings} />
      `;
    }
    if (page === 'about') {
      return html`<${AboutPage} nodeInfo=${nodeInfo} />`;
    }
    return null;
  }

  if (wide) {
    return html`
      <div class="split">
        ${indexPane}
        <section>
          <div class="settings-pane">${renderPage(selectedPage)}</div>
        </section>
      </div>
    `;
  }

  if (selectedPage) {
    return renderPage(selectedPage);
  }

  return indexPane;
}
