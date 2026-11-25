// Settings pages: This device, Connections (paired + invite QR + fleet.add + incoming devices),
// Appearance, Technical details, Refreshing, and About.

import { html, useEffect, useMemo, useRef, useState } from '../vendor/preact-htm.js';
import { Icon } from '../icons.js';
import { CodeText, CopyButton, Notice, QrCode, SwipeRow } from './common.js';
import { SplitView } from './split-view.js';
import { ToolDetail } from './tools.js';
import {
  describePlainPermissions,
  describeRolePlain,
  formatInviteValidity,
} from '../lib/activity.js';
import { formatRelativeTime } from '../lib/format.js';
import { createPairingController } from '../lib/pairing.js';
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
  formatDisagreementNotice,
  formatInvitedDeviceSyncNotice,
  formatLearnRejectionTechnical,
  formatPairSyncNotice,
  isInviteOwnerStrength,
  normalizeNodeRoles,
  parsePolicyForInvite,
  summarizeInvitedDeviceFleet,
} from '../lib/settings.js';

import {
  describeUpdateSummary,
  formatUpdateTechnicalRows,
} from '../lib/updates.js';

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
  hideFirstLabel = false,
  onRequestConfirm,
  onDisconnected,
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const whoami = nodeInfo?.whoami;
  const unreachable = nodeInfo?.reachable === false;
  const checkedAt = nodeInfo?.roleCheckedAt || nodeInfo?.role_checked_at || null;
  const asOfSuffix =
    unreachable || whoami?.stale
      ? checkedAt
        ? ` (as of ${formatRelativeTime(checkedAt)})`
        : ' (last known)'
      : '';
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
      ${unreachable
        ? html`
            <${Notice}
              title=${`${nodeInfo?.node || 'This machine'} is unreachable right now.`}
              hint=${checkedAt
                ? `Showing the last known role on ${nodeInfo?.node || 'this machine'} as of ${formatRelativeTime(checkedAt)}.`
                : `Showing the last known role on ${nodeInfo?.node || 'this machine'}.`}
              kind="warn"
            />
          `
        : null}

      ${!(hideFirstLabel && !error && !unreachable) ? html`<p class="label">Identity</p>` : null}
      <div class="list">
        <dl class=${technical ? 'kv' : 'kv plain'}>
          <dt>${technical ? 'principal' : 'Name'}</dt>
          <dd>${whoami?.principal || nodeInfo?.principal || 'Unknown'}</dd>
          <dt>${technical ? 'node' : 'Connected to'}</dt>
          <dd>${nodeInfo?.node || whoami?.node || 'Unknown'}${unreachable ? ' (unreachable)' : ''}</dd>
          <dt>${technical ? 'access' : 'What it can do'}</dt>
          <dd>${plainPerms}${asOfSuffix}</dd>
          ${technical
            ? html`
                <dt>role</dt>
                <dd>${whoami?.role || nodeInfo?.role || 'unknown'}${asOfSuffix}</dd>
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

function PairMachineSubpage({
  link,
  linkState,
  onConnected,
  pairingController = null,
  pairingState = null,
}) {
  const [scanning, setScanning] = useState(false);
  const [scanError, setScanError] = useState(null);
  const canScan = linkState?.can_scan !== false;

  const fallbackController = useMemo(
    () =>
      pairingController
        ? null
        : createPairingController({
            link,
            onPaired: ({ next }) => {
              window.location.hash = '#/settings/connections';
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
  const error = pState.error || scanError;
  const busy = scanning || pairing;

  useEffect(() => {
    if (typeof link.deviceName !== 'function') return;
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
  }, [link, ctrl]);

  function cancelPairing() {
    ctrl.cancelPair();
  }

  async function onScan() {
    setScanError(null);
    ctrl.clearOutcome();
    setScanning(true);
    try {
      const scanned = await link.scanInvite();
      if (scanned) {
        setScanning(false);
        await ctrl.startPair({
          invite: scanned,
          deviceName: ctrl.getState().deviceName,
          source: 'scan',
          screen: 'settings',
        });
      }
    } catch (err) {
      if (err?.code !== 'cancelled') {
        setScanError(err);
      }
    } finally {
      setScanning(false);
    }
  }

  async function onSubmit(e) {
    e.preventDefault();
    if (!invite.trim()) return;
    setScanError(null);
    await ctrl.startPair({
      invite: invite.trim(),
      deviceName: ctrl.getState().deviceName,
      source: 'paste',
      screen: 'settings',
    });
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
          onInput=${(e) => ctrl.setDeviceName(e.target.value)}
          placeholder="phone"
          disabled=${busy}
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
              ${pairNotice.retryable && invite
                ? html`
                    <div class="actions">
                      <button
                        type="button"
                        class="btn small primary"
                        disabled=${busy}
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
                class="btn primary full"
                disabled=${busy}
                onClick=${() => ctrl.retryPair()}
              >
                Try again
              </button>
            </div>
          `
        : null}
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
            onInput=${(e) => ctrl.setInvite(e.target.value)}
            placeholder="Paste the invite line"
            disabled=${busy}
            required
          ></textarea>
        </label>
        <div class="actions">
          <button type="submit" class=${canScan ? 'btn full' : 'btn primary full'} disabled=${busy}>
            ${pairing ? 'Pairing…' : 'Pair machine'}
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
    </div>
  `;
}

function InviteDeviceSubpage({
  link,
  capsStore = null,
  linkState,
  nodeInfo,
  technical,
  onDevicePaired,
}) {
  const availableNodes = useMemo(
    () => (linkState?.nodes || []).map((n) => n?.node).filter(Boolean),
    [linkState],
  );
  const initialTarget = nodeInfo?.node || availableNodes[0] || '';
  const [targetNode, setTargetNode] = useState(initialTarget);

  useEffect(() => {
    if (!targetNode && initialTarget) {
      setTargetNode(initialTarget);
    } else if (targetNode && availableNodes.length > 0 && !availableNodes.includes(targetNode)) {
      setTargetNode(initialTarget);
    }
  }, [targetNode, initialTarget, availableNodes]);

  // Load tool catalogue, caller identity, and existing devices strictly from `targetNode`.
  // Never fall back to another machine's tool catalogue, permissions, or device list.
  const [catalog, setCatalog] = useState({
    status: 'loading',
    node: '',
    tools: [],
    error: null,
  });
  const [targetWhoami, setTargetWhoami] = useState(null);
  const [targetDevices, setTargetDevices] = useState([]);

  useEffect(() => {
    if (!targetNode || (!capsStore && typeof link.tools !== 'function')) {
      setCatalog({ status: 'error', node: targetNode, tools: [], error: null });
      setTargetWhoami(null);
      setTargetDevices([]);
      return;
    }
    let alive = true;
    setCatalog({ status: 'loading', node: targetNode, tools: [], error: null });
    setTargetWhoami(null);
    setTargetDevices([]);

    const toolsPromise = capsStore
      ? capsStore.ensure(targetNode).then(
          (entry) => ({ ok: true, list: Array.isArray(entry?.tools) ? entry.tools : [] }),
          (err) => ({ ok: false, error: err }),
        )
      : link.tools({ on: targetNode }).then(
          (list) => ({ ok: true, list: Array.isArray(list) ? list : [] }),
          (err) => ({ ok: false, error: err }),
        );

    Promise.all([
      toolsPromise,
      link
        .call('meta.whoami', {}, { on: targetNode })
        .catch(() => null),
    ]).then(([toolsRes, whoamiRes]) => {
      if (!alive) return;
      if (whoamiRes && (!whoamiRes.node || whoamiRes.node === targetNode)) {
        setTargetWhoami(whoamiRes);
      }
      if (toolsRes.ok && toolsRes.list.length > 0) {
        setCatalog({ status: 'ready', node: targetNode, tools: toolsRes.list, error: null });
      } else {
        setCatalog({
          status: 'error',
          node: targetNode,
          tools: [],
          error: toolsRes.ok ? null : toolsRes.error,
        });
      }
    });

    return () => {
      alive = false;
    };
  }, [link, capsStore, targetNode]);

  const targetToolsReady = catalog.status === 'ready' && catalog.node === targetNode;
  const activeTools = targetToolsReady ? catalog.tools : [];
  const targetAllowedSet = useMemo(
    () => (Array.isArray(targetWhoami?.tools) ? new Set(targetWhoami.tools) : null),
    [targetWhoami],
  );
  const inviteSpec = useMemo(() => activeTools.find((t) => t.name === 'device.invite'), [activeTools]);
  const inviteProps = inviteSpec?.input_schema?.properties || {};
  const roleProp = inviteProps.role || null;
  const hasNameProp = Boolean(inviteProps.name);
  const hasAllowOwner = Boolean(inviteProps.allow_owner);
  const defaultIdentity = targetWhoami?.principal || 'phone';
  const hasDeviceRolesTool = Boolean(activeTools.find((t) => t.name === 'device.roles'));
  const canReadPolicy =
    Boolean(activeTools.find((t) => t.name === 'config.read')) &&
    (!targetAllowedSet || targetAllowedSet.has('config.read'));
  const canListDevices =
    Boolean(activeTools.find((t) => t.name === 'device.list')) &&
    (!targetAllowedSet || targetAllowedSet.has('device.list'));

  useEffect(() => {
    if (!targetToolsReady || !canListDevices || !targetNode) {
      setTargetDevices([]);
      return;
    }
    let alive = true;
    link
      .call('device.list', {}, { on: targetNode })
      .then((res) => {
        if (alive) setTargetDevices(Array.isArray(res?.devices) ? res.devices : []);
      })
      .catch(() => {
        if (alive) setTargetDevices([]);
      });
    return () => {
      alive = false;
    };
  }, [link, targetNode, targetToolsReady, canListDevices]);

  const [principal, setPrincipal] = useState(defaultIdentity);
  const [deviceLabel, setDeviceLabel] = useState('');
  const [validMinutes, setValidMinutes] = useState(15);
  const [role, setRole] = useState(roleProp?.default || '');
  const [roleTouched, setRoleTouched] = useState(false);
  const [allowOwner, setAllowOwner] = useState(false);
  const [nodeRoles, setNodeRoles] = useState(null);
  const [policy, setPolicy] = useState(null);
  const [rolesStatus, setRolesStatus] = useState('idle'); // 'idle' | 'loading' | 'ready' | 'refused'
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const [inviteRes, setInviteRes] = useState(null);
  const [pairedDelivery, setPairedDelivery] = useState(null);
  const beforeDeviceKeysRef = useRef(new Set());

  // Load roles from `device.roles` on the target machine when offered; otherwise fall back to
  // reading `policy.toml` on that same named machine. Never guess if both are refused/unavailable.
  useEffect(() => {
    if (!targetToolsReady || !targetNode || (!hasAllowOwner && !roleProp)) {
      setNodeRoles(null);
      setPolicy(null);
      setRolesStatus('idle');
      return;
    }
    let alive = true;
    setRolesStatus('loading');
    setNodeRoles(null);
    setPolicy(null);

    async function loadRoles() {
      if (hasDeviceRolesTool) {
        try {
          const res = await link.call('device.roles', {}, { on: targetNode });
          if (!alive) return;
          const parsed = normalizeNodeRoles(res);
          if (parsed) {
            setNodeRoles(parsed);
            setRolesStatus('ready');
            return;
          }
        } catch (err) {
          if (!alive) return;
          if (err?.code === 'denied') {
            setRolesStatus('refused');
            return;
          }
          // Fall through to policy.toml only when device.roles is not supported/unknown.
        }
      }

      if (!canReadPolicy) {
        if (alive) setRolesStatus(roleProp ? 'refused' : 'idle');
        return;
      }

      try {
        const res = await link.call('config.read', { document: 'policy.toml' }, { on: targetNode });
        if (!alive) return;
        if (res?.exists && typeof res.content === 'string') {
          setPolicy(parsePolicyForInvite(res.content));
          setRolesStatus('ready');
        } else {
          setPolicy(null);
          setRolesStatus(roleProp ? 'refused' : 'idle');
        }
      } catch {
        if (!alive) return;
        setPolicy(null);
        setRolesStatus(roleProp ? 'refused' : 'idle');
      }
    }

    loadRoles();
    return () => {
      alive = false;
    };
  }, [link, targetNode, targetToolsReady, hasAllowOwner, roleProp, hasDeviceRolesTool, canReadPolicy]);

  const roleOptions = useMemo(() => {
    if (Array.isArray(nodeRoles)) return nodeRoles;
    if (!policy && !(Array.isArray(roleProp?.enum) && roleProp.enum.length > 0)) return [];
    return extractInviteRoles(policy, roleProp, {
      allowOwner: hasAllowOwner,
      tools: activeTools,
      technical,
    });
  }, [nodeRoles, policy, roleProp, hasAllowOwner, activeTools, technical]);

  useEffect(() => {
    if (!roleTouched && hasNameProp && roleProp && !role && roleOptions.length > 0) {
      const nonOwner = roleOptions.find((opt) => !opt.ownerStrength);
      setRole((nonOwner || roleOptions[0]).name);
    }
  }, [roleTouched, hasNameProp, roleProp, role, roleOptions]);

  const selectedRoleOption = useMemo(
    () => roleOptions.find((opt) => opt.name === role.trim()) || null,
    [roleOptions, role],
  );
  const usesRoleName = Boolean(hasNameProp && roleProp && role.trim());
  const activeLabel = usesRoleName ? deviceLabel : principal;
  const wantsOwner = Boolean(
    hasAllowOwner &&
      (usesRoleName && selectedRoleOption
        ? selectedRoleOption.ownerStrength
        : isInviteOwnerStrength({
            label: activeLabel,
            role,
            usesRoleName,
            policy,
            whoami: targetWhoami,
            incomingDevices: targetDevices,
          })),
  );

  useEffect(() => {
    if (!wantsOwner) setAllowOwner(false);
  }, [wantsOwner]);

  // Poll device.list on the exact same named target machine while an invite is open; once it is
  // consumed, fan out device.list to show which machines received the new device.
  useEffect(() => {
    if (!inviteRes?.id || !canListDevices || !targetNode || pairedDelivery) return undefined;
    let alive = true;
    let checking = false;
    const intervalMs = link?.mockFlags ? 250 : 1500;
    const inviteNode = inviteRes.targetNode || targetNode;

    async function poll() {
      if (!alive || checking) return;
      checking = true;
      try {
        const res = await link.call('device.list', {}, { on: inviteNode });
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
          summary = { received: inviteNode ? [inviteNode] : [], pending: [] };
        }
        if (!alive) return;
        setPairedDelivery({
          deviceName: added.name || inviteRes.requestedLabel || 'New device',
          summary: summary || { received: inviteNode ? [inviteNode] : [], pending: [] },
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
  }, [inviteRes, canListDevices, targetNode, pairedDelivery, link, onDevicePaired]);

  async function onCreate(e) {
    e.preventDefault();
    const who = activeLabel.trim();
    if (!who || !targetNode || !targetToolsReady || !inviteSpec) return;
    setBusy(true);
    setError(null);
    setPairedDelivery(null);
    try {
      const deviceKey = (d) => d?.id || d?.fingerprint || `${d?.name || ''}:${d?.principal || ''}`;
      beforeDeviceKeysRef.current = new Set((targetDevices || []).map(deviceKey));
      if (canListDevices) {
        try {
          const cur = await link.call('device.list', {}, { on: targetNode });
          if (Array.isArray(cur?.devices)) {
            setTargetDevices(cur.devices);
            beforeDeviceKeysRef.current = new Set(cur.devices.map(deviceKey));
          }
        } catch {
          // Keep target-scoped device snapshot.
        }
      }

      const args = buildDeviceInviteArgs(inviteSpec, {
        label: who,
        role,
        validMinutes,
        allowOwner: wantsOwner && allowOwner,
      });
      const res = await link.call('device.invite', args, { on: targetNode });
      setInviteRes({
        ...res,
        validMinutes,
        requestedLabel: who,
        targetNode,
        roleDescription: selectedRoleOption?.description || null,
      });
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  const selectedRoleDescription = selectedRoleOption?.description || '';
  const deliveryNotice = pairedDelivery
    ? formatInvitedDeviceSyncNotice(pairedDelivery.deviceName, pairedDelivery.summary, technical)
    : null;
  const rolesRefused = Boolean(roleProp && rolesStatus === 'refused' && roleOptions.length === 0);

  return html`
    <div>
      <p class="lede">
        Create a one-time invite on ${targetNode || 'this machine'} so a phone, tablet or another
        computer can pair with it.
      </p>

      <form onSubmit=${onCreate}>
        ${availableNodes.length > 1
          ? html`
              <label class="field">
                <span>Machine</span>
                <select
                  value=${targetNode}
                  onChange=${(e) => {
                    setTargetNode(e.target.value);
                    setRole('');
                    setRoleTouched(false);
                    setAllowOwner(false);
                    setError(null);
                    setInviteRes(null);
                    setPairedDelivery(null);
                  }}
                >
                  ${availableNodes.map(
                    (name) => html`<option key=${name} value=${name}>${name}</option>`,
                  )}
                </select>
              </label>
            `
          : null}

        ${catalog.status === 'loading'
          ? html`<p class="lede">Checking ${targetNode || 'this machine'}…</p>`
          : catalog.status === 'error'
            ? html`
                <${Notice}
                  kind="bad"
                  title=${`Could not read what ${targetNode || 'this machine'} supports right now.`}
                  hint=${technical && catalog.error?.message ? catalog.error.message : null}
                />
              `
            : !inviteSpec
              ? html`
                  <${Notice}
                    kind="warn"
                    title=${`${targetNode || 'This machine'} does not offer device invites for this account.`}
                  />
                `
              : html`
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
                ${rolesRefused
                  ? html`
                      <${Notice}
                        kind="warn"
                        title=${`This device may not see the roles on ${targetNode || 'this machine'}; ask the owner.`}
                      />
                    `
                  : roleOptions.length > 0
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
                                ${opt.description ? `${opt.name} — ${opt.description}` : opt.name}
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
                          placeholder="role name"
                        />
                      `}
                ${!rolesRefused && selectedRoleDescription
                  ? html`<small>${capitalize(selectedRoleDescription)}${selectedRoleDescription.endsWith('.') ? '' : '.'}</small>`
                  : !rolesRefused && hasNameProp
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
            disabled=${busy || !activeLabel.trim() || !targetNode || (wantsOwner && !allowOwner)}
          >
            ${busy ? 'Making invite…' : inviteRes ? 'Make a new invite' : 'Make invite'}
          </button>
        </div>
              `}
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
                    : `Works once, for the next ${formatInviteValidity(inviteRes.validMinutes)}. The device will be able to: ${inviteRes.roleDescription || describeRolePlain(inviteRes.role)}.`}
                </p>
              </div>
            `
          : null}
    </div>
  `;
}

function sameList(a, b) {
  const la = Array.isArray(a) ? a : [];
  const lb = Array.isArray(b) ? b : [];
  return la.length === lb.length && la.every((v, i) => v === lb[i]);
}

function PairedNodeSubpage({
  node,
  isActive,
  link,
  capsStore = null,
  tools,
  technical,
  busy,
  onSelectNode,
  onAcceptIdentityChange,
  onDismissIdentityChange,
  onForgetNode,
}) {
  const [liveWhoami, setLiveWhoami] = useState(null);
  const [liveTools, setLiveTools] = useState(null);

  useEffect(() => {
    setLiveWhoami(null);
    setLiveTools(null);
    if (!node?.node || node.whoami?.node === node.node) return;
    let alive = true;
    link
      .call('meta.whoami', {}, { on: node.node })
      .then((res) => {
        if (alive && res) setLiveWhoami(res);
      })
      .catch(() => {});
    if (capsStore) {
      capsStore
        .ensure(node.node)
        .then((entry) => {
          if (alive && Array.isArray(entry?.tools)) setLiveTools(entry.tools);
        })
        .catch(() => {});
    } else if (typeof link.tools === 'function') {
      link
        .tools({ on: node.node })
        .then((list) => {
          if (alive && Array.isArray(list)) setLiveTools(list);
        })
        .catch(() => {});
    }
    return () => {
      alive = false;
    };
  }, [link, capsStore, node?.node, node?.whoami?.node]);

  if (!node) return html`<div class="empty">Connection not found.</div>`;

  const whoami = node.whoami?.node === node.node ? node.whoami : liveWhoami;
  const nodeTools = liveTools || tools;
  const plainPerms = whoami ? describePlainPermissions(whoami, nodeTools) : 'Unknown';
  const shownPrincipal = whoami?.principal || node.principal || 'Unknown';
  const shownRole = whoami?.role || (node.role && node.role !== 'unknown' ? node.role : 'unknown');
  const routeInfo = describeConnectionRoute(node, technical);
  const idChanged = node.identity_changed || null;
  const isPairedChange = Boolean(idChanged && (idChanged.is_paired || node.origin === 'paired'));
  const conflicts = Array.isArray(idChanged?.conflicts) ? idChanged.conflicts : [];
  const hasConflicts = conflicts.length > 0;
  const hostsChanged =
    Boolean(idChanged?.old_hosts?.length) &&
    Boolean(idChanged?.hosts?.length) &&
    !sameList(idChanged.old_hosts, idChanged.hosts);
  const portChanged =
    Boolean(idChanged?.old_port) &&
    Boolean(idChanged?.port) &&
    idChanged.old_port !== idChanged.port;
  const userChanged =
    Boolean(idChanged?.old_user) &&
    Boolean(idChanged?.user) &&
    idChanged.old_user !== idChanged.user;

  return html`
    <div>
      ${idChanged
        ? html`
            <div class="notice" role="status">
              <strong>Identity changed for ${node.node}</strong>
              <span>${routeInfo.hint}</span>
              <dl class=${technical ? 'kv spaced' : 'kv plain spaced'}>
                ${idChanged.old_fingerprints?.length > 0
                  ? html`
                      <dt>Previous fingerprint</dt>
                      <dd class="mono">${idChanged.old_fingerprints.join(', ')}</dd>
                    `
                  : null}
                ${idChanged.new_fingerprints?.length > 0
                  ? html`
                      <dt>Reported fingerprint</dt>
                      <dd class="mono">${idChanged.new_fingerprints.join(', ')}</dd>
                    `
                  : null}
                ${hostsChanged
                  ? html`
                      <dt>Previous address</dt>
                      <dd class="mono">${idChanged.old_hosts.join(', ')}</dd>
                      <dt>Reported address</dt>
                      <dd class="mono">${idChanged.hosts.join(', ')}</dd>
                    `
                  : null}
                ${portChanged
                  ? html`
                      <dt>Previous port</dt>
                      <dd class="mono">${String(idChanged.old_port)}</dd>
                      <dt>Reported port</dt>
                      <dd class="mono">${String(idChanged.port)}</dd>
                    `
                  : null}
                ${userChanged
                  ? html`
                      <dt>Previous account</dt>
                      <dd class="mono">${idChanged.old_user}</dd>
                      <dt>Reported account</dt>
                      <dd class="mono">${idChanged.user}</dd>
                    `
                  : null}
              </dl>
              ${hasConflicts
                ? html`
                    <div class="spaced">
                      <strong>Conflicting reports</strong>
                      ${conflicts.map(
                        (c, i) => html`
                          <p key=${i}>
                            ${c.reported_by} reported <code>${(c.fingerprints || []).join(', ') || (c.hosts || []).join(', ')}</code>
                            ${c.hosts?.length ? ` (${c.hosts.join(', ')}:${c.port})` : ''}
                          </p>
                        `,
                      )}
                    </div>
                  `
                : null}
              <div class="actions">
                ${!isPairedChange && !hasConflicts && onAcceptIdentityChange
                  ? html`
                      <button
                        type="button"
                        class="btn small primary"
                        disabled=${busy}
                        onClick=${() =>
                          onAcceptIdentityChange(node.node, idChanged.new_fingerprints || [])}
                      >
                        Accept new identity
                      </button>
                    `
                  : null}
                ${isPairedChange && link.kind === 'app'
                  ? html`
                      <a
                        class="btn small primary"
                        href=${routeHref({
                          section: 'settings',
                          page: 'connections',
                          sub: 'pair',
                        })}
                      >
                        Pair again
                      </a>
                    `
                  : null}
                ${onDismissIdentityChange
                  ? html`
                      <button
                        type="button"
                        class="btn small"
                        disabled=${busy}
                        onClick=${() => onDismissIdentityChange(node.node)}
                      >
                        Dismiss
                      </button>
                    `
                  : null}
              </div>
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
          <dd>${shownPrincipal}</dd>
          <dt>${technical ? 'access' : 'What it can do'}</dt>
          <dd>${plainPerms}</dd>
          ${technical
            ? html`
                <dt>role</dt>
                <dd>${shownRole}</dd>
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
  connectionsNode,
  nodeInfo,
  tools,
  allowedSet,
  roleDescriptions,
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

  const roleDesc =
    (device.role && roleDescriptions?.[device.role]) ||
    (device.role ? describeRolePlain(device.role) : '');

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
          const res = await link.call(
            'device.remove',
            buildDeviceRemoveArgs(device),
            { on: connectionsNode || undefined },
          );
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
          ${technical
            ? html`
                <dt>principal</dt>
                <dd>${device.principal || 'Unknown'}</dd>
              `
            : null}
          ${device.role
            ? html`
                <dt>${technical ? 'role' : 'What it can do'}</dt>
                <dd>${technical ? device.role : capitalize(roleDesc)}</dd>
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
                initialTarget=${connectionsNode || 'local'}
                hideTargetPicker=${true}
                nodeInfo=${nodeInfo}
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
  capsStore = null,
  onRegisterRefresh = null,
  linkState,
  pairingController = null,
  pairingState = null,
  nodeInfo,
  tools,
  allowedSet,
  snapshot,
  technical,
  hideFirstLabel = false,
  pairNotice,
  onDismissPairNotice,
  onRequestConfirm,
  onSelectNode,
  onAcceptIdentityChange,
  onDismissIdentityChange,
  onDisconnected,
  onConnected,
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const [notice, setNotice] = useState(null);
  const [incoming, setIncoming] = useState(null);
  const [roleDescriptions, setRoleDescriptions] = useState({});

  const connectionsNode = nodeInfo?.node || linkState?.nodes?.[0]?.node || '';
  const canInvite =
    Boolean((tools || []).find((t) => t.name === 'device.invite')) &&
    (!allowedSet || allowedSet.has('device.invite'));
  const canFleetAdd =
    Boolean((tools || []).find((t) => t.name === 'fleet.add')) &&
    (!allowedSet || allowedSet.has('fleet.add'));
  const canListDevices =
    Boolean((tools || []).find((t) => t.name === 'device.list')) &&
    (!allowedSet || allowedSet.has('device.list'));
  const hasDeviceRoles = Boolean((tools || []).find((t) => t.name === 'device.roles'));
  const canReadPolicy =
    Boolean((tools || []).find((t) => t.name === 'config.read')) &&
    (!allowedSet || allowedSet.has('config.read'));

  async function loadIncoming() {
    if (!canListDevices || !connectionsNode) return;
    try {
      const res = await link.call('device.list', {}, { on: connectionsNode });
      setIncoming(res);
    } catch {
      setIncoming(null);
    }
  }

  useEffect(() => {
    loadIncoming();
  }, [canListDevices, connectionsNode, route.sub]);

  useEffect(() => {
    if (typeof onRegisterRefresh !== 'function' || route.sub) return undefined;
    return onRegisterRefresh(() => loadIncoming());
  }, [onRegisterRefresh, route.sub, canListDevices, connectionsNode]);

  useEffect(() => {
    if (!canListDevices || !connectionsNode) return;
    let alive = true;
    (async () => {
      if (hasDeviceRoles) {
        try {
          const res = await link.call('device.roles', {}, { on: connectionsNode });
          const roles = normalizeNodeRoles(res);
          if (alive && roles) {
            const map = {};
            for (const r of roles) {
              if (r.name && r.description) map[r.name] = r.description;
            }
            setRoleDescriptions(map);
            return;
          }
        } catch {
          // Fall back to policy.toml below.
        }
      }
      if (!canReadPolicy) return;
      try {
        const res = await link.call('config.read', { document: 'policy.toml' }, { on: connectionsNode });
        if (!alive || !res?.exists || typeof res.content !== 'string') return;
        const parsed = parsePolicyForInvite(res.content);
        const extracted = extractInviteRoles(parsed, null, {
          allowOwner: true,
          tools,
          technical: false,
        });
        const map = {};
        for (const r of extracted) {
          if (r.name && r.description) map[r.name] = r.description;
        }
        setRoleDescriptions(map);
      } catch {
        // Ignore; fallback description handles missing policy.
      }
    })();
    return () => {
      alive = false;
    };
  }, [link, canListDevices, connectionsNode, hasDeviceRoles, canReadPolicy, tools]);

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

  async function handleAcceptIdentity(nodeName, fingerprints) {
    if (!onAcceptIdentityChange) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await onAcceptIdentityChange(nodeName, fingerprints);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  async function handleDismissIdentity(nodeName) {
    if (!onDismissIdentityChange) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await onDismissIdentityChange(nodeName);
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
        pairingController=${pairingController}
        pairingState=${pairingState}
        onConnected=${onConnected}
      />
    `;
  }

  if (route.sub === 'invite' && canInvite) {
    return html`
      <${InviteDeviceSubpage}
        link=${link}
        capsStore=${capsStore}
        linkState=${linkState}
        nodeInfo=${nodeInfo}
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
        initialTarget=${connectionsNode || 'local'}
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
        link=${link}
        capsStore=${capsStore}
        tools=${tools}
        technical=${technical}
        busy=${busy}
        onSelectNode=${onSelectNode ? handleSelectNode : null}
        onAcceptIdentityChange=${onAcceptIdentityChange ? handleAcceptIdentity : null}
        onDismissIdentityChange=${onDismissIdentityChange ? handleDismissIdentity : null}
        onForgetNode=${forgetNode}
      />
    `;
  }

  if (route.sub === 'incoming' && route.item) {
    const dev = findIncomingDevice(incoming?.devices, route.item);
    return html`
      <${IncomingDeviceSubpage}
        device=${dev}
        connectionsNode=${connectionsNode}
        nodeInfo=${nodeInfo}
        tools=${tools}
        allowedSet=${allowedSet}
        roleDescriptions=${roleDescriptions}
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
  const disagreements = Array.isArray(linkState?.disagreements) ? linkState.disagreements : [];
  const syncBanner = formatPairSyncNotice(pairNotice, technical);
  const hasMakeSection = link.kind === 'app' || canInvite || canFleetAdd;
  const incomingDevices = incoming?.devices || [];

  function formatIncomingSubtitle(dev) {
    const addedPart = dev.added ? `paired ${formatRelativeTime(dev.added)}` : '';
    if (technical) {
      const parts = [dev.principal || ''];
      if (dev.role) parts.push(dev.role);
      if (addedPart) parts.push(addedPart);
      return parts.filter(Boolean).join(' · ');
    }
    const roleLabel =
      (dev.role && roleDescriptions[dev.role]) ||
      (dev.role === 'owner'
        ? 'everything, including changing who may do what'
        : dev.role
          ? describeRolePlain(dev.role)
          : '');
    const accessPart = roleLabel ? capitalize(roleLabel) : 'Allowed in';
    return [accessPart, addedPart].filter(Boolean).join(' · ');
  }

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
                class="btn small primary"
                href=${routeHref({
                  section: 'settings',
                  page: 'connections',
                  sub: 'node',
                  item: n.node,
                })}
              >
                Review
              </a>
            </div>
          </div>
        `;
      })}

      ${disagreements.map((d) => {
        const formatted = formatDisagreementNotice(d, technical);
        if (!formatted) return null;
        return html`
          <div class="notice" key=${`disagree:${formatted.machine}`} role="status">
            <strong>${formatted.title}</strong>
            ${technical && formatted.details.length > 0
              ? formatted.details.map(
                  (line, i) => html`<span class="mono" key=${i}>${line}</span>`,
                )
              : html`<span>Pair with ${formatted.machine} directly to connect to it without relying on another machine.</span>`}
          </div>
        `;
      })}

      ${!(
        hideFirstLabel &&
        !error &&
        !notice &&
        !syncBanner &&
        identityChangedNodes.length === 0 &&
        disagreements.length === 0
      )
        ? html`<p class="label">Paired machines</p>`
        : null}
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
                  ? `${routeLine} · ${n.principal || 'unknown'} · ${n.role || 'unknown'} · ${n.via}`
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
                    ${routeInfo.isIdentityChanged
                      ? html`<span class="btn small">Review</span>`
                      : html`<span class="chev" aria-hidden="true">›</span>`}
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
                    ? `${capitalize(info.label)} · ${info.hint}`
                    : capitalize(info.label);
                const rejLine = technical
                  ? formatLearnRejectionTechnical(entry.learn_rejection)
                  : null;
                return html`
                  <div class="row" key=${`relay:${entry.node}`}>
                    <span class="dot off" aria-hidden="true"></span>
                    <span class="body">
                      <span class="title mono">${entry.node}</span>
                      <span class="sub wrap"><${CodeText} text=${sub} /></span>
                      ${rejLine
                        ? html`<span class="sub wrap mono">${rejLine}</span>`
                        : null}
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
                        <span class="sub">Connect another computer to ${connectionsNode || 'this node'}</span>
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
                            <span class="sub">${formatIncomingSubtitle(dev)}</span>
                          </span>
                          <span class="chev" aria-hidden="true">›</span>
                        </a>
                      `,
                    )}
                  </div>
                `
              : html`<div class="empty">No other devices paired with ${connectionsNode || 'this machine'}.</div>`}
          `
        : null}
    </div>
  `;
}

// ── Appearance page ──────────────────────────────────────────────────────────────────────────

function AppearancePage({ deviceNoun, settings, hideFirstLabel = false, onUpdateSettings }) {
  return html`
    <div>
      ${!hideFirstLabel ? html`<p class="label">Theme</p>` : null}
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

function RefreshingPage({ settings, hideFirstLabel = false, onUpdateSettings }) {
  return html`
    <div>
      ${!hideFirstLabel ? html`<p class="label">Refresh automatically</p>` : null}
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

function AboutPage({
  link,
  nodeInfo,
  technical,
  hideFirstLabel = false,
  updateState,
  onCheckUpdates,
  onInstallUpdate,
  onSetAutoUpdate,
  onDismissUpdateNotice,
}) {
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState(null);
  const hasAppUpdater = Boolean(
    updateState || (link?.kind === 'app' && typeof link?.checkUpdates === 'function'),
  );
  const summary = describeUpdateSummary(hasAppUpdater ? updateState : null, {
    doorKind: hasAppUpdater ? 'app' : link?.kind || 'http',
    nodeName: nodeInfo?.node || '',
  });
  const techRows = technical && hasAppUpdater ? formatUpdateTechnicalRows(updateState) : [];
  const appVersion = updateState?.current?.version || UI_VERSION;

  async function runCheck() {
    if (!onCheckUpdates || busy) return;
    setBusy(true);
    setActionError(null);
    try {
      await onCheckUpdates();
    } catch (err) {
      setActionError(err);
    } finally {
      setBusy(false);
    }
  }

  async function runInstall() {
    if (!onInstallUpdate || busy) return;
    setBusy(true);
    setActionError(null);
    try {
      await onInstallUpdate();
    } catch (err) {
      setActionError(err);
    } finally {
      setBusy(false);
    }
  }

  async function toggleAuto(checked) {
    if (!onSetAutoUpdate) return;
    setActionError(null);
    try {
      await onSetAutoUpdate(checked);
    } catch (err) {
      setActionError(err);
    }
  }

  return html`
    <div>
      ${!hideFirstLabel ? html`<p class="label">Updates</p>` : null}
      ${actionError ? html`<${Notice} error=${actionError} />` : null}
      ${updateState?.rolled_back
        ? html`
            <${Notice}
              title=${`Kept version ${appVersion}`}
              hint=${updateState.rolled_back.reason ||
                `Version ${updateState.rolled_back.version} did not start cleanly, so Legion went back to the previous copy.`}
              kind="warn"
              onDismiss=${onDismissUpdateNotice}
            />
          `
        : null}

      <div class="list">
        <div class="row">
          <span class="body">
            <span class="title">${summary.line}</span>
            ${summary.detail ? html`<span class="sub wrap">${summary.detail}</span>` : null}
          </span>
        </div>

        ${summary.manualCommand
          ? html`
              <div class="row">
                <span class="body">
                  <span class="title mono">${summary.manualCommand}</span>
                  ${summary.manualSha256
                    ? html`<span class="sub mono wrap">SHA-256: ${summary.manualSha256}</span>`
                    : null}
                </span>
                <${CopyButton} text=${summary.manualCommand} />
              </div>
            `
          : null}

        ${summary.showAutoSwitch && hasAppUpdater
          ? html`
              <label class="row switch row-switch">
                <span class="body">
                  <span class="title">Update automatically</span>
                  <span class="sub wrap">
                    Download when a newer version is seen and install on start or when you leave the app
                  </span>
                </span>
                <input
                  type="checkbox"
                  checked=${Boolean(updateState?.auto_update)}
                  onChange=${(e) => toggleAuto(e.target.checked)}
                />
              </label>
            `
          : null}
      </div>

      ${hasAppUpdater && (summary.canAction || summary.showCheck)
        ? html`
            <div class="actions">
              ${summary.canAction && summary.actionLabel
                ? html`
                    <button
                      type="button"
                      class="btn primary"
                      disabled=${busy}
                      onClick=${runInstall}
                    >
                      ${busy ? 'Working…' : summary.actionLabel}
                    </button>
                  `
                : null}
              ${summary.showCheck
                ? html`
                    <button type="button" class="btn" disabled=${busy} onClick=${runCheck}>
                      ${busy ? 'Checking…' : 'Check now'}
                    </button>
                  `
                : null}
            </div>
          `
        : null}

      ${summary.notes
        ? html`
            <p class="label">What is new</p>
            <div class="list">
              <div class="row">
                <span class="body">
                  <span class="sub wrap">${summary.notes}</span>
                </span>
              </div>
            </div>
          `
        : null}

      ${technical && techRows.length > 0
        ? html`
            <p class="label">Update details</p>
            <div class="list">
              <dl class="kv">
                ${techRows.map(
                  (r) => html`
                    <dt key=${`dt-${r.key}`}>${r.key}</dt>
                    <dd key=${`dd-${r.key}`}>${r.value}</dd>
                  `,
                )}
              </dl>
            </div>
          `
        : null}

      <p class="label">Version</p>
      <div class="list">
        <dl class=${technical ? 'kv' : 'kv plain'}>
          <dt>${hasAppUpdater ? (technical ? 'app' : 'App') : technical ? 'interface' : 'Interface'}</dt>
          <dd>${appVersion}</dd>
          <dt>${technical ? 'node' : 'Connected machine'}</dt>
          <dd>
            ${nodeInfo?.version || 'unknown'}
            ${technical && nodeInfo?.build ? ` (${nodeInfo.build})` : ''}
          </dd>
          ${technical
            ? html`
                <dt>protocol</dt>
                <dd>${nodeInfo?.protocol ?? 1}</dd>
              `
            : null}
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
  capsStore = null,
  onRegisterRefresh = null,
  linkState,
  pairingController = null,
  pairingState = null,
  nodeInfo,
  tools,
  allowedSet,
  snapshot,
  settings,
  pairNotice,
  onDismissPairNotice,
  updateState,
  onCheckUpdates,
  onInstallUpdate,
  onSetAutoUpdate,
  onDismissUpdateNotice,
  onUpdateSettings,
  onRequestConfirm,
  onSelectNode,
  onAcceptIdentityChange,
  onDismissIdentityChange,
  onDisconnected,
  onConnected,
}) {
  const technical = Boolean(settings?.technical);
  const whoami = nodeInfo?.whoami;
  const unreachable = nodeInfo?.reachable === false;
  const checkedAt = nodeInfo?.roleCheckedAt || nodeInfo?.role_checked_at || null;
  const plainPerms = describePlainPermissions(whoami, tools);
  const deviceSub = unreachable
    ? `${nodeInfo?.node || 'Machine'} unreachable · ${plainPerms}${checkedAt ? ` (as of ${formatRelativeTime(checkedAt)})` : ' (last known)'}`
    : `${whoami?.principal || nodeInfo?.principal || capitalize(deviceNoun)} · ${plainPerms}`;
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
            <span class="sub">${deviceSub}</span>
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
            <span class="sub">
              ${`Legion ${updateState?.current?.version || UI_VERSION}${
                updateState?.phase === 'ready' || updateState?.phase === 'available' || updateState?.phase === 'manual'
                  ? ' · Update available'
                  : ''
              }`}
            </span>
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
          hideFirstLabel=${wide}
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
          capsStore=${capsStore}
          onRegisterRefresh=${wide ? null : onRegisterRefresh}
          linkState=${linkState}
          pairingController=${pairingController}
          pairingState=${pairingState}
          nodeInfo=${nodeInfo}
          tools=${tools}
          allowedSet=${allowedSet}
          snapshot=${snapshot}
          technical=${technical}
          hideFirstLabel=${wide}
          pairNotice=${pairNotice}
          onDismissPairNotice=${onDismissPairNotice}
          onRequestConfirm=${onRequestConfirm}
          onSelectNode=${onSelectNode}
          onAcceptIdentityChange=${onAcceptIdentityChange}
          onDismissIdentityChange=${onDismissIdentityChange}
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
          hideFirstLabel=${wide}
          onUpdateSettings=${onUpdateSettings}
        />
      `;
    }
    if (page === 'refreshing') {
      return html`
        <${RefreshingPage}
          settings=${settings}
          hideFirstLabel=${wide}
          onUpdateSettings=${onUpdateSettings}
        />
      `;
    }
    if (page === 'about') {
      return html`
        <${AboutPage}
          link=${link}
          nodeInfo=${nodeInfo}
          technical=${technical}
          hideFirstLabel=${wide}
          updateState=${updateState}
          onCheckUpdates=${onCheckUpdates}
          onInstallUpdate=${onInstallUpdate}
          onSetAutoUpdate=${onSetAutoUpdate}
          onDismissUpdateNotice=${onDismissUpdateNotice}
        />
      `;
    }
    return null;
  }

  if (wide) {
    let rightTitle = null;
    let rightFact = null;
    let rightLabel = null;

    if (selectedPage === 'device') {
      rightTitle = `This ${deviceNoun}`;
      rightLabel = 'Identity';
    } else if (selectedPage === 'connections') {
      if (route.sub) {
        const subTitle =
          route.sub === 'pair'
            ? 'Pair a machine'
            : route.sub === 'invite'
              ? 'Let another device in'
              : route.sub === 'add'
                ? 'Add a machine'
                : route.item || 'Connections';
        rightTitle = html`
          <a
            class="btn icon split-back"
            href=${routeHref({ section: 'settings', page: 'connections' })}
            aria-label="Back to Connections"
          >
            <${Icon} name="back" />
          </a>
          <span class="pane-title-text">${subTitle}</span>
        `;
        rightLabel = html`
          <nav class="where" aria-label="Settings location">
            <a href=${routeHref({ section: 'settings', page: 'connections' })}>Connections</a>
            <i aria-hidden="true">›</i>
            <span class="current">${subTitle}</span>
          </nav>
        `;
      } else {
        rightTitle = 'Connections';
        rightFact = `${nodeCount} ${nodeCount === 1 ? 'machine' : 'machines'}`;
        rightLabel = 'Paired machines';
      }
    } else if (selectedPage === 'appearance') {
      rightTitle = 'Appearance';
      rightLabel = 'Theme';
    } else if (selectedPage === 'refreshing') {
      rightTitle = 'Refreshing';
      rightLabel = 'Refresh automatically';
    } else if (selectedPage === 'about') {
      rightTitle = 'About';
      rightFact = `Legion ${updateState?.current?.version || UI_VERSION}`;
      rightLabel = 'Updates';
    }

    return html`
      <${SplitView}
        leftTitle="Settings"
        leftLabel="General"
        leftBody=${indexPane}
        rightTitle=${rightTitle}
        rightFact=${rightFact}
        rightLabel=${rightLabel}
        rightEmpty=${!selectedPage ? 'Choose a setting on the left.' : null}
        rightBody=${selectedPage ? html`<div class="settings-pane">${renderPage(selectedPage)}</div>` : null}
      />
    `;
  }

  if (selectedPage) {
    return renderPage(selectedPage);
  }

  return indexPane;
}
