// Friendly Actions catalog mapping everyday tasks onto real Legion tools and machine screens.

export const ACTION_GROUPS = ['Power', 'Files', 'Run', 'Projects', 'Services', 'Fleet'];

export const ACTION_DEFS = [
  {
    id: 'sleep',
    group: 'Power',
    title: 'Sleep',
    summary: 'Put a machine to sleep',
    tool: 'power.sleep',
    mode: 'tool',
    targetFilter: 'on',
  },
  {
    id: 'wake',
    group: 'Power',
    title: 'Wake',
    summary: 'Switch on one that is off',
    tool: 'power.wake',
    mode: 'tool',
    targetFilter: 'wakeable',
  },
  {
    id: 'restart',
    group: 'Power',
    title: 'Restart',
    summary: 'Also into another system',
    tool: 'power.restart',
    mode: 'tool',
    targetFilter: 'on',
  },
  {
    id: 'shutdown',
    group: 'Power',
    title: 'Shut down',
    summary: 'Switch a machine off',
    tool: 'power.shutdown',
    mode: 'tool',
    targetFilter: 'on',
  },
  {
    id: 'browse',
    group: 'Files',
    title: 'Browse',
    summary: 'Look at files on a machine',
    tool: 'fs.list',
    mode: 'machine-tab',
    machineTab: 'files',
    targetFilter: 'on',
  },
  {
    id: 'copy',
    group: 'Files',
    title: 'Copy',
    summary: 'From one machine to another',
    tool: 'fs.transfer',
    fallbackTools: ['fs.copy'],
    mode: 'tool',
    targetFilter: 'none',
  },
  {
    id: 'command',
    group: 'Run',
    title: 'Command',
    summary: 'Run one and see what it says',
    tool: 'exec.run',
    mode: 'machine-tab',
    machineTab: 'command',
    targetFilter: 'on',
  },
  {
    id: 'update',
    group: 'Run',
    title: 'Update Legion',
    summary: 'Get the latest version',
    tool: 'meta.update',
    mode: 'tool',
    targetFilter: 'on',
  },
  {
    id: 'projects',
    group: 'Projects',
    title: 'Projects',
    summary: 'Pull code and run tasks',
    tool: 'proj.list',
    mode: 'machine-tab',
    machineTab: 'projects',
    targetFilter: 'on',
  },
  {
    id: 'services',
    group: 'Services',
    title: 'Services',
    summary: 'Check, start or restart services',
    tool: 'svc.list',
    mode: 'machine-tab',
    machineTab: 'services',
    targetFilter: 'on',
  },
  {
    id: 'ping',
    group: 'Fleet',
    title: 'Check machines',
    summary: 'See which machines answer',
    tool: 'fleet.ping',
    mode: 'tool',
    targetFilter: 'none',
  },
  {
    id: 'add-machine',
    group: 'Fleet',
    title: 'Add a machine',
    summary: 'Connect another computer to the fleet',
    tool: 'fleet.add',
    mode: 'tool',
    targetFilter: 'none',
  },
];

export function plainDenyReason(toolSpec, whoami) {
  const effect = toolSpec?.effect;
  if (effect === 'admin') return 'Only an owner can do this';
  if (effect === 'power') return 'Power controls are not allowed on this device';
  if (effect === 'destroy') return 'Removing files is not allowed on this device';
  if (effect === 'exec') return 'Running commands is not allowed on this device';
  if (effect === 'write') return 'Making changes is not allowed on this device';
  const role = whoami?.role;
  return role ? `Not allowed for ${role}` : 'Not allowed on this device';
}

/**
 * Resolves the friendly action list against the node's live tool catalog and caller permissions.
 */
export function buildActionGroups({ tools = [], allowedSet = null, whoami = null, query = '', technical = false } = {}) {
  const specMap = new Map();
  for (const t of tools || []) {
    if (t && t.name) specMap.set(t.name, t);
  }

  const needle = String(query || '').trim().toLowerCase();
  const byGroup = new Map();
  for (const g of ACTION_GROUPS) byGroup.set(g, []);

  for (const def of ACTION_DEFS) {
    let resolvedTool = def.tool;
    let spec = specMap.get(resolvedTool) || null;
    if (!spec && Array.isArray(def.fallbackTools)) {
      for (const alt of def.fallbackTools) {
        if (specMap.has(alt)) {
          resolvedTool = alt;
          spec = specMap.get(alt);
          break;
        }
      }
    }
    // Hide actions whose underlying tool does not exist on this node at all (when catalog loaded).
    if (specMap.size > 0 && !spec) continue;

    const allowed = allowedSet ? allowedSet.has(resolvedTool) : true;
    const reason = allowed ? null : plainDenyReason(spec, whoami);

    if (needle) {
      const hay = [
        def.title,
        def.summary,
        def.group,
        technical ? resolvedTool : '',
      ]
        .join(' ')
        .toLowerCase();
      if (!hay.includes(needle)) continue;
    }

    const bucket = byGroup.get(def.group);
    if (bucket) {
      bucket.push({
        ...def,
        tool: resolvedTool,
        spec,
        allowed,
        reason,
      });
    }
  }

  const out = [];
  for (const group of ACTION_GROUPS) {
    const items = byGroup.get(group) || [];
    if (items.length > 0) out.push({ group, items });
  }
  return out;
}

export function findActionById(id, tools = [], allowedSet = null, whoami = null) {
  const groups = buildActionGroups({ tools, allowedSet, whoami });
  for (const g of groups) {
    for (const item of g.items) {
      if (item.id === id) return item;
    }
  }
  return null;
}
