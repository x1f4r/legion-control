// Deterministic mock link (?mock) shaped like real Legion tool results.

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
];

export function createMockLink(mode = '') {
  let authed = mode !== 'token' && mode !== 'pair';
  const kind = mode === 'pair' ? 'app' : 'mock';
  let jobCounter = 10;

  const allowedTools = MOCK_TOOLS.filter((t) => t.effect !== 'admin').map((t) => t.name);

  const machines = {
    tower: {
      reachable: true,
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
          id: 'j-build-1',
          name: 'cargo build --release',
          state: 'running',
          created: isoAgo(252),
          command: 'cargo build --release',
          duration_s: 252,
          logs: '   Compiling legion-core v0.1.0\n   Compiling legion-doors v0.1.0\n   Compiling legion-cli v0.1.0\n',
        },
        {
          id: 'j-rsync-2',
          name: 'rsync photos/ nas:',
          state: 'running',
          created: isoAgo(3820),
          command: 'rsync -av photos/ nas:/volume1/photos/',
          duration_s: 3820,
          logs: 'sending incremental file list\n2026/autumn/IMG_4012.JPG\n2026/autumn/IMG_4013.JPG\n',
        },
        {
          id: 'j-test-3',
          name: 'cargo test',
          state: 'succeeded',
          created: isoAgo(7200),
          command: 'cargo test --workspace',
          exit_code: 0,
          duration_s: 41.2,
          logs: 'test result: ok. 142 passed; 0 failed\n',
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
        busy: { state: 'busy', reasons: ['exclusive job: cargo build --release'] },
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
        busy: { state: 'idle', reasons: [] },
        targets: [],
        sudo: false,
        pending: null,
      },
    },
    laptop: {
      reachable: false,
      error: {
        code: 'unreachable',
        message: 'Did not answer · last seen 14 days ago. SSH timed out after 8 s.',
        hint: 'Check that laptop is powered on and reachable on your tailnet, or wake it via pi.',
        machine: 'laptop',
      },
    },
  };

  const fsTree = {
    '/home/me': [
      { name: 'projects', path: '/home/me/projects', kind: 'dir', size: 4096, mtime: isoAgo(1200), mode: '0755' },
      { name: 'docs', path: '/home/me/docs', kind: 'dir', size: 4096, mtime: isoAgo(86400), mode: '0755' },
      { name: 'notes.txt', path: '/home/me/notes.txt', kind: 'file', size: 124, mtime: isoAgo(3600), mode: '0644' },
    ],
    '/home/me/projects': [
      { name: 'legion', path: '/home/me/projects/legion', kind: 'dir', size: 4096, mtime: isoAgo(600), mode: '0755' },
      { name: 'notes', path: '/home/me/projects/notes', kind: 'dir', size: 4096, mtime: isoAgo(5400), mode: '0755' },
    ],
    '/home/me/projects/legion': [
      { name: 'README.md', path: '/home/me/projects/legion/README.md', kind: 'file', size: 312, mtime: isoAgo(900), mode: '0644' },
      { name: 'Cargo.toml', path: '/home/me/projects/legion/Cargo.toml', kind: 'file', size: 184, mtime: isoAgo(900), mode: '0644' },
    ],
    '/home/me/docs': [
      { name: 'hello.txt', path: '/home/me/docs/hello.txt', kind: 'file', size: 38, mtime: isoAgo(86400), mode: '0644' },
    ],
  };

  const fsFiles = {
    '/home/me/notes.txt': 'Tower runs Linux 7.2 with 32 cores and 60 GB RAM.\nPi stays online as the Wake-on-LAN helper for laptop.\n',
    '/home/me/docs/hello.txt': 'Hello from Legion mock file preview.\n',
    '/home/me/projects/legion/README.md': '# Legion\n\nOne interface, one protocol, three pipes.\n',
    '/home/me/projects/legion/Cargo.toml': '[workspace]\nmembers = ["crates/*"]\n',
  };

  function resolveOne(target, tool, args, onEvent) {
    const name = target || 'tower';
    const m = machines[name];
    if (!m) {
      throw { code: 'not_found', message: `There is no machine called ${name} in fleet.toml.`, hint: null, machine: 'tower' };
    }
    if (!m.reachable) {
      throw m.error;
    }

    const spec = MOCK_TOOLS.find((t) => t.name === tool);
    if (spec && !allowedTools.includes(tool)) {
      throw {
        code: 'denied',
        message: `Principal "phone" (role "operator") may not call ${tool}.`,
        hint: 'Ask the machine owner to grant access in policy.toml.',
        machine: name,
      };
    }
    if (name === 'pi' && tool === 'audit.tail') {
      throw {
        code: 'denied',
        message: 'Principal "phone" may not call audit.tail on pi.',
        hint: 'Each machine checks its own policy.toml before running a tool.',
        machine: 'pi',
      };
    }

    switch (tool) {
      case 'meta.version':
        return {
          name: 'legion',
          version: '0.1.0',
          build: 'mock00000000',
          protocol: 1,
          node: name,
          os: m.sys.os.family,
          arch: m.sys.os.arch,
        };
      case 'meta.whoami':
        return {
          principal: 'phone',
          role: 'operator',
          door: 'http',
          node: name,
          delegate: false,
          paths: null,
          tools: allowedTools,
        };
      case 'fleet.list':
        return {
          node: 'tower',
          machines: [
            { name: 'tower', this_node: true, systems: [{ name: 'linux', via: 'ssh', target: 'tower.example' }], wake_via: [] },
            { name: 'pi', this_node: false, systems: [{ name: 'linux', via: 'ssh', target: 'pi.example' }], wake_via: [] },
            { name: 'studio', this_node: false, systems: [{ name: 'macos', via: 'ssh', target: 'studio.example' }], wake_via: [] },
            {
              name: 'laptop',
              this_node: false,
              systems: [{ name: 'linux', via: 'ssh', target: 'laptop.example' }],
              mac: '02:00:00:00:00:04',
              wake_via: ['pi'],
            },
          ],
        };
      case 'fleet.ping': {
        const all = [
          { machine: 'tower', reachable: true, node: 'tower', system: 'linux', version: '0.1.0', os: 'linux', arch: 'x86_64', latency_ms: 0 },
          { machine: 'pi', reachable: true, node: 'pi', system: 'linux', version: '0.1.0', os: 'linux', arch: 'aarch64', latency_ms: 4 },
          { machine: 'studio', reachable: true, node: 'studio', system: 'macos', version: '0.1.0', os: 'macos', arch: 'aarch64', latency_ms: 11 },
          { machine: 'laptop', reachable: false, error: machines.laptop.error },
        ];
        if (Array.isArray(args?.machines) && args.machines.length > 0) {
          const wanted = new Set(args.machines);
          return { results: all.filter((entry) => wanted.has(entry.machine)) };
        }
        return { results: all.filter((entry) => entry.machine !== 'tower') };
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
      case 'fs.list': {
        const requested = (args?.path || '~').replace(/^~(?=\/|$)/, '/home/me').replace(/\/+$/, '') || '/home/me';
        const entries = fsTree[requested];
        if (!entries) {
          throw { code: 'not_found', message: `${requested} does not exist.`, hint: null, machine: name };
        }
        return { path: requested, entries, truncated: false };
      }
      case 'fs.read': {
        const requested = (args?.path || '').replace(/^~(?=\/|$)/, '/home/me');
        const content = fsFiles[requested];
        if (content === undefined) {
          throw { code: 'not_found', message: `${requested} does not exist.`, hint: null, machine: name };
        }
        return {
          path: requested,
          size: content.length,
          offset: 0,
          length: content.length,
          encoding: 'utf8',
          content,
          eof: true,
        };
      }
      case 'job.list': {
        const jobs = args?.state ? m.jobs.filter((j) => j.state === args.state) : m.jobs;
        return {
          jobs: jobs.map(({ logs: _logs, ...rest }) => rest),
          truncated: false,
        };
      }
      case 'job.start': {
        const id = `j-mock-${jobCounter++}`;
        const cmd = args?.cmd || (Array.isArray(args?.argv) ? args.argv.join(' ') : 'job');
        const job = {
          id,
          name: args?.name || cmd.split(/\s+/)[0] || 'job',
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
          name: job.name,
          cmd: job.command,
          state: job.state,
          created: job.created,
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
                message: `Refused to ${action} ${name}: ${m.power.busy.reasons.join(', ')}.`,
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
      case 'power.wake':
        return {
          machine: args?.machine || 'laptop',
          awake: false,
          sent_by: 'pi',
          dry_run: Boolean(args?.dry_run) || undefined,
          would_send: args?.dry_run ? ['255.255.255.255:9'] : undefined,
        };
      default:
        return { ok: true, tool, args: args || {}, node: name };
    }
  }

  async function call(tool, args = {}, options = {}) {
    const { on, onEvent } = options || {};
    if (on === 'all' || Array.isArray(on)) {
      const targets = on === 'all' ? Object.keys(machines) : on;
      const results = {};
      for (const machine of targets) {
        try {
          results[machine] = { ok: resolveOne(machine, tool, args, onEvent) };
        } catch (err) {
          results[machine] = { error: err };
        }
      }
      return { results };
    }
    return resolveOne(on || 'tower', tool, args, onEvent);
  }

  async function state() {
    if (!authed) {
      return {
        ready: false,
        nodes: [],
        needs: mode === 'pair' ? 'pairing' : 'token',
        health: { ok: true, node: 'tower', version: '0.1.0' },
      };
    }
    const whoami = resolveOne('tower', 'meta.whoami', {});
    return {
      ready: true,
      needs: null,
      health: { ok: true, node: 'tower', version: '0.1.0' },
      nodes: [
        {
          node: 'tower',
          principal: whoami.principal,
          role: whoami.role,
          via: kind,
          origin: typeof window !== 'undefined' ? window.location.origin : 'https://tower.example.ts.net',
          version: '0.1.0',
          build: 'mock00000000',
          protocol: 1,
          whoami,
        },
      ],
    };
  }

  async function tools() {
    return MOCK_TOOLS;
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
    return state();
  }

  async function unpair() {
    authed = false;
  }

  async function deviceName() {
    return 'Pixel 10';
  }

  async function scanInvite() {
    return 'legion-invite:mock';
  }

  return {
    kind,
    state,
    tools,
    call,
    setToken,
    forget,
    deviceName,
    pair,
    unpair,
    scanInvite,
  };
}
