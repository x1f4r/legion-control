import { formatBusyReasonLines } from '../lib/format.js';
import { createMockFilesState, FILES_EXTRA_TOOLS, OLD_FS_FIND_TOOL } from './mock-files.js';
import {
  OLD_FIXTURE_BUILD,
  OLD_FIXTURE_TOOLS,
  OLD_FIXTURE_VERSION,
  validateToolArgsAgainstSpec,
} from './mock-old-tools.js';

// Deterministic mock link (?mock) shaped like real Legion tool results.
// Mode flags (comma-separated, e.g. ?mock=multi,tech):
//   token     - unauthenticated HTTP token start screen
//   pair      - unauthenticated app pairing start screen
//   repair    - damaged state file recovery screen
//   limited   - observer role (no exec, write, power, destroy, admin)
//   multi     - native app mode with two paired nodes (tower, pi)
//   fleet     - native app mode with learned routes, unsynced peer, relay peer, identity change, and fleet device.invite schema
//   enrol     - with fleet, auto-completes the next created invite so post-pairing delivery appears
//   first     - adds a never-seen machine ("mini") that starts in asking state
//   tech      - starts with Technical details turned on
//   old       - configures every mock machine as an older node (exact build 05ba1ef2e385 fixture, strict args)
//   mixed     - mixed-version fleet: tower & pi run current Legion while studio runs build 05ba1ef2e385 (strict args)
//   nosync    - hides sync.* tools so the Kept in sync section is omitted
//   large     - adds a 5 000-entry folder (/home/me/Archive5000) for paging tests
//   transfers - seeds live running machine & device transfers for Activity preview

const isoAgo = (sec) => new Date(Date.now() - sec * 1000).toISOString();

const MOCK_TOOLS = [
  {
    name: 'audit.tail',
    effect: 'read',
    description: 'Show the last calls this node recorded in its audit log, oldest first.',
    input_schema: {
      type: 'object',
      properties: {
        limit: { type: 'integer', default: 20, minimum: 1, maximum: 1000, description: 'How many entries to return.' },
        principal: { type: 'string', description: 'Only calls that acted as this principal.' },
        tool: { type: 'string', description: 'Only calls matching this tool glob.' },
      },
    },
  },
  {
    name: 'config.list',
    effect: 'admin',
    description: 'List the shared configuration documents present on this node.',
    input_schema: { type: 'object', properties: {} },
  },
  {
    name: 'device.invite',
    effect: 'admin',
    description: 'Create a one-time pairing invite so another device can connect to this machine.',
    input_schema: {
      type: 'object',
      required: ['principal'],
      properties: {
        principal: { type: 'string', description: 'Name in policy.toml for the new device (for example phone or tablet).' },
        valid_minutes: { type: 'integer', default: 15, minimum: 1, maximum: 1440, description: 'Minutes before the invite expires.' },
      },
    },
  },
  {
    name: 'device.list',
    effect: 'admin',
    description: 'List paired devices and pending invites accepted by this machine.',
    input_schema: { type: 'object', properties: {} },
  },
  {
    name: 'device.remove',
    effect: 'admin',
    description: 'Revoke a paired device or cancel a pending invite on this machine.',
    input_schema: {
      type: 'object',
      properties: {
        name: { type: 'string', description: 'Paired device name to remove.' },
        id: { type: 'string', description: 'Pending invite id to cancel.' },
      },
    },
  },
  {
    name: 'exec.run',
    effect: 'exec',
    description: 'Run a command that finishes and return its exit code and output.',
    input_schema: {
      type: 'object',
      properties: {
        cmd: { type: 'string', description: 'A command line handed whole to the platform shell.' },
        argv: { type: 'array', items: { type: 'string' }, description: 'Program and arguments with no shell.' },
        cwd: { type: 'string', description: 'Directory to run in.' },
        timeout_s: { type: 'number', description: 'Seconds before the command is stopped (default 120).' },
      },
    },
  },
  {
    name: 'fleet.add',
    effect: 'admin',
    description: 'Add a machine to fleet.toml on this node.',
    input_schema: {
      type: 'object',
      required: ['name', 'host'],
      properties: {
        name: { type: 'string', description: 'Short name for the machine (for example studio).' },
        host: { type: 'string', description: 'Hostname or tailnet address to connect to.' },
        user: { type: 'string', description: 'SSH user name when different from this machine.' },
        system: { type: 'string', default: 'linux', description: 'System label (linux, macos, or windows).' },
        mac: { type: 'string', description: 'Optional MAC address for Wake-on-LAN.' },
      },
    },
  },
  {
    name: 'fleet.list',
    effect: 'read',
    description: 'List the machines configured in fleet.toml, their systems and transport targets, without making network connections.',
    input_schema: { type: 'object', properties: {} },
  },
  {
    name: 'fleet.ping',
    effect: 'read',
    description: 'Check whether machines in fleet.toml answer hello, concurrently, reporting their node name, system, version, platform and latency.',
    input_schema: {
      type: 'object',
      properties: {
        machines: { type: 'array', items: { type: 'string' }, description: 'Machine names to probe; every machine in fleet.toml when absent.' },
        timeout_s: { type: 'number', description: 'Seconds to wait per machine, up to 60; 8 when absent.' },
      },
    },
  },
  {
    name: 'fs.list',
    effect: 'read',
    description: 'List entries in a directory up to a bounded depth and count, directories first then by name.',
    input_schema: {
      type: 'object',
      required: ['path'],
      properties: {
        path: { type: 'string', description: 'Directory to list (absolute or starting with ~).' },
        depth: { type: 'integer', default: 1, minimum: 1, maximum: 8, description: 'How many directory levels to descend.' },
        hidden: { type: 'boolean', default: true, description: 'Include hidden files and directories.' },
        limit: { type: 'integer', default: 500, minimum: 1, maximum: 5000, description: 'Maximum entries to return.' },
      },
    },
  },
  {
    name: 'fs.read',
    effect: 'read',
    description: 'Read up to 1 MiB of a file from an offset as UTF-8 or base64.',
    input_schema: {
      type: 'object',
      required: ['path'],
      properties: {
        path: { type: 'string', description: 'File to read (absolute or starting with ~).' },
        offset: { type: 'integer', default: 0, minimum: 0, description: 'Byte offset to start reading from.' },
        length: { type: 'integer', default: 1048576, minimum: 0, maximum: 1048576, description: 'Maximum bytes to read.' },
        encoding: { type: 'string', enum: ['utf8', 'base64'], description: 'Output encoding.' },
      },
    },
  },
  {
    name: 'fs.remove',
    effect: 'destroy',
    description: 'Remove a file, symbolic link or directory (with optional recursive and dry_run modes).',
    input_schema: {
      type: 'object',
      required: ['path'],
      properties: {
        path: { type: 'string', description: 'Path to remove.' },
        recursive: { type: 'boolean', default: false, description: 'Remove a directory and everything inside it.' },
        dry_run: { type: 'boolean', default: false, description: 'Report what would be removed without changing the file system.' },
      },
    },
  },
  {
    name: 'fs.transfer',
    effect: 'write',
    description: 'Copy a file or folder from one machine in the fleet to another.',
    input_schema: {
      type: 'object',
      required: ['from', 'to'],
      properties: {
        from: { type: 'string', description: 'Source machine and path (for example tower:/home/me/notes.txt).' },
        to: { type: 'string', description: 'Destination machine and path (for example studio:/home/me/notes.txt).' },
        dry_run: { type: 'boolean', default: false, description: 'Preview what would be copied without changing anything.' },
      },
    },
  },
  {
    name: 'job.cancel',
    effect: 'exec',
    description: 'Stop a running background job and its entire process tree.',
    input_schema: {
      type: 'object',
      required: ['id'],
      properties: {
        id: { type: 'string', description: 'The job identifier.' },
        force: { type: 'boolean', default: false, description: 'Terminate immediately.' },
      },
    },
  },
  {
    name: 'job.list',
    effect: 'read',
    description: 'List background jobs recorded on this node, newest first.',
    input_schema: {
      type: 'object',
      properties: {
        state: {
          type: 'string',
          enum: ['starting', 'running', 'succeeded', 'failed', 'cancelled', 'interrupted'],
          description: 'Keep only jobs currently in this derived state.',
        },
        limit: { type: 'integer', minimum: 1, maximum: 500, description: 'Maximum jobs to return.' },
      },
    },
  },
  {
    name: 'job.logs',
    effect: 'read',
    description: 'Read captured stdout and stderr from a background job.',
    input_schema: {
      type: 'object',
      required: ['id'],
      properties: {
        id: { type: 'string', description: 'The job identifier.' },
        offset: { type: 'integer', minimum: 0, description: 'Byte offset to read forward from.' },
        tail: { type: 'integer', minimum: 1, maximum: 5000, description: 'Trailing lines when offset is omitted.' },
      },
    },
  },
  {
    name: 'job.start',
    effect: 'exec',
    description: 'Start a command as a detached background job that outlives this call.',
    input_schema: {
      type: 'object',
      properties: {
        cmd: { type: 'string', description: 'Command line handed to the platform shell.' },
        name: { type: 'string', description: 'Short human-readable name for the job.' },
        exclusive: { type: 'boolean', default: false, description: 'Mark this machine busy while running.' },
      },
    },
  },
  {
    name: 'job.status',
    effect: 'read',
    description: 'Inspect one background job record, derived state, exit code and duration.',
    input_schema: {
      type: 'object',
      required: ['id'],
      properties: { id: { type: 'string', description: 'The job identifier.' } },
    },
  },
  {
    name: 'meta.update',
    effect: 'admin',
    description: 'Check for a newer Legion release or update this node.',
    input_schema: {
      type: 'object',
      properties: {
        check: { type: 'boolean', default: true, description: 'Only check whether a newer build is available.' },
      },
    },
  },
  {
    name: 'meta.version',
    effect: 'read',
    description: 'Say which build of legion this node runs, the protocol it speaks, and its operating system and architecture.',
    input_schema: { type: 'object', properties: {} },
  },
  {
    name: 'meta.whoami',
    effect: 'read',
    description: 'Say who the caller is on this node: the principal and role it acts as, the door it came through, and the tools it may use.',
    input_schema: { type: 'object', properties: {} },
  },
  {
    name: 'power.cancel',
    effect: 'power',
    description: 'Call off the sleep, restart or shutdown handed over on this machine and not yet carried out.',
    input_schema: { type: 'object', properties: {} },
  },
  {
    name: 'power.restart',
    effect: 'power',
    description: 'Restart this machine, after checking that nothing important is running on it.',
    input_schema: {
      type: 'object',
      properties: {
        into: { type: 'string', description: 'A boot target from node.toml ([boot.<name>]) to start once, such as windows.' },
        delay_s: { type: 'integer', minimum: 1, maximum: 600, description: 'Seconds to wait first. It can be cancelled until then.' },
        force: { type: 'boolean', default: false, description: 'Go ahead even while the machine is busy or unmonitored.' },
        dry_run: { type: 'boolean', default: false, description: 'Return the exact commands this call would run, without running them.' },
      },
    },
  },
  {
    name: 'power.shutdown',
    effect: 'power',
    description: 'Power this machine off after a short delay.',
    input_schema: {
      type: 'object',
      properties: {
        delay_s: { type: 'integer', minimum: 1, maximum: 600, description: 'Seconds before powering off.' },
        force: { type: 'boolean', default: false, description: 'Go ahead even while busy.' },
        dry_run: { type: 'boolean', default: false, description: 'Return the exact commands without running them.' },
      },
    },
  },
  {
    name: 'power.sleep',
    effect: 'power',
    description: 'Put this machine to sleep about 3 seconds after this call replies.',
    input_schema: {
      type: 'object',
      properties: {
        force: { type: 'boolean', default: false, description: 'Go ahead even while busy.' },
        dry_run: { type: 'boolean', default: false, description: 'Return the exact commands without running them.' },
      },
    },
  },
  {
    name: 'power.status',
    effect: 'read',
    description: 'Report this machine power state without changing anything: pending action, busy state, system, and boot targets.',
    input_schema: { type: 'object', properties: {} },
  },
  {
    name: 'power.wake',
    effect: 'power',
    description: 'Wake a machine from fleet.toml using Wake-on-LAN.',
    input_schema: {
      type: 'object',
      required: ['machine'],
      properties: {
        machine: { type: 'string', description: 'The machine to wake, as fleet.toml names it.' },
        wait_s: { type: 'integer', default: 0, minimum: 0, maximum: 300, description: 'Seconds to wait for the machine to answer.' },
        dry_run: { type: 'boolean', default: false, description: 'Report where packets would go without sending any.' },
      },
    },
  },
  {
    name: 'proj.list',
    effect: 'read',
    description: 'List the projects recorded in projects.toml on this machine.',
    input_schema: { type: 'object', properties: {} },
  },
  {
    name: 'proj.pull',
    effect: 'exec',
    description: 'Update a project on this machine from its upstream with git pull --ff-only.',
    input_schema: {
      type: 'object',
      required: ['project'],
      properties: { project: { type: 'string', description: 'The project to update.' } },
    },
  },
  {
    name: 'proj.run',
    effect: 'exec',
    description: 'Run one of a project tasks in its directory on this machine.',
    input_schema: {
      type: 'object',
      required: ['project', 'task'],
      properties: {
        project: { type: 'string', description: 'Project name.' },
        task: { type: 'string', description: 'Task name.' },
        args: { type: 'array', items: { type: 'string' }, description: 'Extra arguments.' },
      },
    },
  },
  {
    name: 'proj.status',
    effect: 'read',
    description: 'Show the git state of projects on this machine.',
    input_schema: {
      type: 'object',
      properties: { project: { type: 'string', description: 'Project name, or all when absent.' } },
    },
  },
  {
    name: 'svc.list',
    effect: 'read',
    description: 'List background services managed by systemd, launchd or Windows SCM.',
    input_schema: {
      type: 'object',
      properties: {
        scope: { type: 'string', enum: ['user', 'system', 'all'], description: 'Service scope.' },
        filter: { type: 'string', description: 'Substring filter.' },
        state: { type: 'string', enum: ['running', 'stopped', 'failed'], description: 'State filter.' },
      },
    },
  },
  {
    name: 'svc.logs',
    effect: 'read',
    description: 'Read recent log lines for a background service.',
    input_schema: {
      type: 'object',
      required: ['name'],
      properties: {
        name: { type: 'string', description: 'Service name.' },
        scope: { type: 'string', enum: ['user', 'system'], description: 'Service scope.' },
        lines: { type: 'integer', default: 100, minimum: 1, maximum: 2000, description: 'Maximum lines.' },
      },
    },
  },
  {
    name: 'svc.restart',
    effect: 'exec',
    description: 'Restart a background service and confirm it is running.',
    input_schema: {
      type: 'object',
      required: ['name'],
      properties: {
        name: { type: 'string', description: 'Service name.' },
        scope: { type: 'string', enum: ['user', 'system'], description: 'Service scope.' },
      },
    },
  },
  {
    name: 'svc.start',
    effect: 'exec',
    description: 'Start a background service and confirm it is running.',
    input_schema: {
      type: 'object',
      required: ['name'],
      properties: {
        name: { type: 'string', description: 'Service name.' },
        scope: { type: 'string', enum: ['user', 'system'], description: 'Service scope.' },
      },
    },
  },
  {
    name: 'svc.status',
    effect: 'read',
    description: 'Read the current state, enabled flag, PID and start time of one service.',
    input_schema: {
      type: 'object',
      required: ['name'],
      properties: {
        name: { type: 'string', description: 'Service name.' },
        scope: { type: 'string', enum: ['user', 'system'], description: 'Service scope.' },
      },
    },
  },
  {
    name: 'svc.stop',
    effect: 'exec',
    description: 'Stop a background service and confirm it is stopped.',
    input_schema: {
      type: 'object',
      required: ['name'],
      properties: {
        name: { type: 'string', description: 'Service name.' },
        scope: { type: 'string', enum: ['user', 'system'], description: 'Service scope.' },
      },
    },
  },
  {
    name: 'sys.info',
    effect: 'read',
    description: 'Describe this machine: host name, operating system, uptime, CPU, memory, disks, and GPUs.',
    input_schema: { type: 'object', properties: {} },
  },
  {
    name: 'sys.kill',
    effect: 'destroy',
    description: 'Stop a process: ask it to exit, and force it after a short grace if it has not.',
    input_schema: {
      type: 'object',
      required: ['pid'],
      properties: {
        pid: { type: 'integer', minimum: 0, description: 'Process id.' },
        tree: { type: 'boolean', default: false, description: 'Stop descendants too.' },
        force: { type: 'boolean', default: false, description: 'Stop immediately.' },
      },
    },
  },
  {
    name: 'token.add',
    effect: 'admin',
    description: 'Make a bearer token for legion serve, standing for one principal in policy.toml.',
    input_schema: {
      type: 'object',
      required: ['name', 'principal'],
      properties: {
        name: { type: 'string', description: 'Token name.' },
        principal: { type: 'string', description: 'Principal name.' },
        expires_days: { type: 'integer', minimum: 0, description: 'Days until expiry.' },
      },
    },
  },
  {
    name: 'update.list',
    effect: 'read',
    description: 'List the signed update sets stored on this node (newest first).',
    input_schema: {
      type: 'object',
      properties: {
        channel: { type: 'string', enum: ['stable', 'preview'], description: 'Filter by channel.' },
      },
    },
  },
  {
    name: 'update.publish',
    effect: 'admin',
    description: 'Verify a signed update set, store it, and notify reachable fleet machines.',
    input_schema: {
      type: 'object',
      properties: {
        from: { type: 'string', description: 'Local directory holding the signed update set.' },
        from_peer: { type: 'string', description: 'Peer machine to pull a set from.' },
        channel: { type: 'string', enum: ['stable', 'preview'], description: 'Channel when using from_peer.' },
        sequence: { type: 'integer', minimum: 1, description: 'Sequence when using from_peer.' },
      },
    },
  },
  {
    name: 'update.read',
    effect: 'read',
    description: 'Read up to 1 MiB of one listed file from a stored update set as base64.',
    input_schema: {
      type: 'object',
      required: ['channel', 'sequence', 'name'],
      properties: {
        channel: { type: 'string', enum: ['stable', 'preview'] },
        sequence: { type: 'integer', minimum: 1 },
        name: { type: 'string' },
        offset: { type: 'integer', minimum: 0 },
        length: { type: 'integer', minimum: 0, maximum: 1048576 },
      },
    },
  },
];

export function createMockLink(mode = '') {
  const flags = new Set(
    String(mode || '')
      .split(/[,\s+]+/)
      .map((s) => s.trim().toLowerCase())
      .filter(Boolean),
  );
  const isLimited = flags.has('limited');
  const isNone = flags.has('none');
  const isEmpty = flags.has('empty');
  const isMulti = flags.has('multi');
  const isFleet = flags.has('fleet');
  const isNoRoles = flags.has('noroles');
  const isDenyRead = flags.has('denyread');
  const isConflict = flags.has('conflict');
  const isPairedChange = flags.has('pairedchg');
  const isPartial = flags.has('partial');
  const isPrimaryOffline = flags.has('aoff');
  const isDisagree = flags.has('disagree');
  const isAutoEnrol = flags.has('enrol');
  const isFirst = flags.has('first');
  const isRepair = flags.has('repair');
  const isPair = flags.has('pair');
  const isToken = flags.has('token');
  const isUpdate = flags.has('update');
  const isUpdated = flags.has('updated');
  const isUpdateDeb = flags.has('update-deb');
  const isUpdateFailed = flags.has('update-failed');
  const isUpdateMetered = flags.has('update-metered');
  const isUpdateSeq0 = flags.has('update-seq0');

  let authed = !isToken && !isPair && !isRepair;
  let repaired = false;
  const kind =
    isPair ||
    isMulti ||
    isRepair ||
    isFleet ||
    isUpdate ||
    isUpdated ||
    isUpdateDeb ||
    isUpdateFailed ||
    isUpdateMetered ||
    isUpdateSeq0
      ? 'app'
      : 'mock';
  let jobCounter = 10;
  let inviteCounter = 1;
  let activeNode = 'tower';
  let pairedNodes = isFleet ? ['tower', 'pi', 'studio', 'mini'] : isMulti ? ['tower', 'pi'] : ['tower'];
  let miniIdentityChanged = isFleet;
  let towerIdentityChanged = isFleet && isPairedChange;
  let lastPaired = null;
  let deviceListPollCount = 0;
  let fleetDevicesByMachine = null;

  const isOldNode = flags.has('old') || flags.has('legacy');
  const isMixed = !isOldNode && (flags.has('mixed') || flags.has('old-peer'));
  const isNoSync = flags.has('nosync') || isOldNode;
  const isMachineOld = (machineName) => isOldNode || (isMixed && machineName === 'studio');

  const baseTools = MOCK_TOOLS.map((t) => {
    if (!isOldNode && t.name === 'fs.list') {
      return {
        ...t,
        input_schema: {
          ...t.input_schema,
          properties: {
            ...t.input_schema.properties,
            sort: { type: 'string', enum: ['name', 'modified', 'size'] },
            order: { type: 'string', enum: ['asc', 'desc'] },
            categories: { type: 'array', items: { type: 'string' } },
            offset: { type: 'integer', default: 0, minimum: 0 },
          },
        },
      };
    }
    if (!isOldNode && t.name === 'fs.transfer') {
      return {
        ...t,
        input_schema: {
          ...t.input_schema,
          properties: {
            ...t.input_schema.properties,
            if_exists: { type: 'string', enum: ['fail', 'skip', 'replace', 'rename'] },
            move: { type: 'boolean', default: false },
            background: { type: 'boolean', default: false },
          },
        },
      };
    }
    return t;
  });

  const extraFileTools = isOldNode
    ? [
        OLD_FS_FIND_TOOL,
        ...FILES_EXTRA_TOOLS.filter((t) => ['fs.mkdir', 'fs.move', 'fs.stat', 'fs.hash'].includes(t.name)),
      ]
    : FILES_EXTRA_TOOLS.filter((t) => !(isNoSync && t.name.startsWith('sync.')));

  const fullBaseTools = [...baseTools, ...extraFileTools];

  const mockToolsList = isFleet
    ? [
        ...fullBaseTools.map((t) =>
          t.name === 'device.invite'
            ? {
                ...t,
                input_schema: {
                  type: 'object',
                  properties: {
                    principal: { type: 'string', description: 'Existing principal in policy.toml.' },
                    name: { type: 'string', description: 'Display name for the new device.' },
                    role: { type: 'string', description: 'Role from policy.toml or owner.' },
                    allow_owner: { type: 'boolean', description: 'Confirm granting owner-strength access.' },
                    valid_minutes: { type: 'integer', default: 15, minimum: 1, maximum: 1440 },
                  },
                },
              }
            : t,
        ),
        {
          name: 'config.read',
          effect: 'admin',
          description: 'Read a shared configuration document on this node.',
          input_schema: {
            type: 'object',
            required: ['document'],
            properties: { document: { type: 'string' } },
          },
        },
        {
          name: 'device.endpoint',
          effect: 'read',
          description: 'Return this node SSH endpoint and host keys for direct pairing.',
          input_schema: { type: 'object', properties: {} },
        },
        ...(!isNoRoles
          ? [
              {
                name: 'device.roles',
                effect: 'read',
                description: 'List the roles this caller may hand to a new device on this machine.',
                input_schema: { type: 'object', properties: {} },
              },
            ]
          : []),
      ]
    : fullBaseTools;

  const role = isNone ? 'guest' : isLimited ? 'observer' : 'owner';
  const toolsForMachine = (machineName) =>
    isMachineOld(machineName) ? OLD_FIXTURE_TOOLS : mockToolsList;
  const allowedToolsForMachine = (machineName) => {
    const list = toolsForMachine(machineName);
    if (isNone) {
      return ['meta.whoami', 'meta.version', 'fleet.list', 'sys.info', 'power.status'];
    }
    if (isLimited) {
      return list.filter((t) => t.effect === 'read').map((t) => t.name);
    }
    return list.map((t) => t.name);
  };
  const allowedTools = allowedToolsForMachine(activeNode);

  let incomingDevices = [
    {
      name: 'Pixel 10',
      principal: 'device:pixel-10',
      role: 'owner',
      fingerprint: 'SHA256:4kP9vR2mN8xQ1wL6tY3bZ5cH7jD0sF9eA2uK4mV8nX0',
      added: isoAgo(86400 * 12),
      foreign: false,
      missing_home: false,
    },
    {
      name: 'Work laptop',
      principal: 'device:work-laptop',
      role: 'observer',
      fingerprint: 'SHA256:9mB2nC4vX6zL8kJ0hG2fD4sA6pO8iU0yT2rE4wQ6aS8',
      added: isoAgo(86400 * 3),
      foreign: false,
      missing_home: false,
    },
  ];
  let pendingInvites = [];

  const machines = {
    tower: {
      reachable: !isPrimaryOffline,
      error: isPrimaryOffline
        ? { code: 'unreachable', message: 'tower did not answer.', hint: null, machine: 'tower' }
        : null,
      sys: {
        node: 'tower',
        hostname: 'tower',
        os: { family: 'linux', name: 'Linux', version: '7.2', kernel: '7.2.0', arch: 'x86_64' },
        uptime_s: 11 * 3600 + 26 * 60,
        boot_id: '7b51c60a-0d29-42b0-9c52-88b6d5a10001',
        cpu: { model: 'Ryzen 9 9950X3D', cores: 32, usage_pct: 71.0, load: [3.2, 2.9, 2.4] },
        memory: { total: 60e9, used: 29e9, available: 31e9, swap_total: 32e9, swap_used: 0 },
        disks: [{ mount: '/', fs: 'btrfs', total: 2.8e12, available: 2.553e12 }],
        gpus: [{ name: 'RTX 5080', memory_total: 16e9, memory_used: 11e9, utilization_pct: 69.0 }],
      },
      jobs: [
        {
          id: 'j-backup-1',
          name: 'Photo backup',
          state: 'running',
          created: isoAgo(240),
          command: 'rsync -av /home/me/photos/ nas:/volume1/photos/',
          duration_s: 240,
          logs: 'sending incremental file list\n2026/autumn/IMG_4012.JPG\n2026/autumn/IMG_4013.JPG\n',
        },
        {
          id: 'j-build-2',
          name: 'Build',
          state: 'running',
          created: isoAgo(720),
          command: 'cargo build --release --workspace',
          duration_s: 720,
          logs: '   Compiling legion-core v0.1.0\n   Compiling legion-doors v0.1.0\n   Compiling legion-cli v0.1.0\n',
        },
        {
          id: 'j-cmd-3',
          name: 'df',
          state: 'succeeded',
          created: isoAgo(2100),
          finished: isoAgo(2097),
          command: 'df -h /',
          exit_code: 0,
          duration_s: 3.0,
          logs: 'Filesystem      Size  Used Avail Use% Mounted on\n/dev/nvme0n1p5  2.8T  247G  2.5T   9% /\n',
        },
        {
          id: 'j-tests-4',
          name: 'proj:website:tests',
          state: 'failed',
          created: isoAgo(7200),
          finished: isoAgo(7080),
          command: 'npm test -- --runInBand',
          exit_code: 1,
          duration_s: 120,
          logs: 'FAIL src/nav.test.js\n  expected 3 tabs to match snapshot\n',
        },
        {
          id: 'j-cancel-5',
          name: 'sleep',
          state: 'cancelled',
          created: isoAgo(14400),
          finished: isoAgo(14340),
          command: 'sleep 600',
          duration_s: 60,
          logs: '',
        },
        {
          id: 'j-restart-6',
          name: 'power.restart',
          state: 'succeeded',
          created: isoAgo(86400 + 3600),
          finished: isoAgo(86400 + 3595),
          command: 'power.restart',
          exit_code: 0,
          duration_s: 5,
          logs: 'Restarted cleanly.\n',
        },
      ],
      services: [
        { name: 'legion-serve.service', scope: 'user', state: 'running', enabled: true, description: 'Legion HTTP door' },
        { name: 'sshd.service', scope: 'system', state: 'running', enabled: true, description: 'OpenSSH daemon' },
        { name: 'tailscaled.service', scope: 'system', state: 'running', enabled: true, description: 'Tailscale node agent' },
        { name: 'backup-nightly.service', scope: 'user', state: 'stopped', enabled: true, description: 'Nightly restic backup' },
      ],
      projects: [
        {
          name: 'legion',
          description: 'Fleet control node and interface.',
          repo: 'https://git.example.net/me/legion.git',
          path: '/home/me/projects/legion',
          present: true,
          elsewhere: ['studio'],
          tasks: ['check', 'test', 'release'],
          git: { branch: 'main', commit: 'b14ebff', ahead: 0, behind: 0, staged: 0, changed: 1, untracked: 0, conflicts: 0 },
        },
        {
          name: 'notes',
          description: 'Personal lab notebook.',
          repo: 'https://git.example.net/me/notes.git',
          path: '/home/me/projects/notes',
          present: true,
          elsewhere: ['studio', 'pi'],
          tasks: ['sync'],
          git: { branch: 'main', commit: '3a9f10c', ahead: 2, behind: 0, staged: 0, changed: 0, untracked: 0, conflicts: 0 },
        },
      ],
      power: {
        system: 'linux',
        boot_id: '7b51c60a-0d29-42b0-9c52-88b6d5a10001',
        busy: {
          state: 'busy',
          reasons: [
            {
              source: 'job',
              name: 'Build',
              detail: 'exclusive job j-build-2',
            },
          ],
        },
        targets: [{ name: 'windows', how: 'efibootmgr --bootnext' }],
        sudo: true,
        pending: null,
      },
    },
    pi: {
      reachable: true,
      sys: {
        node: 'pi',
        hostname: 'pi',
        os: { family: 'linux', name: 'Debian', version: '13', kernel: '6.12.0', arch: 'aarch64' },
        uptime_s: 41 * 86400 + 3600,
        boot_id: 'a011c60a-0d29-42b0-9c52-88b6d5a10002',
        cpu: { model: 'Cortex-A76', cores: 4, usage_pct: 6.0, load: [0.18, 0.22, 0.2] },
        memory: { total: 8e9, used: 2.5e9, available: 5.5e9, swap_total: 2e9, swap_used: 0 },
        disks: [{ mount: '/', fs: 'ext4', total: 128e9, available: 71.7e9 }],
        gpus: [],
      },
      jobs: [],
      services: [
        { name: 'tailscaled.service', scope: 'system', state: 'running', enabled: true, description: 'Tailscale node agent' },
        { name: 'sshd.service', scope: 'system', state: 'running', enabled: true, description: 'OpenSSH daemon' },
      ],
      projects: [],
      power: {
        system: 'linux',
        boot_id: 'a011c60a-0d29-42b0-9c52-88b6d5a10002',
        busy: { state: 'idle', reasons: [] },
        targets: [],
        sudo: true,
        pending: null,
      },
    },
    studio: {
      reachable: true,
      sys: {
        node: 'studio',
        hostname: 'studio',
        os: { family: 'macos', name: 'macOS', version: '15.4', kernel: '24.4.0', arch: 'aarch64' },
        uptime_s: 3 * 86400 + 1800,
        boot_id: 'c200c60a-0d29-42b0-9c52-88b6d5a10003',
        cpu: { model: 'Apple M4 Max', cores: 16, usage_pct: 12.0, load: [1.4, 1.2, 1.1] },
        memory: { total: 128e9, used: 99.8e9, available: 28.2e9, swap_total: 0, swap_used: 0 },
        disks: [{ mount: '/', fs: 'apfs', total: 2e12, available: 140e9 }],
        gpus: [],
      },
      jobs: [],
      services: [
        { name: 'net.example.legion', scope: 'user', state: 'running', enabled: true, description: 'Legion HTTP door' },
      ],
      projects: [
        {
          name: 'legion',
          description: 'Fleet control node and interface.',
          repo: 'https://git.example.net/me/legion.git',
          path: '/home/me/projects/legion',
          present: true,
          elsewhere: ['tower'],
          tasks: ['check', 'test'],
          git: { branch: 'main', commit: 'b14ebff', ahead: 0, behind: 0, staged: 0, changed: 0, untracked: 0, conflicts: 0 },
        },
      ],
      power: {
        system: 'macos',
        boot_id: 'c200c60a-0d29-42b0-9c52-88b6d5a10003',
        busy: {
          state: 'busy',
          reasons: [
            { source: 'probe', name: 'agents', detail: 'busy: 1239887 worker' },
          ],
        },
        targets: [],
        sudo: false,
        pending: null,
      },
    },
    laptop: {
      reachable: false,
      lastSeenAt: Date.now() - 14 * 86400 * 1000,
      error: {
        code: 'unreachable',
        message: "laptop did not answer.",
        hint: 'Check that laptop is powered on and reachable on your tailnet, or wake it via pi.',
        machine: 'laptop',
      },
    },
    nas: {
      reachable: false,
      lastSeenAt: null,
      error: {
        code: 'unreachable',
        message: 'nas did not answer.',
        hint: 'Check that nas is powered on and reachable on your tailnet.',
        machine: 'nas',
      },
    },
  };

  if (isFirst) {
    machines.mini = {
      reachable: false,
      neverSeen: true,
      error: {
        code: 'unreachable',
        message: 'mini did not answer.',
        hint: 'Check that mini is powered on and reachable on your tailnet.',
        machine: 'mini',
      },
    };
  }

  const filesState = createMockFilesState({
    machines,
    flags,
    isMachineOld,
    activeNodeRef: () => activeNode,
    isAuthedRef: () => authed,
    canReadRef: (machineName) =>
      allowedToolsForMachine(machineName || activeNode).includes('fs.read') && !isDenyRead,
  });

  function resolveOne(target, tool, args, onEvent) {
    const name = target || activeNode;
    const m = machines[name];
    if (!m) {
      throw { code: 'not_found', message: `There is no machine called ${name} in fleet.toml.`, hint: null, machine: activeNode };
    }
    if (!m.reachable) {
      throw m.error;
    }

    const machineTools = toolsForMachine(name);
    const machineAllowed = allowedToolsForMachine(name);
    const spec = machineTools.find((t) => t.name === tool);
    if (isMachineOld(name)) {
      if (!spec) {
        throw {
          code: 'unknown_tool',
          message: `Unknown tool: ${tool}`,
          hint: '`legion tools` lists the tools this node offers.',
          machine: name,
        };
      }
      if (!machineAllowed.includes(tool)) {
        throw {
          code: 'denied',
          message: `Principal "phone" (role "${role}") may not call ${tool}.`,
          hint: 'Ask the machine owner to grant access in policy.toml.',
          machine: name,
        };
      }
      validateToolArgsAgainstSpec(name, spec, args);
    } else if (spec && !machineAllowed.includes(tool)) {
      throw {
        code: 'denied',
        message: `Principal "phone" (role "${role}") may not call ${tool}.`,
        hint: 'Ask the machine owner to grant access in policy.toml.',
        machine: name,
      };
    }

    if (tool.startsWith('fs.') || tool.startsWith('fav.') || tool.startsWith('sync.')) {
      const handled = filesState.handleTool(name, tool, args, onEvent);
      if (handled !== undefined) return handled;
    }

    switch (tool) {
      case 'meta.version':
        return {
          name: 'legion',
          version: isMachineOld(name) ? OLD_FIXTURE_VERSION : isMixed ? '0.2.0' : '0.1.0',
          build: isMachineOld(name) ? OLD_FIXTURE_BUILD : 'mock00000000',
          protocol: 1,
          node: name,
          os: m.sys.os.family,
          arch: m.sys.os.arch,
        };
      case 'meta.update':
        return {
          current: isMachineOld(name) ? OLD_FIXTURE_VERSION : '0.1.0',
          latest: '0.2.0',
          up_to_date: !isMachineOld(name),
          checked: true,
        };
      case 'meta.whoami':
        return {
          principal: 'phone',
          role,
          door: kind === 'app' ? 'ssh' : 'http',
          node: name,
          delegate: false,
          paths: null,
          tools: machineAllowed,
        };
      case 'fleet.list': {
        const answering = isPrimaryOffline && activeNode === 'tower' ? 'pi' : activeNode;
        const list = [
          { name: 'tower', this_node: answering === 'tower', systems: [{ name: 'linux', via: 'ssh', target: 'tower.example' }], wake_via: [] },
          { name: 'pi', this_node: answering === 'pi', systems: [{ name: 'linux', via: 'ssh', target: 'pi.example' }], wake_via: [] },
          { name: 'studio', this_node: false, systems: [{ name: 'macos', via: 'ssh', target: 'studio.example' }], wake_via: [] },
          {
            name: 'laptop',
            this_node: false,
            systems: [{ name: 'linux', via: 'ssh', target: 'laptop.example' }],
            mac: '02:00:00:00:00:04',
            wake_via: ['pi'],
          },
          {
            name: 'nas',
            this_node: false,
            systems: [{ name: 'linux', via: 'ssh', target: 'nas.example' }],
            wake_via: [],
          },
        ];
        if (isFirst) {
          list.push({
            name: 'mini',
            this_node: false,
            systems: [{ name: 'linux', via: 'ssh', target: 'mini.example' }],
            wake_via: [],
          });
        }
        return { node: answering, answered_by: answering, machines: list };
      }
      case 'fleet.ping': {
        const answering = isPrimaryOffline && activeNode === 'tower' ? 'pi' : activeNode;
        const all = [
          isPrimaryOffline
            ? { machine: 'tower', reachable: false, error: machines.tower.error }
            : {
                machine: 'tower',
                reachable: true,
                node: 'tower',
                system: 'linux',
                version: isMachineOld('tower') ? OLD_FIXTURE_VERSION : isMixed ? '0.2.0' : '0.1.0',
                build: isMachineOld('tower') ? OLD_FIXTURE_BUILD : 'mock00000000',
                os: 'linux',
                arch: 'x86_64',
                latency_ms: 0,
              },
          {
            machine: 'pi',
            reachable: true,
            node: 'pi',
            system: 'linux',
            version: isMachineOld('pi') ? OLD_FIXTURE_VERSION : isMixed ? '0.2.0' : '0.1.0',
            build: isMachineOld('pi') ? OLD_FIXTURE_BUILD : 'mock00000000',
            os: 'linux',
            arch: 'aarch64',
            latency_ms: 4,
          },
          {
            machine: 'studio',
            reachable: true,
            node: 'studio',
            system: 'macos',
            version: isMachineOld('studio') ? OLD_FIXTURE_VERSION : '0.1.0',
            build: isMachineOld('studio') ? OLD_FIXTURE_BUILD : 'mock00000000',
            os: 'macos',
            arch: 'aarch64',
            latency_ms: 11,
          },
          machines.laptop.reachable
            ? { machine: 'laptop', reachable: true, node: 'laptop', system: 'linux', version: '0.1.0', os: 'linux', arch: 'x86_64', latency_ms: 8 }
            : { machine: 'laptop', reachable: false, error: machines.laptop.error },
          { machine: 'nas', reachable: false, error: machines.nas.error },
        ];
        if (isFirst) {
          all.push({ machine: 'mini', reachable: false, error: machines.mini.error });
        }
        if (Array.isArray(args?.machines) && args.machines.length > 0) {
          const wanted = new Set(args.machines);
          return { answered_by: answering, results: all.filter((entry) => wanted.has(entry.machine)) };
        }
        return { answered_by: answering, results: all.filter((entry) => entry.machine !== answering) };
      }
      case 'fleet.add': {
        const addedName = String(args?.name || '').trim();
        const addedHost = String(args?.host || '').trim();
        if (!addedName || !addedHost) {
          throw { code: 'invalid_args', message: 'Enter both a machine name and host.', hint: null, machine: name };
        }
        return { added: true, machine: addedName, host: addedHost, system: args?.system || 'linux' };
      }
      case 'device.roles': {
        if (isNoRoles) {
          throw {
            code: 'unknown_tool',
            message: 'Unknown tool: device.roles',
            hint: null,
            machine: name,
          };
        }
        return [
          {
            name: 'observer',
            description: 'look only (read status and files, change nothing)',
            strongest_effect: 'read',
            owner_strength: false,
          },
          {
            name: 'editor',
            description: 'change files (read and edit files, no running commands)',
            strongest_effect: 'write',
            owner_strength: false,
          },
          {
            name: 'operator',
            description: 'operate (read, change files, and run commands — as strong as this account)',
            strongest_effect: 'exec',
            owner_strength: false,
          },
          {
            name: 'owner',
            description: 'everything, including changing who may do what',
            strongest_effect: 'admin',
            owner_strength: true,
          },
        ];
      }
      case 'config.read': {
        if (isDenyRead) {
          throw {
            code: 'denied',
            message: 'Reading policy.toml is not allowed for this device.',
            hint: null,
            machine: name,
          };
        }
        if (args?.document === 'policy.toml') {
          return {
            exists: true,
            document: 'policy.toml',
            content: [
              'default = "nobody"',
              '',
              '[principals.phone]',
              'role = "operator"',
              '',
              '[roles.observer]',
              'allow = ["effect:read"]',
              '',
              '[roles.editor]',
              'allow = ["effect:read", "effect:write"]',
              '',
              '[roles.operator]',
              'allow = ["effect:read", "effect:write", "effect:exec", "effect:power"]',
              '',
            ].join('\n'),
          };
        }
        return { exists: false, document: args?.document || '' };
      }
      case 'device.list': {
        if (isFleet && isAutoEnrol && pendingInvites.length > 0 && name === activeNode) {
          deviceListPollCount += 1;
          if (deviceListPollCount >= 2) {
            const consumed = pendingInvites.shift();
            const newDev = {
              id: '0123456789abcdef0123456789abcdef',
              name: consumed.name || consumed.principal || 'tablet',
              principal: consumed.principal,
              role: consumed.role || 'observer',
              fingerprint: 'SHA256:enrolled-device-fp',
              added: new Date().toISOString(),
              foreign: false,
              missing_home: false,
            };
            incomingDevices = [newDev, ...incomingDevices];
            fleetDevicesByMachine = {
              tower: [...incomingDevices],
              pi: [...incomingDevices],
              studio: incomingDevices.filter((d) => d.id !== newDev.id),
            };
          }
        }
        const devs =
          fleetDevicesByMachine && fleetDevicesByMachine[name]
            ? fleetDevicesByMachine[name]
            : incomingDevices;
        return { devices: devs, invites: name === activeNode ? pendingInvites : [] };
      }
      case 'device.invite': {
        const chosenRole = String(args?.role || '').trim();
        const deviceName = String(args?.name || '').trim();
        const principal = deviceName
          ? `device:${deviceName.toLowerCase().replace(/[^a-z0-9_-]+/g, '-')}`
          : String(args?.principal || 'phone').trim() || 'phone';
        const effectiveRole = chosenRole || (principal === 'owner' ? 'owner' : 'operator');
        const validMin = Number(args?.valid_minutes) || 15;
        const id = `inv-mock-${inviteCounter++}`;
        const expires = new Date(Date.now() + validMin * 60_000).toISOString();
        const invite = `legion-invite-1.mock.${id}.${name}.${principal}.example`;
        deviceListPollCount = 0;
        pendingInvites = [
          {
            id,
            name: deviceName || undefined,
            principal,
            role: effectiveRole,
            fingerprint: 'SHA256:pending-invite',
            expires,
            created: new Date().toISOString(),
          },
          ...pendingInvites,
        ];
        return {
          invite,
          id,
          node: name,
          principal,
          role: effectiveRole,
          expires,
          hosts: [`${name}.example.ts.net`],
          port: 22,
          user: 'me',
        };
      }
      case 'device.remove': {
        if (args?.name) {
          incomingDevices = incomingDevices.filter((d) => d.name !== args.name);
          return { removed: true, name: args.name };
        }
        if (args?.id) {
          pendingInvites = pendingInvites.filter((inv) => inv.id !== args.id);
          return { removed: true, id: args.id };
        }
        throw { code: 'invalid_args', message: 'Specify a device name or invite id.', hint: null, machine: name };
      }
      case 'sys.info':
        return m.sys;
      case 'exec.run': {
        const line = args?.cmd || (Array.isArray(args?.argv) ? args.argv.join(' ') : '');
        const stdout = line.startsWith('df')
          ? 'Filesystem      Size  Used Avail Use% Mounted on\n/dev/nvme0n1p5  2.8T  247G  2.5T   9% /\n'
          : `${line || 'ok'}\n`;
        if (typeof onEvent === 'function') {
          onEvent({ kind: 'stdout', data: stdout, machine: name });
        }
        return {
          exit_code: 0,
          stdout,
          stderr: '',
          stdout_truncated: false,
          stderr_truncated: false,
          timed_out: false,
          duration_ms: 38,
        };
      }
      case 'job.list': {
        if (isEmpty) return { jobs: [], truncated: false };
        const jobs = args?.state ? m.jobs.filter((j) => j.state === args.state) : m.jobs;
        return {
          jobs: jobs.map(({ logs: _logs, _mockLive: _live, ...rest }) => rest),
          truncated: false,
        };
      }
      case 'job.start': {
        const id = `j-mock-${jobCounter++}`;
        const cmd = args?.cmd || (Array.isArray(args?.argv) ? args.argv.join(' ') : 'job');
        const job = {
          id,
          name: (args?.name && String(args.name).trim()) || cmd.split(/\s+/)[0] || 'job',
          state: 'running',
          created: new Date().toISOString(),
          command: cmd,
          duration_s: 0.1,
          logs: `Started ${cmd}\n`,
        };
        m.jobs.unshift(job);
        return { id: job.id, name: job.name, state: job.state, created: job.created, existing: false, warnings: [] };
      }
      case 'job.status': {
        const job = m.jobs.find((j) => j.id === args?.id);
        if (!job) throw { code: 'not_found', message: `No job ${args?.id}.`, hint: null, machine: name };
        return {
          id: job.id,
          kind: job.kind,
          name: job.name,
          title: job.title,
          cmd: job.command,
          from: job.from,
          to: job.to,
          move: job.move,
          hash: job.hash,
          error_summary: job.error_summary,
          progress: job.progress ? { ...job.progress } : undefined,
          state: job.state,
          created: job.created,
          finished: job.finished,
          exit_code: job.exit_code,
          duration_s: job.duration_s,
          output_bytes: job.logs.length,
        };
      }
      case 'job.logs': {
        const job = m.jobs.find((j) => j.id === args?.id);
        if (!job) throw { code: 'not_found', message: `No job ${args?.id}.`, hint: null, machine: name };
        const offset = Number.isInteger(args?.offset) ? args.offset : 0;
        const slice = job.logs.slice(offset);
        return {
          id: job.id,
          state: job.state,
          content: slice,
          offset,
          next_offset: job.logs.length,
          eof: true,
          truncated: false,
          output_dropped: false,
        };
      }
      case 'job.cancel': {
        const job = m.jobs.find((j) => j.id === args?.id);
        if (!job) throw { code: 'not_found', message: `No job ${args?.id}.`, hint: null, machine: name };
        job.state = 'cancelled';
        return { id: job.id, state: 'cancelled', survivors: [] };
      }
      case 'svc.list': {
        let services = m.services;
        if (args?.scope && args.scope !== 'all') services = services.filter((s) => s.scope === args.scope);
        if (args?.state) services = services.filter((s) => s.state === args.state);
        if (args?.filter) {
          const needle = args.filter.toLowerCase();
          services = services.filter((s) => `${s.name} ${s.description || ''}`.toLowerCase().includes(needle));
        }
        return { backend: m.sys.os.family === 'macos' ? 'launchd' : 'systemd', services, truncated: false };
      }
      case 'svc.status': {
        const svc = m.services.find((s) => s.name === args?.name);
        if (!svc) throw { code: 'not_found', message: `Service ${args?.name} not found.`, hint: null, machine: name };
        return { ...svc, pid: svc.state === 'running' ? 1420 : null };
      }
      case 'svc.logs':
        return {
          name: args?.name || 'service',
          scope: args?.scope || 'user',
          lines: [`${isoAgo(60)} ${args?.name}: listening and ready`],
          truncated: false,
        };
      case 'svc.start':
      case 'svc.restart': {
        const svc = m.services.find((s) => s.name === args?.name);
        if (!svc) throw { code: 'not_found', message: `Service ${args?.name} not found.`, hint: null, machine: name };
        svc.state = 'running';
        return { ...svc };
      }
      case 'svc.stop': {
        const svc = m.services.find((s) => s.name === args?.name);
        if (!svc) throw { code: 'not_found', message: `Service ${args?.name} not found.`, hint: null, machine: name };
        svc.state = 'stopped';
        return { ...svc };
      }
      case 'proj.list':
        return {
          node: name,
          projects: m.projects.map(({ git: _git, ...rest }) => rest),
        };
      case 'proj.status':
        return {
          projects: m.projects.map((p) => ({ name: p.name, path: p.path, present: p.present, git: p.git })),
        };
      case 'proj.pull':
        return { project: args?.project, before: 'b14ebff', after: 'b14ebff', summary: 'Already up to date.' };
      case 'proj.run':
        return { exit_code: 0, stdout: `Ran ${args?.task} in ${args?.project}\n`, stderr: '', duration_ms: 145 };
      case 'power.status':
        return m.power;
      case 'power.restart':
      case 'power.shutdown':
      case 'power.sleep': {
        const action = tool.split('.')[1];
        const cmd = action === 'restart' ? ['systemctl', 'reboot'] : action === 'shutdown' ? ['systemctl', 'poweroff'] : ['systemctl', 'suspend'];
        const dry = Boolean(args?.dry_run);
        const wouldRefuse =
          !args?.force && m.power.busy.state === 'busy'
            ? {
                code: 'busy',
                message: `Refused to ${action} ${name}: ${formatBusyReasonLines(m.power.busy.reasons).join('; ')}.`,
                hint: 'Pass force: true to go ahead anyway.',
              }
            : undefined;
        if (!dry && wouldRefuse) throw { ...wouldRefuse, machine: name };
        if (!dry) {
          m.power.pending = { action, in_s: args?.delay_s || 3, into: args?.into || null };
        }
        return {
          action,
          scheduled: !dry,
          in_s: args?.delay_s || 3,
          into: args?.into || undefined,
          boot_id: m.power.boot_id,
          commands: [cmd],
          dry_run: dry || undefined,
          would_refuse: dry ? wouldRefuse : undefined,
        };
      }
      case 'power.cancel': {
        const had = Boolean(m.power.pending);
        m.power.pending = null;
        return {
          cancelled: had,
          detail: had ? 'Cancelled pending power action.' : 'Nothing is pending on this machine.',
        };
      }
      case 'power.wake': {
        const wakeTarget = args?.machine || 'laptop';
        if (!args?.dry_run && machines[wakeTarget] && !machines[wakeTarget].reachable) {
          setTimeout(() => {
            const targetMachine = machines[wakeTarget];
            if (!targetMachine || targetMachine.reachable) return;
            targetMachine.reachable = true;
            targetMachine.error = null;
            if (!targetMachine.sys) {
              targetMachine.sys = {
                node: wakeTarget,
                os: { family: 'linux', name: 'Linux', arch: 'x86_64' },
                uptime_s: 12,
                load: [0.18, 0.14, 0.1],
                memory: { total: 32 * 1024 ** 3, used: 6 * 1024 ** 3, available: 26 * 1024 ** 3 },
                disks: [{ mount: '/', total: 1000 * 1024 ** 3, used: 420 * 1024 ** 3, available: 580 * 1024 ** 3 }],
                gpus: [],
              };
            }
          }, 450);
        }
        return {
          machine: wakeTarget,
          awake: false,
          sent_by: 'pi',
          dry_run: Boolean(args?.dry_run) || undefined,
          would_send: args?.dry_run ? ['255.255.255.255:9'] : undefined,
        };
      }
      default:
        return { ok: true, tool, args: args || {}, node: name };
    }
  }

  const MOCK_ANY_NODE_TOOLS = new Set([
    'fleet.list',
    'fleet.ping',
    'device.endpoint',
  ]);

  async function call(tool, args = {}, options = {}) {
    const { on, onEvent } = options || {};
    if (typeof window !== 'undefined') {
      window.__LEGION_MOCK_LAST_CALL__ = { tool, args, on: on ?? null };
      // Tests that must not depend on what the screen calls next read the recent calls instead.
      const recent = window.__LEGION_MOCK_CALLS__ || (window.__LEGION_MOCK_CALLS__ = []);
      recent.push({ tool, args, on: on ?? null });
      if (recent.length > 200) recent.splice(0, recent.length - 200);
    }
    if (kind === 'app' && !on && !MOCK_ANY_NODE_TOOLS.has(tool)) {
      throw {
        code: 'invalid_args',
        message: `Tool \`${tool}\` requires a target machine in \`on\`.`,
        hint: 'Pass `on: "<machine>"` to name the machine this call should run on.',
        machine: null,
      };
    }
    if (isFirst && tool === 'fleet.ping' && Array.isArray(args?.machines) && args.machines.includes('mini')) {
      // Keep the never-seen machine briefly in asking state so tests & screenshots can observe it.
      await new Promise((r) => setTimeout(r, 120));
    }
    if (on === 'all' || Array.isArray(on)) {
      const targets = on === 'all' ? Object.keys(machines) : on;
      const results = {};
      for (const machine of targets) {
        try {
          results[machine] = { ok: resolveOne(machine, tool, args, onEvent) };
        } catch (err) {
          if (typeof window !== 'undefined') {
            const errs = window.__LEGION_MOCK_ERRORS__ || (window.__LEGION_MOCK_ERRORS__ = []);
            errs.push({ tool, args, on: machine, code: err?.code || 'failed', message: err?.message || '' });
          }
          results[machine] = { error: err };
        }
      }
      return { results };
    }
    const target = on || (isPrimaryOffline && activeNode === 'tower' ? 'pi' : activeNode);
    try {
      return resolveOne(target, tool, args, onEvent);
    } catch (err) {
      if (typeof window !== 'undefined') {
        const errs = window.__LEGION_MOCK_ERRORS__ || (window.__LEGION_MOCK_ERRORS__ = []);
        errs.push({ tool, args, on: target, code: err?.code || 'failed', message: err?.message || '' });
      }
      throw err;
    }
  }

  async function state() {
    if (isRepair && !repaired) {
      return {
        ready: false,
        nodes: [],
        needs: 'repair',
        repair: {
          message: 'Saved device state could not be decoded.',
          path: '/home/me/.config/legion/device.json',
        },
        health: { ok: true, node: activeNode, version: '0.1.0' },
      };
    }
    if (!authed) {
      return {
        ready: false,
        nodes: [],
        needs: isPair || isRepair ? 'pairing' : 'token',
        can_scan: true,
        health: { ok: true, node: activeNode, version: '0.1.0' },
      };
    }
    const ordered = [activeNode, ...pairedNodes.filter((n) => n !== activeNode)];
    const staleCheckedAt = isoAgo(600);
    const nodes = ordered.map((nodeName) => {
      const hasMachine = Boolean(machines[nodeName]?.reachable);
      const isOfflinePrimary = isPrimaryOffline && nodeName === 'tower';
      const whoami = isOfflinePrimary
        ? {
            principal: 'phone',
            role: 'operator',
            door: 'ssh',
            node: 'tower',
            delegate: false,
            paths: null,
            tools: null,
            stale: true,
          }
        : hasMachine
          ? resolveOne(nodeName, 'meta.whoami', {})
          : {
              principal: 'phone',
              role,
              door: kind === 'app' ? 'ssh' : 'mock',
              node: nodeName,
              delegate: false,
              paths: null,
              tools: allowedTools,
            };
      const base = {
        node: nodeName,
        principal: whoami.principal,
        role: whoami.role,
        via: kind === 'app' ? 'ssh' : 'mock',
        reachable: !isOfflinePrimary,
        whoamiStale: isOfflinePrimary,
        roleCheckedAt: isOfflinePrimary ? staleCheckedAt : isoAgo(5),
        role_checked_at: isOfflinePrimary ? staleCheckedAt : isoAgo(5),
        origin: isFleet
          ? nodeName === 'tower'
            ? 'paired'
            : 'learned'
          : typeof window !== 'undefined'
            ? window.location.origin
            : `https://${nodeName}.example.ts.net`,
        version: isOfflinePrimary ? '' : isMachineOld(activeNode) ? OLD_FIXTURE_VERSION : isMixed ? '0.2.0' : '0.1.0',
        build: isOfflinePrimary ? null : isMachineOld(activeNode) ? OLD_FIXTURE_BUILD : 'mock00000000',
        protocol: 1,
        whoami,
      };
      if (isFleet) {
        if (nodeName === 'tower') {
          if (towerIdentityChanged) {
            base.route = 'identity_changed';
            base.route_via = 'pi';
            base.identity_changed = {
              reported_by: 'pi',
              hosts: ['tower.example.net'],
              port: 22,
              user: 'me',
              old_hosts: ['tower.example.net'],
              old_port: 22,
              old_user: 'me',
              old_fingerprints: ['SHA256:oldTowerKey111111111111111111111111111111'],
              new_fingerprints: ['SHA256:newTowerKey222222222222222222222222222222'],
              conflicts: [],
              is_paired: true,
            };
          } else {
            base.route = 'direct';
          }
        } else if (nodeName === 'pi') {
          base.learned_from = 'tower';
          base.route = 'direct';
        } else if (nodeName === 'studio') {
          base.learned_from = 'tower';
          base.route = 'not_synced';
          base.route_via = 'tower';
        } else if (nodeName === 'mini') {
          base.learned_from = 'tower';
          if (miniIdentityChanged) {
            base.route = 'identity_changed';
            base.route_via = 'tower';
            base.identity_changed = {
              reported_by: 'tower',
              hosts: ['mini-new.example.net'],
              port: 2222,
              user: 'me',
              old_hosts: ['mini.example.net'],
              old_port: 22,
              old_user: 'me',
              old_fingerprints: ['SHA256:oldMiniKey1111111111111111111111111111111'],
              new_fingerprints: ['SHA256:newMiniKey2222222222222222222222222222222'],
              conflicts: isConflict
                ? [
                    {
                      reported_by: 'studio',
                      hosts: ['mini-other.example.net'],
                      port: 22,
                      user: 'me',
                      fingerprints: ['SHA256:conflictMiniKey333333333333333333333333'],
                    },
                  ]
                : [],
              is_paired: false,
            };
          } else {
            base.route = 'direct';
          }
        }
      }
      return base;
    });
    const routes = isFleet
      ? {
          tower: { kind: 'direct' },
          pi: { kind: 'direct' },
          studio: { kind: 'not_synced', introducer: 'tower', via: 'tower' },
          nas: { kind: 'relay', via: 'tower' },
          mini: miniIdentityChanged
            ? { kind: 'identity_changed', via: 'tower' }
            : { kind: 'direct' },
        }
      : {};
    const disagreements = isDisagree
      ? [
          {
            machine: 'studio',
            reports: [
              {
                reported_by: 'tower',
                hosts: ['studio.example.net'],
                port: 22,
                user: 'me',
                fingerprints: ['SHA256:studioTowerKey111111111111111111111111111'],
              },
              {
                reported_by: 'pi',
                hosts: ['studio-alt.example.net'],
                port: 2222,
                user: 'me',
                fingerprints: ['SHA256:studioPiKey222222222222222222222222222222'],
              },
            ],
          },
        ]
      : [];
    return {
      ready: true,
      needs: null,
      can_scan: kind === 'app',
      health: { ok: true, node: activeNode, version: '0.1.0' },
      nodes,
      routes,
      disagreements,
      last_paired: lastPaired,
    };
  }

  // Seeds initial cached offline history for mock mode so previously-off machines stay calmly Off.
  function initialFleetCache() {
    const now = Date.now();
    return {
      version: 1,
      localNode: activeNode,
      cachedAt: now - 30_000,
      machines: [
        { name: 'tower', thisNode: activeNode === 'tower', reachable: true, status: 'online', os: 'linux', arch: 'x86_64', lastSeenAt: now - 30_000, lastCheckedAt: now - 30_000, canWake: false },
        { name: 'pi', thisNode: activeNode === 'pi', reachable: true, status: 'online', os: 'linux', arch: 'aarch64', lastSeenAt: now - 30_000, lastCheckedAt: now - 30_000, canWake: false },
        { name: 'studio', thisNode: false, reachable: true, status: 'online', os: 'macos', arch: 'aarch64', lastSeenAt: now - 30_000, lastCheckedAt: now - 30_000, canWake: false },
        { name: 'laptop', thisNode: false, reachable: false, status: 'offline', os: 'linux', lastSeenAt: machines.laptop.lastSeenAt, lastCheckedAt: now - 30_000, canWake: true },
        { name: 'nas', thisNode: false, reachable: false, status: 'offline', lastSeenAt: null, lastCheckedAt: now - 30_000, canWake: false },
      ],
    };
  }

  async function tools(options = {}) {
    if (kind === 'app' && !options?.on) {
      throw {
        code: 'invalid_args',
        message: 'Listing tools requires a single target machine in `on`.',
        hint: 'Pass `on: "<machine>"` to name the machine whose tool catalogue you want.',
        machine: null,
      };
    }
    const target = options?.on || activeNode;
    if (!machines[target] || !machines[target].reachable) {
      throw (
        machines[target]?.error || {
          code: 'unreachable',
          message: `${target} did not answer.`,
          hint: null,
          machine: target,
        }
      );
    }
    return toolsForMachine(target);
  }

  async function setToken(t) {
    if (!String(t || '').trim()) {
      throw { code: 'invalid_args', message: 'Enter a token first.', hint: null, machine: null };
    }
    authed = true;
    return state();
  }

  async function forget() {
    authed = false;
  }

  async function pair() {
    authed = true;
    if (!pairedNodes.includes('tower')) pairedNodes.push('tower');
    if (isFleet || isPair) {
      lastPaired = {
        node: 'tower',
        device: 'Pixel 10',
        role: 'operator',
        synced: ['pi'],
        not_synced: isPartial
          ? [{ node: 'studio', status: 'partial', reason: 'live device changes refused' }]
          : [{ node: 'studio', status: 'not_reached', reason: 'connection refused' }],
        sync_command: 'legion fleet.sync direction=push documents=\'["devices.toml"]\'',
      };
    }
    return state();
  }

  async function learn() {
    return {
      report: {
        introducer: activeNode,
        added: [],
        updated: [],
        identity_changed: miniIdentityChanged ? ['mini'] : [],
        skipped: [],
      },
      state: await state(),
    };
  }

  async function acceptIdentityChange(node, fingerprints = []) {
    if (node === 'tower' && towerIdentityChanged) {
      throw {
        code: 'denied',
        message: 'Cannot accept a learned identity change for directly paired node tower.',
        hint: null,
        machine: 'tower',
      };
    }
    if (node === 'mini' && miniIdentityChanged) {
      if (isConflict) {
        throw {
          code: 'conflict',
          message: 'Conflicting identity reports exist for mini.',
          hint: null,
          machine: 'mini',
        };
      }
      const expected = ['SHA256:newMiniKey2222222222222222222222222222222'];
      const actual = Array.isArray(fingerprints) ? fingerprints : [];
      if (actual.length !== expected.length || actual[0] !== expected[0]) {
        throw {
          code: 'conflict',
          message: 'Pending identity fingerprints for mini do not match what was reviewed.',
          hint: null,
          machine: 'mini',
        };
      }
      miniIdentityChanged = false;
    }
    return state();
  }

  async function dismissIdentityChange(node) {
    if (node === 'mini') miniIdentityChanged = false;
    if (node === 'tower') towerIdentityChanged = false;
    return state();
  }

  async function unpair(node) {
    pairedNodes = pairedNodes.filter((n) => n !== node);
    if (pairedNodes.length === 0) {
      authed = false;
      return { message: `Removed ${node}.` };
    }
    if (activeNode === node) activeNode = pairedNodes[0];
    return { message: `Removed ${node}.` };
  }

  async function selectNode(node) {
    if (pairedNodes.includes(node)) {
      activeNode = node;
    }
    return state();
  }

  async function repairStore() {
    repaired = true;
    return {
      archived_to: '/home/me/.config/legion/device.json.broken-20261124',
      message: 'Moved aside unreadable state.',
      state: await state(),
    };
  }

  async function deviceName() {
    return 'Pixel 10';
  }

  async function scanInvite() {
    return 'legion-invite-1.mock.scanned';
  }

  const updateWatchers = new Set();
  const mockDebSha256 = 'a'.repeat(64);
  let mockUpdateState =
    kind === 'app'
      ? {
          phase: isUpdateSeq0
            ? 'unsupported'
            : isUpdateDeb
              ? 'manual'
              : isUpdateFailed
                ? 'error'
                : isUpdateMetered
                  ? 'available'
                  : isUpdate
                    ? 'ready'
                    : 'up_to_date',
          current: {
            version: isUpdated ? '0.2.0' : '0.1.0',
            build: isUpdated ? 'feedface1234' : 'abc123456789',
            sequence: isUpdateSeq0 ? 0 : isUpdated ? 202611250205 : 202611250200,
            os: isUpdateDeb ? 'linux' : 'android',
            arch: isUpdateDeb ? 'x86_64' : 'aarch64',
            install_kind: isUpdateDeb ? 'deb' : 'apk',
            preview_enabled: true,
            release_key_fingerprint: 'SHA256:MockReleaseKey11111111111111111111111111111',
            preview_key_fingerprint: 'SHA256:MockPreviewKey22222222222222222222222222222',
          },
          sequence_floor: isUpdateSeq0 ? 0 : isUpdated ? 202611250205 : 202611250200,
          auto_update: true,
          can_self_install: !isUpdateDeb && !isUpdateSeq0,
          unsupported_reason: isUpdateSeq0
            ? 'This build has sequence 0, so self-updates are disabled.'
            : null,
          manual_command: isUpdateDeb
            ? 'sudo apt-get install /home/me/.local/share/com.x1f4r.legion/updates/verified/legion-app_0.2.0_amd64.deb'
            : null,
          manual_sha256: isUpdateDeb ? mockDebSha256 : null,
          available:
            isUpdate || isUpdateDeb || isUpdateFailed || isUpdateMetered
              ? {
                  channel: 'preview',
                  version: '0.2.0',
                  build: 'feedface1234',
                  sequence: 202611250205,
                  published_at: isoAgo(180),
                  notes: 'Automatic fleet updates and faster status refresh.',
                  signer: 'preview',
                  key_fingerprint: 'SHA256:MockPreviewKey22222222222222222222222222222',
                  source_node: 'tower',
                  artifact_name: isUpdateDeb ? 'legion-app_0.2.0_amd64.deb' : 'Legion-0.2.0.apk',
                  artifact_sha256: isUpdateDeb ? mockDebSha256 : '0'.repeat(64),
                  artifact_size: 18_000_000,
                  downloaded: !isUpdateMetered,
                }
              : null,
          progress: null,
          metered_paused: isUpdateMetered,
          last_checked_at: isoAgo(30),
          checked_nodes: ['tower'],
          just_updated: isUpdated
            ? {
                channel: 'preview',
                version: '0.2.0',
                build: 'feedface1234',
                sequence: 202611250205,
                notes: 'Automatic fleet updates and faster status refresh.',
                updated_at: isoAgo(60),
              }
            : null,
          rolled_back: null,
          failed_update: isUpdateFailed
            ? {
                channel: 'preview',
                version: '0.2.0',
                sequence: 202611250205,
                reason: 'Version 0.2.0 did not finish installing.',
                failed_at: isoAgo(120),
              }
            : null,
          android: isUpdateDeb
            ? null
            : {
                canRequestPackageInstalls: true,
                isSelfInstaller: true,
                installerPackage: 'com.x1f4r.legion',
                apiLevel: 35,
                supportsSilentSelfUpdate: true,
                isMetered: isUpdateMetered,
              },
          last_error: isUpdateFailed ? 'Version 0.2.0 did not finish installing.' : null,
          last_error_detail: isUpdateFailed ? 'INSTALL_FAILED_VERIFICATION_FAILURE' : null,
        }
      : null;

  function emitMockUpdateState() {
    if (!mockUpdateState) return;
    const copy = structuredClone(mockUpdateState);
    for (const fn of updateWatchers) {
      try {
        fn(copy);
      } catch {
        // Ignore callback failure.
      }
    }
  }

  async function updateState() {
    return mockUpdateState ? structuredClone(mockUpdateState) : null;
  }

  function watchUpdates(onState) {
    if (!mockUpdateState || typeof onState !== 'function') return () => {};
    updateWatchers.add(onState);
    onState(structuredClone(mockUpdateState));
    return () => updateWatchers.delete(onState);
  }

  async function checkUpdates() {
    if (!mockUpdateState) return null;
    mockUpdateState.last_checked_at = new Date().toISOString();
    emitMockUpdateState();
    return structuredClone(mockUpdateState);
  }

  async function installUpdate() {
    if (!mockUpdateState) return null;
    if (mockUpdateState.available && mockUpdateState.can_self_install) {
      const next = mockUpdateState.available;
      mockUpdateState = {
        ...mockUpdateState,
        phase: 'up_to_date',
        current: {
          ...mockUpdateState.current,
          version: next.version,
          build: next.build,
          sequence: next.sequence,
        },
        sequence_floor: next.sequence,
        available: null,
        just_updated: {
          channel: next.channel,
          version: next.version,
          build: next.build,
          sequence: next.sequence,
          notes: next.notes,
          updated_at: new Date().toISOString(),
        },
      };
      emitMockUpdateState();
    }
    return structuredClone(mockUpdateState);
  }

  async function setAutoUpdate(enabled) {
    if (!mockUpdateState) return null;
    mockUpdateState.auto_update = Boolean(enabled);
    emitMockUpdateState();
    return structuredClone(mockUpdateState);
  }

  async function dismissUpdateNotice() {
    if (!mockUpdateState) return null;
    mockUpdateState.just_updated = null;
    mockUpdateState.rolled_back = null;
    emitMockUpdateState();
    return structuredClone(mockUpdateState);
  }

  async function maybeAutoInstall(_trigger = 'startup') {
    return updateState();
  }

  const baseApi = {
    kind,
    mockFlags: flags,
    supportsContentFor: (machine) => !isMachineOld(machine || activeNode),
    state,
    tools,
    call,
    setToken,
    forget,
    deviceName,
    pair,
    unpair,
    selectNode,
    learn,
    acceptIdentityChange,
    dismissIdentityChange,
    repairStore,
    scanInvite,
    initialFleetCache,
    ...filesState.optionalLinkMethods,
  };

  if (kind === 'app') {
    return {
      ...baseApi,
      updateState,
      watchUpdates,
      checkUpdates,
      installUpdate,
      setAutoUpdate,
      dismissUpdateNotice,
      maybeAutoInstall,
    };
  }
  return baseApi;
}
