// Settings pages: This device, Connections (paired + invite QR + fleet.add + incoming devices),
// Appearance, Technical details, Refreshing, and About.

import { html, useEffect, useMemo, useRef, useState } from '../vendor/preact-htm.js';
import { CodeText, CopyButton, Notice, QrCode, SwipeRow } from './common.js';
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
  buildDeviceInviteArgs,
  buildDeviceRemoveArgs,
  buildRelayedConnections,
  describeAppearance,
  describeConnectionRoute,
  describePolicyRolePlain,
  describeRefreshing,
  extractInviteRoles,
  findIncomingDevice,
  formatDeviceRemovedNotice,
  formatInvitedDeviceSyncNotice,
  formatPairSyncNotice,
  isInviteOwnerStrength,
  parsePolicyForInvite,
  summarizeInvitedDeviceFleet,
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
        On the other machine, create an invite (or run <code>legion pair</code>), then scan its
        code or paste the invite below.
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

function InviteDeviceSubpage({
  link,
  tools,
  allowedSet,
  nodeInfo,
  incomingDevices,
  technical,
  onDevicePaired,
}) {
  const inviteSpec = useMemo(() => (tools || []).find((t) => t.name === 'device.invite'), [tools]);
  const inviteProps = inviteSpec?.input_schema?.properties || {};
  const roleProp = inviteProps.role || null;
  const hasNameProp = Boolean(inviteProps.name);
  const hasAllowOwner = Boolean(inviteProps.allow_owner);
  const defaultIdentity = nodeInfo?.whoami?.principal || 'phone';
  const canReadPolicy =
    Boolean((tools || []).find((t) => t.name === 'config.read')) &&
    (!allowedSet || allowedSet.has('config.read'));
  const canListDevices =
    Boolean((tools || []).find((t) => t.name === 'device.list')) &&
    (!allowedSet || allowedSet.has('device.list'));

  const [principal, setPrincipal] = useState(defaultIdentity);
  const [deviceLabel, setDeviceLabel] = useState('');
  const [validMinutes, setValidMinutes] = useState(15);
  const [role, setRole] = useState(roleProp?.default || '');
  const [roleTouched, setRoleTouched] = useState(false);
  const [allowOwner, setAllowOwner] = useState(false);
  const [policy, setPolicy] = useState(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const [inviteRes, setInviteRes] = useState(null);
  const [pairedDelivery, setPairedDelivery] = useState(null);
  const beforeDeviceKeysRef = useRef(new Set());

  useEffect(() => {
    if ((!hasAllowOwner && !roleProp) || !canReadPolicy) {
      setPolicy(null);
      return;
    }
    let alive = true;
    link
      .call('config.read', { document: 'policy.toml' })
      .then((res) => {
        if (!alive) return;
        setPolicy(
          res?.exists && typeof res.content === 'string'
            ? parsePolicyForInvite(res.content)
            : null,
        );
      })
      .catch(() => {
        if (alive) setPolicy(null);
      });
    return () => {
      alive = false;
    };
  }, [link, nodeInfo?.node, hasAllowOwner, roleProp, canReadPolicy]);

  const roleOptions = useMemo(
    () => extractInviteRoles(policy, roleProp, { allowOwner: hasAllowOwner }),
    [policy, roleProp, hasAllowOwner],
  );

  useEffect(() => {
    if (!roleTouched && hasNameProp && roleProp && !role && roleOptions.length > 0) {
      const nonOwner = roleOptions.find((opt) => !opt.ownerStrength);
      setRole((nonOwner || roleOptions[0]).name);
    }
  }, [roleTouched, hasNameProp, roleProp, role, roleOptions]);

  const usesRoleName = Boolean(hasNameProp && roleProp && role.trim());
  const activeLabel = usesRoleName ? deviceLabel : principal;
  const wantsOwner = Boolean(
    hasAllowOwner &&
      isInviteOwnerStrength({
        label: activeLabel,
        role,
        usesRoleName,
        policy,
        whoami: nodeInfo?.whoami,
        incomingDevices,
      }),
  );

  useEffect(() => {
    if (!wantsOwner) setAllowOwner(false);
  }, [wantsOwner]);

  // Poll device.list while an invite is open; once it is consumed, fan out device.list to show
  // which machines received the new device.
  useEffect(() => {
    if (!inviteRes?.id || !canListDevices || pairedDelivery) return undefined;
    let alive = true;
    let checking = false;
    const intervalMs = link?.mockFlags ? 250 : 1500;

    async function poll() {
      if (!alive || checking) return;
      checking = true;
      try {
        const res = await link.call('device.list', {});
        if (!alive || !res) return;
        const invites = Array.isArray(res.invites) ? res.invites : [];
        const stillPending = invites.some((inv) => inv?.id === inviteRes.id);
        if (stillPending) return;

        const devices = Array.isArray(res.devices) ? res.devices : [];
        const before = beforeDeviceKeysRef.current;
        const deviceKey = (d) => d?.id || d?.fingerprint || `${d?.name || ''}:${d?.principal || ''}`;
        const added =
          devices.find((d) => !before.has(deviceKey(d))) ||
          devices.find(
            (d) =>
              (inviteRes.principal && d?.principal === inviteRes.principal) ||
              (inviteRes.requestedLabel && d?.name === inviteRes.requestedLabel),
          ) ||
          null;
        if (!added) return;

        let summary = null;
        try {
          const allRes = await link.call('device.list', {}, { on: 'all' });
          if (alive && allRes?.results) {
            summary = summarizeInvitedDeviceFleet(allRes, added);
          }
        } catch {
          // Best-effort fleet fan-out; still show that the device paired here.
          summary = { received: nodeInfo?.node ? [nodeInfo.node] : [], pending: [] };
        }
        if (!alive) return;
        setPairedDelivery({
          deviceName: added.name || inviteRes.requestedLabel || 'New device',
          summary: summary || { received: nodeInfo?.node ? [nodeInfo.node] : [], pending: [] },
        });
        if (typeof onDevicePaired === 'function') onDevicePaired();
      } catch {
        // Ignore transient poll failures while waiting for the other device to pair.
      } finally {
        checking = false;
      }
    }

    const timer = window.setInterval(poll, intervalMs);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [inviteRes, canListDevices, pairedDelivery, link, nodeInfo?.node, onDevicePaired]);

  async function onCreate(e) {
    e.preventDefault();
    const who = activeLabel.trim();
    if (!who) return;
    setBusy(true);
    setError(null);
    setPairedDelivery(null);
    try {
      const deviceKey = (d) => d?.id || d?.fingerprint || `${d?.name || ''}:${d?.principal || ''}`;
      beforeDeviceKeysRef.current = new Set((incomingDevices || []).map(deviceKey));
      if (canListDevices) {
        try {
          const cur = await link.call('device.list', {});
          if (Array.isArray(cur?.devices)) {
            beforeDeviceKeysRef.current = new Set(cur.devices.map(deviceKey));
          }
        } catch {
          // Keep snapshot from props.
        }
      }

      const args = buildDeviceInviteArgs(inviteSpec, {
        label: who,
        role,
        validMinutes,
        allowOwner: wantsOwner && allowOwner,
      });
      const res = await link.call('device.invite', args);
      setInviteRes({ ...res, validMinutes, requestedLabel: who });
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  const selectedRoleDescription = role.trim()
    ? describePolicyRolePlain(role, policy)
    : null;
  const deliveryNotice = pairedDelivery
    ? formatInvitedDeviceSyncNotice(pairedDelivery.deviceName, pairedDelivery.summary, technical)
    : null;

  return html`
    <div>
      <p class="lede">
        Create a one-time invite on ${nodeInfo?.node || 'this machine'} so a phone, tablet or
        another computer can pair with it.
      </p>

      <form onSubmit=${onCreate}>
        <label class="field">
          <span>
            ${usesRoleName
              ? technical
                ? 'Device name'
                : 'Name for this device'
              : technical
                ? 'Principal'
                : 'Use the identity'}
          </span>
          <input
            type="text"
            value=${activeLabel}
            onInput=${(e) => {
              if (usesRoleName) setDeviceLabel(e.target.value);
              else setPrincipal(e.target.value);
              setAllowOwner(false);
            }}
            placeholder=${usesRoleName ? 'tablet' : defaultIdentity}
            required
          />
          <small>
            ${usesRoleName
              ? technical
                ? 'Creates a dedicated device:<name> principal in policy.toml for this device.'
                : 'Gives this device its own identity on this machine.'
              : technical
                ? 'Matches a principal in policy.toml (for example phone or tablet).'
                : 'A name this machine already knows, such as the one this device uses.'}
          </small>
        </label>

        ${roleProp
          ? html`
              <label class="field">
                <span>Role</span>
                ${roleOptions.length > 0
                  ? html`
                      <select
                        value=${role}
                        onChange=${(e) => {
                          setRoleTouched(true);
                          setRole(e.target.value);
                          setAllowOwner(false);
                        }}
                      >
                        ${roleOptions.map(
                          (opt) => html`
                            <option key=${opt.name} value=${opt.name}>
                              ${`${opt.name} — ${opt.description}`}
                            </option>
                          `,
                        )}
                        ${hasNameProp
                          ? html`
                              <option value="">
                                ${technical
                                  ? 'Use an existing principal'
                                  : 'Share an existing identity'}
                              </option>
                            `
                          : null}
                      </select>
                    `
                  : html`
                      <input
                        type="text"
                        value=${role}
                        onInput=${(e) => {
                          setRoleTouched(true);
                          setRole(e.target.value);
                          setAllowOwner(false);
                        }}
                        placeholder="observer"
                      />
                    `}
                ${selectedRoleDescription
                  ? html`<small>${selectedRoleDescription}.</small>`
                  : hasNameProp
                    ? html`
                        <small>
                          ${technical
                            ? 'Leave blank to reuse an existing principal instead of creating device:<name>.'
                            : 'Leave blank to share an existing identity instead.'}
                        </small>
                      `
                    : null}
              </label>
            `
          : null}

        ${wantsOwner
          ? html`
              <div class="list spaced">
                <label class="row switch row-switch">
                  <span class="body">
                    <span class="title">Allow full owner access</span>
                    <span class="sub">
                      ${technical
                        ? 'This device will be able to do everything, including changing who may do what (passes allow_owner=true).'
                        : 'This device will be able to do everything, including changing who may do what.'}
                    </span>
                  </span>
                  <input
                    type="checkbox"
                    checked=${allowOwner}
                    onChange=${(e) => setAllowOwner(e.target.checked)}
                  />
                </label>
              </div>
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
          <button
            type="submit"
            class="btn primary full"
            disabled=${busy || !activeLabel.trim() || (wantsOwner && !allowOwner)}
          >
            ${busy ? 'Making invite…' : inviteRes ? 'Make a new invite' : 'Make invite'}
          </button>
        </div>
      </form>

      ${deliveryNotice
        ? html`
            <div class="spaced-lg">
              <${Notice}
                title=${deliveryNotice.title}
                hint=${deliveryNotice.detail}
                kind=${deliveryNotice.kind}
              />
            </div>
          `
        : inviteRes?.invite
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
  onAcceptIdentityChange,
  onForgetNode,
}) {
  if (!node) return html`<div class="empty">Connection not found.</div>`;
  const plainPerms = describePlainPermissions(node.whoami, tools);
  const routeInfo = describeConnectionRoute(node, technical);
  const idChanged = node.identity_changed || null;

  return html`
    <div>
      ${idChanged
        ? html`
            <div class="notice" role="status">
              <strong>Identity changed for ${node.node}</strong>
              <span>${routeInfo.hint}</span>
              ${(idChanged.old_fingerprints?.length > 0 || idChanged.new_fingerprints?.length > 0)
                ? html`
                    <details class="more" open=${technical || undefined}>
                      <summary>Host key fingerprints</summary>
                      ${idChanged.old_fingerprints?.length > 0
                        ? html`<p>Previous: <code>${idChanged.old_fingerprints.join(', ')}</code></p>`
                        : null}
                      ${idChanged.new_fingerprints?.length > 0
                        ? html`<p>Reported: <code>${idChanged.new_fingerprints.join(', ')}</code></p>`
                        : null}
                    </details>
                  `
                : null}
              ${onAcceptIdentityChange
                ? html`
                    <div class="actions">
                      <button
                        type="button"
                        class="btn small primary"
                        disabled=${busy}
                        onClick=${() => onAcceptIdentityChange(node.node)}
                      >
                        Accept new identity
                      </button>
                    </div>
                  `
                : null}
            </div>
          `
        : null}

      <div class="list">
        <dl class=${technical ? 'kv' : 'kv plain'}>
          <dt>${technical ? 'node' : 'Machine'}</dt>
          <dd>${node.node}</dd>
          <dt>${technical ? 'route' : 'Reached'}</dt>
          <dd>
            ${routeInfo.label}
            ${routeInfo.hint && !idChanged
              ? html`<br /><span class="dim"><${CodeText} text=${routeInfo.hint} /></span>`
              : null}
          </dd>
          ${node.learned_from
            ? html`
                <dt>${technical ? 'learned_from' : 'Learned from'}</dt>
                <dd>${node.learned_from}</dd>
              `
            : null}
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
                ${node.origin
                  ? html`
                      <dt>origin</dt>
                      <dd>${node.origin}</dd>
                    `
                  : null}
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
        ${!isActive && onSelectNode && !idChanged
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
      summary: device.local_only
        ? `${device.name} is only paired with this machine and will lose access here right away.`
        : `${device.name} will lose access to this machine right away.`,
      confirmLabel: 'Remove access',
      danger: true,
      async onConfirm() {
        setBusy(true);
        setError(null);
        try {
          const res = await link.call('device.remove', buildDeviceRemoveArgs(device));
          onRemoved(formatDeviceRemovedNotice(device, res));
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
      ${device.status ? html`<${Notice} title=${device.status} kind="warn" />` : null}
      <div class="list">
        <dl class=${technical ? 'kv' : 'kv plain'}>
          <dt>${technical ? 'name' : 'Device'}</dt>
          <dd>${device.name}</dd>
          <dt>${technical ? 'principal' : 'Identity'}</dt>
          <dd>${device.principal || 'Unknown'}</dd>
          ${device.role
            ? html`
                <dt>${technical ? 'role' : 'Access'}</dt>
                <dd>${technical ? device.role : describeRolePlain(device.role)}</dd>
              `
            : null}
          ${device.added
            ? html`
                <dt>${technical ? 'added' : 'Paired'}</dt>
                <dd>${formatRelativeTime(device.added)}</dd>
              `
            : null}
          ${technical && device.id
            ? html`
                <dt>id</dt>
                <dd>${device.id}</dd>
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
  pairNotice,
  onDismissPairNotice,
  onRequestConfirm,
  onSelectNode,
  onAcceptIdentityChange,
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

  async function handleAcceptIdentity(nodeName) {
    if (!onAcceptIdentityChange) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await onAcceptIdentityChange(nodeName);
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
        allowedSet=${allowedSet}
        nodeInfo=${nodeInfo}
        incomingDevices=${incoming?.devices || []}
        technical=${technical}
        onDevicePaired=${loadIncoming}
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
        onAcceptIdentityChange=${onAcceptIdentityChange ? handleAcceptIdentity : null}
        onForgetNode=${forgetNode}
      />
    `;
  }

  if (route.sub === 'incoming' && route.item) {
    const dev = findIncomingDevice(incoming?.devices, route.item);
    return html`
      <${IncomingDeviceSubpage}
        device=${dev}
        tools=${tools}
        allowedSet=${allowedSet}
        technical=${technical}
        link=${link}
        onRequestConfirm=${onRequestConfirm}
        onRemoved=${(removedNotice) => {
          setNotice(removedNotice || null);
          loadIncoming();
          window.location.hash = '#/settings/connections';
        }}
      />
    `;
  }

  const nodes = linkState?.nodes || [];
  const relayed = link.kind === 'app' ? buildRelayedConnections(linkState, snapshot) : [];
  const identityChangedNodes = nodes.filter((n) => Boolean(n.identity_changed));
  const syncBanner = formatPairSyncNotice(pairNotice, technical);
  const hasMakeSection = link.kind === 'app' || canInvite || canFleetAdd;
  const incomingDevices = incoming?.devices || [];

  return html`
    <div>
      ${error ? html`<${Notice} error=${error} />` : null}
      ${notice ? html`<${Notice} title=${notice} kind="warn" />` : null}
      ${syncBanner
        ? html`
            <${Notice}
              title=${syncBanner.title}
              hint=${syncBanner.detail}
              kind=${syncBanner.kind}
              onDismiss=${onDismissPairNotice}
            />
          `
        : null}

      ${identityChangedNodes.map((n) => {
        const info = describeConnectionRoute(n, technical);
        return html`
          <div class="notice" key=${`idchg:${n.node}`} role="status">
            <strong>Identity changed for ${n.node}</strong>
            <span>${info.hint}</span>
            <div class="actions">
              <a
                class="btn small"
                href=${routeHref({
                  section: 'settings',
                  page: 'connections',
                  sub: 'node',
                  item: n.node,
                })}
              >
                Inspect
              </a>
              ${onAcceptIdentityChange
                ? html`
                    <button
                      type="button"
                      class="btn small primary"
                      disabled=${busy}
                      onClick=${() => handleAcceptIdentity(n.node)}
                    >
                      Accept new identity
                    </button>
                  `
                : null}
            </div>
          </div>
        `;
      })}

      <p class="label">Paired machines</p>
      ${nodes.length > 0
        ? html`
            <div class="list">
              ${nodes.map((n, idx) => {
                const routeInfo = describeConnectionRoute(n, technical);
                const dotCls = routeInfo.isIdentityChanged
                  ? 'dot busy'
                  : idx === 0
                    ? 'dot'
                    : 'dot off';
                const routeLine =
                  routeInfo.kind === 'not_synced' && routeInfo.hint
                    ? `${routeInfo.label} · ${routeInfo.hint}`
                    : routeInfo.label;
                const sub = technical
                  ? `${routeLine} · ${n.principal} · ${n.role} · ${n.via}`
                  : link.kind === 'app'
                    ? `${idx === 0 ? 'Using now · ' : ''}${routeLine}`
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
                    <span class=${dotCls} aria-hidden="true"></span>
                    <span class="body">
                      <span class="title mono">${n.node}</span>
                      <span class="sub wrap"><${CodeText} text=${sub} /></span>
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

      ${relayed.length > 0
        ? html`
            <p class="label">Reached through another machine</p>
            <div class="list">
              ${relayed.map((entry) => {
                const info = describeConnectionRoute(entry, technical);
                const sub =
                  info.hint && info.kind === 'not_synced'
                    ? `${info.label} · ${info.hint}`
                    : info.label;
                return html`
                  <div class="row" key=${`relay:${entry.node}`}>
                    <span class="dot off" aria-hidden="true"></span>
                    <span class="body">
                      <span class="title mono">${entry.node}</span>
                      <span class="sub wrap"><${CodeText} text=${sub} /></span>
                    </span>
                  </div>
                `;
              })}
            </div>
          `
        : null}

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
                          key=${dev.id || dev.name}
                          class="row"
                          href=${routeHref({
                            section: 'settings',
                            page: 'connections',
                            sub: 'incoming',
                            item: dev.id || dev.name,
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
  pairNotice,
  onDismissPairNotice,
  onUpdateSettings,
  onRequestConfirm,
  onSelectNode,
  onAcceptIdentityChange,
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
          pairNotice=${pairNotice}
          onDismissPairNotice=${onDismissPairNotice}
          onRequestConfirm=${onRequestConfirm}
          onSelectNode=${onSelectNode}
          onAcceptIdentityChange=${onAcceptIdentityChange}
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
