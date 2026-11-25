// Files mock state and tool handlers for link/mock.js: filesystem, thumbs, favourites, sync, and transfers.

import {
  baseFileName,
  canOpenExternally,
  categoryFromName,
  filterEntries,
  isHiddenName,
  joinFilePath,
  mimeFromName,
  parentFilePath,
  sortEntries,
} from '../lib/files.js';
import {
  base64ToBlobUrl,
  makeSampleImageBase64,
  makeSampleImageDataUrl,
  SAMPLE_AUDIO_OGG_BASE64,
  SAMPLE_PDF_BASE64,
  SAMPLE_PNG_BASE64,
  SAMPLE_UNPLAYABLE_VIDEO_BASE64,
  SAMPLE_VIDEO_WEBM_BASE64,
} from './mock-media.js';

const isoAgo = (sec) => new Date(Date.now() - sec * 1000).toISOString();

export const FILES_EXTRA_TOOLS = [
  {
    name: 'fs.find',
    effect: 'read',
    description: 'Search files and folders by name, kind, size, and modification time.',
    input_schema: {
      type: 'object',
      required: ['path'],
      properties: {
        path: { type: 'string' },
        name: { type: 'string' },
        glob: { type: 'string' },
        kind: { type: 'string', enum: ['file', 'dir', 'symlink', 'other'] },
        categories: { type: 'array', items: { type: 'string' } },
        min_size: { type: 'integer', minimum: 0 },
        max_size: { type: 'integer', minimum: 0 },
        modified_after: { type: 'string' },
        modified_before: { type: 'string' },
        sort: { type: 'string', enum: ['name', 'modified', 'size'] },
        order: { type: 'string', enum: ['asc', 'desc'] },
        hidden: { type: 'boolean', default: false },
        ignore: { type: 'boolean', default: false },
        limit: { type: 'integer', default: 200, minimum: 1, maximum: 5000 },
        budget_ms: { type: 'integer', default: 10000, minimum: 100, maximum: 60000 },
      },
    },
  },
  {
    name: 'fs.thumb',
    effect: 'read',
    description: 'Generate cached thumbnails for pictures and videos.',
    input_schema: {
      type: 'object',
      required: ['paths'],
      properties: {
        paths: { type: 'array', items: { type: 'string' }, minItems: 1, maxItems: 32 },
        size: { type: 'integer', default: 256, minimum: 96, maximum: 512 },
        fit: { type: 'string', enum: ['cover', 'contain'], default: 'cover' },
      },
    },
  },
  {
    name: 'fs.mkdir',
    effect: 'write',
    description: 'Create a directory.',
    input_schema: {
      type: 'object',
      required: ['path'],
      properties: {
        path: { type: 'string' },
        parents: { type: 'boolean', default: true },
      },
    },
  },
  {
    name: 'fs.copy',
    effect: 'write',
    description: 'Copy a file or folder on the same machine.',
    input_schema: {
      type: 'object',
      required: ['from', 'to'],
      properties: {
        from: { type: 'string' },
        to: { type: 'string' },
        overwrite: { type: 'boolean', default: false },
      },
    },
  },
  {
    name: 'fs.move',
    effect: 'write',
    description: 'Move or rename a file or folder.',
    input_schema: {
      type: 'object',
      required: ['from', 'to'],
      properties: {
        from: { type: 'string' },
        to: { type: 'string' },
        overwrite: { type: 'boolean', default: false },
      },
    },
  },
  {
    name: 'fs.stat',
    effect: 'read',
    description: 'Inspect metadata of one path.',
    input_schema: {
      type: 'object',
      required: ['path'],
      properties: { path: { type: 'string' } },
    },
  },
  {
    name: 'fs.hash',
    effect: 'read',
    description: 'Compute the BLAKE3 hash of a file.',
    input_schema: {
      type: 'object',
      required: ['path'],
      properties: { path: { type: 'string' } },
    },
  },
  {
    name: 'fav.list',
    effect: 'read',
    description: 'List starred and pinned files and folders across the fleet.',
    input_schema: {
      type: 'object',
      properties: {
        machine: { type: 'string' },
        pinned: { type: 'boolean' },
      },
    },
  },
  {
    name: 'fav.add',
    effect: 'write',
    description: 'Star or pin a file or folder.',
    input_schema: {
      type: 'object',
      required: ['path'],
      properties: {
        machine: { type: 'string' },
        path: { type: 'string' },
        label: { type: 'string' },
        pinned: { type: 'boolean' },
      },
    },
  },
  {
    name: 'fav.set',
    effect: 'write',
    description: 'Update label or pin state of a favourite.',
    input_schema: {
      type: 'object',
      required: ['id'],
      properties: {
        id: { type: 'string' },
        label: { type: 'string' },
        pinned: { type: 'boolean' },
      },
    },
  },
  {
    name: 'fav.remove',
    effect: 'write',
    description: 'Remove a favourite.',
    input_schema: {
      type: 'object',
      required: ['id'],
      properties: { id: { type: 'string' } },
    },
  },
  {
    name: 'sync.list',
    effect: 'read',
    description: 'List folders kept in sync across the fleet.',
    input_schema: { type: 'object', properties: {} },
  },
  {
    name: 'sync.status',
    effect: 'read',
    description: 'Read per-member status for a synced folder.',
    input_schema: {
      type: 'object',
      required: ['id'],
      properties: { id: { type: 'string' } },
    },
  },
  {
    name: 'sync.pause',
    effect: 'write',
    description: 'Pause syncing a folder.',
    input_schema: { type: 'object', required: ['id'], properties: { id: { type: 'string' } } },
  },
  {
    name: 'sync.resume',
    effect: 'write',
    description: 'Resume syncing a folder.',
    input_schema: { type: 'object', required: ['id'], properties: { id: { type: 'string' } } },
  },
  {
    name: 'sync.conflicts',
    effect: 'read',
    description: 'List files with two versions in a synced folder.',
    input_schema: { type: 'object', required: ['id'], properties: { id: { type: 'string' } } },
  },
  {
    name: 'sync.resolve',
    effect: 'write',
    description: 'Resolve a two-versions conflict in a synced folder.',
    input_schema: {
      type: 'object',
      required: ['id', 'path', 'keep'],
      properties: { id: { type: 'string' }, path: { type: 'string' }, keep: { type: 'string' } },
    },
  },
  {
    name: 'sync.bin',
    effect: 'read',
    description: 'List recently deleted files in a synced folder bin.',
    input_schema: { type: 'object', required: ['id'], properties: { id: { type: 'string' } } },
  },
  {
    name: 'sync.restore',
    effect: 'write',
    description: 'Restore a deleted file from a synced folder bin.',
    input_schema: {
      type: 'object',
      required: ['id', 'path'],
      properties: { id: { type: 'string' }, path: { type: 'string' } },
    },
  },
  {
    name: 'sync.add',
    effect: 'write',
    description: 'Add a machine to a synced folder.',
    input_schema: {
      type: 'object',
      required: ['id', 'machine', 'path'],
      properties: { id: { type: 'string' }, machine: { type: 'string' }, path: { type: 'string' } },
    },
  },
  {
    name: 'sync.remove',
    effect: 'write',
    description: 'Remove a machine from a synced folder.',
    input_schema: {
      type: 'object',
      required: ['id', 'machine'],
      properties: { id: { type: 'string' }, machine: { type: 'string' } },
    },
  },
  {
    name: 'sync.delete',
    effect: 'write',
    description: 'Stop keeping a folder in sync.',
    input_schema: { type: 'object', required: ['id'], properties: { id: { type: 'string' } } },
  },
  {
    name: 'sync.now',
    effect: 'write',
    description: 'Ask peers to sync now.',
    input_schema: { type: 'object', required: ['id'], properties: { id: { type: 'string' } } },
  },
];

export const OLD_FS_FIND_TOOL = {
  name: 'fs.find',
  effect: 'read',
  description: 'Find files and directories matching a glob pattern.',
  input_schema: {
    type: 'object',
    required: ['path'],
    properties: {
      path: { type: 'string' },
      glob: { type: 'string' },
      kind: { type: 'string', enum: ['file', 'dir', 'symlink', 'other'] },
      limit: { type: 'integer', default: 500 },
    },
  },
};

const PALETTES = [
  { a: '#3b6f7a', b: '#12262b', accent: '#7bd4c4', shape: 'coast' },
  { a: '#7a6a3b', b: '#2b2412', accent: '#f0c46c', shape: 'coast' },
  { a: '#5d7a3b', b: '#1b2b12', accent: '#c9e87e', shape: 'hills' },
  { a: '#3b4f7a', b: '#12182b', accent: '#8db8f7', shape: 'hills' },
  { a: '#7a3b5d', b: '#2b1220', accent: '#f29bc0', shape: 'coast' },
  { a: '#3b7a6a', b: '#122b25', accent: '#7ce8cf', shape: 'hills' },
  { a: '#7a4a3b', b: '#2b1812', accent: '#f4a57e', shape: 'hills' },
  { a: '#4a3b7a', b: '#18122b', accent: '#b79cf7', shape: 'hills' },
  { a: '#6a7a3b', b: '#252b12', accent: '#d8ef78', shape: 'coast' },
];

function paletteFor(path) {
  let h = 0;
  for (let i = 0; i < path.length; i += 1) {
    h = (h * 31 + path.charCodeAt(i)) >>> 0;
  }
  return PALETTES[h % PALETTES.length];
}

function makeEntry(path, opts = {}) {
  const kind = opts.kind || 'file';
  const name = opts.name || baseFileName(path);
  return {
    name,
    path,
    kind,
    size: opts.size ?? (kind === 'dir' ? 4096 : 1024),
    mtime: opts.mtime || isoAgo(86400 * 3),
    mode: opts.mode || (kind === 'dir' ? '0755' : '0644'),
    duration_s: opts.duration_s ?? undefined,
    palette: opts.palette || null,
    sampleMedia: opts.sampleMedia || null, // 'video' | 'audio' | 'unplayable' | 'image'
    thumbFail: Boolean(opts.thumbFail),
    mockContentStatus: opts.mockContentStatus || null,
    mockMintError: opts.mockMintError || null,
  };
}

function makeLongLogText() {
  const lines = [];
  for (let i = 1; i <= 2600; i += 1) {
    lines.push(
      `2026-11-24T12:${String(Math.floor(i / 60) % 60).padStart(2, '0')}:${String(i % 60).padStart(2, '0')}Z [tower] sync scan #${String(i).padStart(4, '0')} ok (verified 14 entries)`,
    );
  }
  return `${lines.join('\n')}\n`;
}

export function createMockFilesState({
  machines,
  flags,
  isMachineOld: isMachineOldProp = null,
  activeNodeRef,
  isAuthedRef = () => true,
  canReadRef = () => true,
}) {
  const isOld = flags.has('old') || flags.has('legacy');
  const isMixed = !isOld && (flags.has('mixed') || flags.has('old-peer'));
  const isMachineOld =
    typeof isMachineOldProp === 'function'
      ? isMachineOldProp
      : (name) => isOld || (isMixed && name === 'studio');
  const isNoSync = flags.has('nosync') || isOld;
  const isLarge = flags.has('large');
  const isInexact = flags.has('inexact');
  const isThumbRetry = flags.has('thumb-retry');
  const isTransfersDemo = flags.has('transfers');
  const isEmpty = flags.has('empty');
  const isSlowUpload = flags.has('slow-upload');
  const isFailUpload = flags.has('fail-upload');
  const isFormats = flags.has('formats');
  const isNoHandler = flags.has('no-handler');
  const isLongNames = flags.has('longnames');
  const isLongList = flags.has('longlist');
  const LONG_MACHINE_NAME = 'studio-rendering-workstation-north-01';
  const LONG_FOLDER_NAME =
    '2026-annual-architecture-deliverables-and-reference-assets-quarterly-verified-archive-release';

  const day = 86400;
  const longLogText = makeLongLogText();
  const LONG_NO_SPACE_NAME =
    'quarterly_photogrammetry_orthomosaic_calibration_dataset_2026_11_24_camera_rig_north_ridge_sector_04_final_verified_v12.txt';

  // Per-machine filesystem directories: Map<dirPath, Entry[]>
  const trees = {
    tower: new Map([
      [
        '/home/me',
        [
          makeEntry('/home/me/Videos', { kind: 'dir', mtime: isoAgo(day * 3) }),
          makeEntry('/home/me/Pictures', { kind: 'dir', mtime: isoAgo(day * 2) }),
          makeEntry('/home/me/Documents', { kind: 'dir', mtime: isoAgo(day * 5) }),
          makeEntry('/home/me/projects', { kind: 'dir', mtime: isoAgo(1200) }),
          makeEntry('/home/me/docs', { kind: 'dir', mtime: isoAgo(day) }),
          makeEntry('/home/me/Private', { kind: 'dir', mtime: isoAgo(day * 14) }),
          makeEntry('/home/me/notes.txt', { kind: 'file', size: 124, mtime: isoAgo(3600) }),
          makeEntry('/home/me/.profile', { kind: 'file', size: 280, mtime: isoAgo(day * 40) }),
          ...(isLarge ? [makeEntry('/home/me/Archive5000', { kind: 'dir', mtime: isoAgo(day) })] : []),
          ...(isFormats ? [makeEntry('/home/me/Formats', { kind: 'dir', mtime: isoAgo(day) })] : []),
        ],
      ],
      [
        '/home/me/Videos',
        [
          makeEntry('/home/me/Videos/Holiday', { kind: 'dir', mtime: isoAgo(day * 3) }),
          makeEntry('/home/me/Videos/2025', { kind: 'dir', mtime: isoAgo(day * 90) }),
        ],
      ],
      [
        '/home/me/Videos/Holiday',
        [
          makeEntry('/home/me/Videos/Holiday/Raw', { kind: 'dir', mtime: isoAgo(day * 3) }),
          makeEntry('/home/me/Videos/Holiday/Arrival.mp4', {
            size: 1_200_000_000,
            mtime: isoAgo(day * 3),
            duration_s: 724,
            palette: PALETTES[0],
            sampleMedia: 'video',
          }),
          makeEntry('/home/me/Videos/Holiday/Beach.mp4', {
            size: 840_000_000,
            mtime: isoAgo(day * 3 + 1800),
            duration_s: 511,
            palette: PALETTES[1],
            sampleMedia: 'video',
          }),
          makeEntry('/home/me/Videos/Holiday/Concert (H.264).mp4', {
            size: 6_300_000_000,
            mtime: isoAgo(day * 30),
            duration_s: 4210,
            palette: PALETTES[7],
            sampleMedia: 'unplayable',
          }),
          makeEntry('/home/me/Videos/Holiday/IMG_2041.jpg', {
            size: 4_100_000,
            mtime: isoAgo(day * 3 + 3600),
            palette: PALETTES[2],
            sampleMedia: 'image',
          }),
          makeEntry('/home/me/Videos/Holiday/IMG_2042.jpg', {
            size: 3_800_000,
            mtime: isoAgo(day * 3 + 4200),
            palette: PALETTES[3],
            sampleMedia: 'image',
          }),
          makeEntry('/home/me/Videos/Holiday/IMG_2043.jpg', {
            size: 4_500_000,
            mtime: isoAgo(day * 3 + 5000),
            palette: PALETTES[4],
            sampleMedia: 'image',
          }),
          makeEntry('/home/me/Videos/Holiday/Voice note.ogg', {
            size: 2_100_000,
            mtime: isoAgo(day * 4),
            duration_s: 48,
            sampleMedia: 'audio',
          }),
          makeEntry('/home/me/Videos/Holiday/Tickets.pdf', {
            size: 310_000,
            mtime: isoAgo(day * 7),
          }),
          makeEntry('/home/me/Videos/Holiday/notes.txt', {
            size: 2_048,
            mtime: isoAgo(day * 7),
          }),
          makeEntry('/home/me/Videos/Holiday/photos-backup.zip', {
            size: 412_000_000,
            mtime: isoAgo(day * 8),
          }),
          makeEntry(`/home/me/Videos/Holiday/${LONG_NO_SPACE_NAME}`, {
            size: 4_820,
            mtime: isoAgo(day * 6),
          }),
          makeEntry('/home/me/Videos/Holiday/.draft.txt', {
            size: 96,
            mtime: isoAgo(day * 2),
          }),
        ],
      ],
      [
        '/home/me/Videos/Holiday/Raw',
        [
          makeEntry('/home/me/Videos/Holiday/Raw/CLIP_001.mp4', {
            size: 420_000_000,
            mtime: isoAgo(day * 3),
            duration_s: 195,
            palette: PALETTES[5],
            sampleMedia: 'video',
          }),
          makeEntry('/home/me/Videos/Holiday/Raw/IMG_2040.jpg', {
            size: 5_200_000,
            mtime: isoAgo(day * 3),
            palette: PALETTES[6],
            sampleMedia: 'image',
          }),
        ],
      ],
      [
        '/home/me/Videos/2025',
        [
          makeEntry('/home/me/Videos/2025/Arrival at the lake.mov', {
            size: 2_600_000_000,
            mtime: isoAgo(day * 90),
            duration_s: 1412,
            palette: PALETTES[4],
            sampleMedia: 'video',
          }),
        ],
      ],
      [
        '/home/me/Pictures',
        [
          makeEntry('/home/me/Pictures/Camera', { kind: 'dir', mtime: isoAgo(day) }),
          makeEntry('/home/me/Pictures/IMG_2041.jpg', { size: 4_100_000, mtime: isoAgo(day * 3), palette: PALETTES[2], sampleMedia: 'image' }),
          makeEntry('/home/me/Pictures/IMG_2042.jpg', { size: 3_800_000, mtime: isoAgo(day * 3), palette: PALETTES[3], sampleMedia: 'image' }),
          makeEntry('/home/me/Pictures/IMG_2043.jpg', { size: 4_500_000, mtime: isoAgo(day * 4), palette: PALETTES[4], sampleMedia: 'image' }),
          makeEntry('/home/me/Pictures/IMG_2044.jpg', { size: 3_900_000, mtime: isoAgo(day * 4), palette: PALETTES[5], sampleMedia: 'image' }),
          makeEntry('/home/me/Pictures/IMG_2045.jpg', { size: 4_200_000, mtime: isoAgo(day * 5), palette: PALETTES[6], sampleMedia: 'image' }),
          makeEntry('/home/me/Pictures/IMG_2046.jpg', { size: 3_600_000, mtime: isoAgo(day * 6), palette: PALETTES[7], sampleMedia: 'image' }),
          makeEntry('/home/me/Pictures/Tickets.pdf', { size: 310_000, mtime: isoAgo(day * 7) }),
        ],
      ],
      [
        '/home/me/Pictures/Camera',
        [
          makeEntry('/home/me/Pictures/Camera/Raw', { kind: 'dir', mtime: isoAgo(day * 2) }),
          makeEntry('/home/me/Pictures/Camera/PXL_1001.mp4', { size: 220_000_000, mtime: isoAgo(day), duration_s: 724, palette: PALETTES[0], sampleMedia: 'video' }),
          makeEntry('/home/me/Pictures/Camera/PXL_1002.mp4', { size: 180_000_000, mtime: isoAgo(day + 600), duration_s: 511, palette: PALETTES[1], sampleMedia: 'video' }),
          makeEntry('/home/me/Pictures/Camera/PXL_1003.jpg', { size: 4_100_000, mtime: isoAgo(day * 2), palette: PALETTES[2], sampleMedia: 'image' }),
          makeEntry('/home/me/Pictures/Camera/PXL_1004.jpg', { size: 3_800_000, mtime: isoAgo(day * 2), palette: PALETTES[3], sampleMedia: 'image' }),
          makeEntry('/home/me/Pictures/Camera/PXL_1005.jpg', { size: 4_400_000, mtime: isoAgo(day * 3), palette: PALETTES[4], sampleMedia: 'image' }),
          makeEntry('/home/me/Pictures/Camera/PXL_1006.jpg', { size: 3_700_000, mtime: isoAgo(day * 3), palette: PALETTES[5], sampleMedia: 'image' }),
          makeEntry('/home/me/Pictures/Camera/PXL_1007.jpg', { size: 4_000_000, mtime: isoAgo(day * 4), palette: PALETTES[6], sampleMedia: 'image' }),
          makeEntry('/home/me/Pictures/Camera/PXL_1008.jpg', { size: 3_500_000, mtime: isoAgo(day * 4), palette: PALETTES[7], sampleMedia: 'image' }),
          makeEntry('/home/me/Pictures/Camera/PXL_1009.jpg', { size: 4_300_000, mtime: isoAgo(day * 5), palette: PALETTES[8], sampleMedia: 'image' }),
          makeEntry('/home/me/Pictures/Camera/PXL_1010.jpg', { size: 3_900_000, mtime: isoAgo(day * 5), palette: PALETTES[0], sampleMedia: 'image' }),
          makeEntry('/home/me/Pictures/Camera/PXL_1011.jpg', { size: 4_100_000, mtime: isoAgo(day * 6), palette: PALETTES[1], sampleMedia: 'image' }),
          makeEntry('/home/me/Pictures/Camera/Tickets.pdf', { size: 310_000, mtime: isoAgo(day * 7) }),
        ],
      ],
      [
        '/home/me/Pictures/Camera/Raw',
        [
          makeEntry('/home/me/Pictures/Camera/Raw/IMG_RAW_01.jpg', { size: 12_400_000, mtime: isoAgo(day * 2), palette: PALETTES[6], sampleMedia: 'image' }),
        ],
      ],
      [
        '/home/me/Documents',
        [
          makeEntry('/home/me/Documents/Empty', { kind: 'dir', mtime: isoAgo(day * 2) }),
          makeEntry('/home/me/Documents/Passport.pdf', { size: 540_000, mtime: isoAgo(day * 20), palette: { a: '#7a5b3b', b: '#2a1c14', accent: '#f0d8a8', shape: 'doc' } }),
          makeEntry('/home/me/Documents/README.md', { size: 412, mtime: isoAgo(day * 5) }),
          makeEntry('/home/me/Documents/broken-preview.jpg', { size: 64_000, mtime: isoAgo(day * 4), thumbFail: true }),
          makeEntry('/home/me/Documents/long-log.txt', { size: longLogText.length, mtime: isoAgo(3600) }),
        ],
      ],
      ['/home/me/Documents/Empty', []],
      [
        '/home/me/projects',
        [
          makeEntry('/home/me/projects/legion', { kind: 'dir', mtime: isoAgo(600) }),
          makeEntry('/home/me/projects/notes', { kind: 'dir', mtime: isoAgo(5400) }),
        ],
      ],
      [
        '/home/me/projects/legion',
        [
          makeEntry('/home/me/projects/legion/README.md', { size: 312, mtime: isoAgo(900) }),
          makeEntry('/home/me/projects/legion/Cargo.toml', { size: 184, mtime: isoAgo(900) }),
        ],
      ],
      [
        '/home/me/projects/notes',
        [
          makeEntry('/home/me/projects/notes/todo.md', { size: 148, mtime: isoAgo(5400) }),
        ],
      ],
      [
        '/home/me/docs',
        [
          makeEntry('/home/me/docs/hello.txt', { size: 38, mtime: isoAgo(day) }),
        ],
      ],
    ]),
    pi: new Map([
      [
        '/home/me',
        [
          makeEntry('/home/me/Films', { kind: 'dir', mtime: isoAgo(day * 6) }),
          makeEntry('/home/me/Documents', { kind: 'dir', mtime: isoAgo(day * 4) }),
          makeEntry('/home/me/Backups', { kind: 'dir', mtime: isoAgo(day * 2) }),
        ],
      ],
      [
        '/home/me/Films',
        [
          makeEntry('/home/me/Films/Arrival (2016).mkv', {
            size: 9_800_000_000,
            mtime: isoAgo(day * 45),
            duration_s: 6960,
            palette: PALETTES[3],
            sampleMedia: 'unplayable',
          }),
          makeEntry('/home/me/Films/Short clip.mp4', {
            size: 42_000_000,
            mtime: isoAgo(day * 10),
            duration_s: 95,
            palette: PALETTES[0],
            sampleMedia: 'video',
          }),
        ],
      ],
      [
        '/home/me/Documents',
        [
          makeEntry('/home/me/Documents/Passport.pdf', {
            size: 540_000,
            mtime: isoAgo(day * 20),
            palette: { a: '#7a5b3b', b: '#2a1c14', accent: '#f0d8a8', shape: 'doc' },
          }),
          makeEntry('/home/me/Documents/shopping.txt', { size: 84, mtime: isoAgo(day * 2) }),
        ],
      ],
      [
        '/home/me/Backups',
        [
          makeEntry('/home/me/Backups/tower-2026.tar.zst', { size: 4_800_000_000, mtime: isoAgo(day * 2) }),
        ],
      ],
    ]),
    studio: new Map([
      [
        '/home/me',
        [
          makeEntry('/home/me/Projects', { kind: 'dir', mtime: isoAgo(day) }),
          makeEntry('/home/me/Documents', { kind: 'dir', mtime: isoAgo(day * 3) }),
          makeEntry('/home/me/Pictures', { kind: 'dir', mtime: isoAgo(day * 2) }),
          ...(isMixed
            ? [makeEntry('/home/me/BigFolder', { kind: 'dir', mtime: isoAgo(day * 4) })]
            : []),
          ...(isFormats
            ? [makeEntry('/home/me/Formats', { kind: 'dir', mtime: isoAgo(day) })]
            : []),
        ],
      ],
      [
        '/home/me/Projects',
        [
          makeEntry('/home/me/Projects/legion', { kind: 'dir', mtime: isoAgo(day) }),
          makeEntry('/home/me/Projects/notes', { kind: 'dir', mtime: isoAgo(day * 4) }),
        ],
      ],
      [
        '/home/me/Projects/legion',
        [makeEntry('/home/me/Projects/legion/README.md', { size: 312, mtime: isoAgo(day) })],
      ],
      [
        '/home/me/Projects/notes',
        [makeEntry('/home/me/Projects/notes/ideas.md', { size: 220, mtime: isoAgo(day * 4) })],
      ],
      [
        '/home/me/Documents',
        [
          makeEntry('/home/me/Documents/design-notes.md', { size: 840, mtime: isoAgo(day * 3) }),
          ...(isMixed
            ? [
                makeEntry('/home/me/Documents/Report.pdf', { size: 2_400_000, mtime: isoAgo(day * 5) }),
                makeEntry('/home/me/Documents/avatar.png', { size: 48_000, mtime: isoAgo(day), palette: PALETTES[2], sampleMedia: 'image' }),
                makeEntry('/home/me/Documents/Poster.jpg', { size: 4_800_000, mtime: isoAgo(day * 6), palette: PALETTES[4], sampleMedia: 'image' }),
                makeEntry('/home/me/Documents/Clip.mp4', { size: 640_000_000, mtime: isoAgo(day * 2), duration_s: 312, palette: PALETTES[0], sampleMedia: 'video' }),
                makeEntry('/home/me/Documents/Track.mp3', { size: 6_200_000, mtime: isoAgo(day * 7), sampleMedia: 'audio' }),
                makeEntry('/home/me/Documents/archive.zip', { size: 18_500_000, mtime: isoAgo(day * 8) }),
              ]
            : []),
        ],
      ],
      [
        '/home/me/Pictures',
        [
          makeEntry('/home/me/Pictures/Camera', { kind: 'dir', mtime: isoAgo(day * 2) }),
          ...(isMixed
            ? [
                makeEntry('/home/me/Pictures/avatar.png', { size: 48_000, mtime: isoAgo(day), palette: PALETTES[2], sampleMedia: 'image' }),
              ]
            : []),
        ],
      ],
      [
        '/home/me/Pictures/Camera',
        [
          makeEntry('/home/me/Pictures/Camera/PXL_1003.jpg', { size: 4_100_000, mtime: isoAgo(day * 2), palette: PALETTES[2], sampleMedia: 'image' }),
        ],
      ],
    ]),
    laptop: new Map([
      [
        '/home/me',
        [
          makeEntry('/home/me/Documents', { kind: 'dir', mtime: isoAgo(day * 2) }),
          makeEntry('/home/me/Downloads', { kind: 'dir', mtime: isoAgo(day) }),
        ],
      ],
      ['/home/me/Documents', []],
      ['/home/me/Downloads', []],
    ]),
  };

  if (isMixed) {
    const big = [];
    for (let i = 1; i <= 520; i += 1) {
      const idx = String(i).padStart(3, '0');
      const ext = i % 4 === 0 ? 'jpg' : i % 4 === 1 ? 'txt' : i % 4 === 2 ? 'pdf' : 'mp4';
      big.push(
        makeEntry(`/home/me/BigFolder/file-${idx}.${ext}`, {
          size: 2048 * ((i % 40) + 1),
          mtime: isoAgo(day + i * 30),
        }),
      );
    }
    trees.studio.set('/home/me/BigFolder', big);
  }

  if (isLarge) {
    const big = [];
    for (let i = 1; i <= 5000; i += 1) {
      const idx = String(i).padStart(4, '0');
      const isPic = i % 3 === 0;
      const ext = isPic ? 'jpg' : 'txt';
      big.push(
        makeEntry(`/home/me/Archive5000/item-${idx}.${ext}`, {
          size: 1024 * ((i % 90) + 1),
          mtime: isoAgo(day + i * 10),
          sampleMedia: isPic ? 'image' : null,
        }),
      );
    }
    trees.tower.set('/home/me/Archive5000', big);
  }

  if (isLongNames) {
    const longFolderPath = `/home/me/${LONG_FOLDER_NAME}`;
    trees[LONG_MACHINE_NAME] = new Map([
      [
        '/home/me',
        [
          makeEntry(longFolderPath, { kind: 'dir', mtime: isoAgo(day) }),
          makeEntry('/home/me/Documents', { kind: 'dir', mtime: isoAgo(day * 2) }),
          makeEntry('/home/me/notes.txt', { size: 420, mtime: isoAgo(3600) }),
        ],
      ],
      [
        longFolderPath,
        [
          makeEntry(`${longFolderPath}/reference-diagrams`, { kind: 'dir', mtime: isoAgo(day) }),
          makeEntry(`${longFolderPath}/overview-specification-v4.pdf`, { size: 2_840_000, mtime: isoAgo(day * 2) }),
          makeEntry(`${longFolderPath}/render-pass-01.jpg`, {
            size: 4_200_000,
            mtime: isoAgo(day * 2),
            palette: PALETTES[2],
            sampleMedia: 'image',
          }),
          makeEntry(`${longFolderPath}/render-pass-02.jpg`, {
            size: 3_950_000,
            mtime: isoAgo(day * 3),
            palette: PALETTES[3],
            sampleMedia: 'image',
          }),
          makeEntry(`${longFolderPath}/scene-checklist-final.txt`, { size: 1_840, mtime: isoAgo(3600) }),
          makeEntry(`${longFolderPath}/asset-bundle-2026.zip`, { size: 248_000_000, mtime: isoAgo(day * 4) }),
        ],
      ],
      [`${longFolderPath}/reference-diagrams`, []],
      ['/home/me/Documents', []],
    ]);
  }

  if (isLongList) {
    const towerHome = trees.tower.get('/home/me');
    for (let i = 1; i <= 36; i += 1) {
      const idx = String(i).padStart(2, '0');
      const ext = i % 4 === 0 ? 'jpg' : i % 4 === 1 ? 'pdf' : i % 4 === 2 ? 'txt' : 'zip';
      towerHome.push(
        makeEntry(`/home/me/item-${idx}-${ext === 'jpg' ? 'photo' : ext === 'pdf' ? 'report' : ext === 'txt' ? 'notes' : 'archive'}.${ext}`, {
          size: 48_000 * (i + 2),
          mtime: isoAgo(day * (i + 1)),
          sampleMedia: ext === 'jpg' ? 'image' : null,
          palette: ext === 'jpg' ? PALETTES[i % PALETTES.length] : null,
        }),
      );
    }
    for (let i = 1; i <= 12; i += 1) {
      const mName = `node-${String(i).padStart(2, '0')}`;
      trees[mName] = new Map([
        [
          '/home/me',
          [
            makeEntry('/home/me/Documents', { kind: 'dir', mtime: isoAgo(day * i) }),
            makeEntry('/home/me/logs.txt', { size: 2048, mtime: isoAgo(3600 * i) }),
          ],
        ],
        ['/home/me/Documents', []],
      ]);
    }
  }

  // Root `/` directory pointing to `/home` for Unix root browsing tests.
  for (const mName of Object.keys(trees)) {
    if (!trees[mName] || mName === 'laptop') continue;
    trees[mName].set('/', [
      makeEntry('/home', { kind: 'dir', mtime: isoAgo(day * 100) }),
      makeEntry('/etc', { kind: 'dir', mtime: isoAgo(day * 20) }),
    ]);
    trees[mName].set('/home', [makeEntry('/home/me', { kind: 'dir', mtime: isoAgo(day) })]);
    trees[mName].set('/etc', [makeEntry('/etc/hosts', { size: 120, mtime: isoAgo(day * 20) })]);
  }

  const textFiles = {
    'tower:/home/me/notes.txt': 'Tower runs Linux 7.2 with 32 cores and 60 GB RAM.\nPi stays online as the Wake-on-LAN helper for laptop.\n',
    'tower:/home/me/docs/hello.txt': 'Hello from Legion mock file preview.\n',
    'tower:/home/me/projects/legion/README.md': '# Legion\n\nOne interface, one protocol, three pipes.\n',
    'tower:/home/me/projects/legion/Cargo.toml': '[workspace]\nmembers = ["crates/*"]\n',
    'tower:/home/me/projects/notes/todo.md': '- Finish holiday photo album\n- Sync camera folder\n',
    'tower:/home/me/Videos/Holiday/notes.txt': 'Holiday itinerary\n\nDay 1: Arrival at the coast, sunset walk.\nDay 2: Beach morning, boat trip.\nDay 3: Mountain pass and evening concert.\n',
    [`tower:/home/me/Videos/Holiday/${LONG_NO_SPACE_NAME}`]:
      'calibration_run_id=2026_11_24_north_ridge_sector_04_final_verified_v12\n' +
      'camera_matrix=1420.550000,0.000000,960.000000,0.000000,1420.550000,540.000000,0.000000,0.000000,1.000000\n' +
      'status=verified_within_0.18px_reprojection_error_across_all_48_ground_control_targets_without_drift\n',
    'tower:/home/me/Videos/Holiday/.draft.txt': 'Hidden draft notes for packing list.\n',
    'tower:/home/me/Documents/README.md': '# Documents\n\nSynced between tower, pi and studio.\n',
    'tower:/home/me/Documents/long-log.txt': longLogText,
    'tower:/etc/hosts': '127.0.0.1 localhost\n',
    'pi:/home/me/Documents/shopping.txt': 'Bread\nCoffee\nOlive oil\n',
    'studio:/home/me/Documents/design-notes.md': '# Design notes\n\nPlain words first. Daily actions in reach of the thumb.\n',
    'studio:/home/me/Projects/legion/README.md': '# Legion\n\nOne interface, one protocol, three pipes.\n',
    'studio:/home/me/Projects/notes/ideas.md': '# Ideas\n\nKeep folders in sync without a central hub.\n',
  };

  if (isFormats) {
    trees.tower.set('/home/me/Formats', [
      makeEntry('/home/me/Formats/studio-master.flac', {
        size: 43_000_000,
        mtime: isoAgo(day),
        sampleMedia: 'audio',
      }),
      makeEntry('/home/me/Formats/iphone-shot.heic', {
        size: 1_500_000,
        mtime: isoAgo(day),
        palette: PALETTES[2],
        sampleMedia: 'image',
      }),
      makeEntry('/home/me/Formats/camera-raw.dng', {
        size: 12_000_000,
        mtime: isoAgo(day),
        thumbFail: true,
      }),
      makeEntry('/home/me/Formats/legacy-track.wma', {
        size: 5_000_000,
        mtime: isoAgo(day),
      }),
      makeEntry('/home/me/Formats/legacy-clip.avi', {
        size: 20_000_000,
        mtime: isoAgo(day),
      }),
      makeEntry('/home/me/Formats/README', {
        size: 86,
        mtime: isoAgo(day),
      }),
      makeEntry('/home/me/Formats/installer.apk', {
        size: 18_000_000,
        mtime: isoAgo(day),
      }),
      makeEntry('/home/me/Formats/diagram.svg', {
        size: 4_096,
        mtime: isoAgo(day),
      }),
      makeEntry('/home/me/Formats/fail-413.flac', {
        size: 43_000_000,
        mtime: isoAgo(day),
        sampleMedia: 'unplayable',
        mockContentStatus: {
          request_count: 1,
          status: 413,
          content_type: 'application/json',
          stream_failed: false,
          error_code: 'too_large_for_inline',
          error_message: 'File exceeds inline size cap.',
          ticket_live: true,
        },
      }),
      makeEntry('/home/me/Formats/fail-502.flac', {
        size: 43_000_000,
        mtime: isoAgo(day),
        sampleMedia: 'unplayable',
        mockContentStatus: {
          request_count: 1,
          status: 502,
          content_type: 'application/json',
          stream_failed: false,
          error_code: 'unreachable',
          error_message: 'Machine tower dropped the stream.',
          ticket_live: true,
        },
      }),
      makeEntry('/home/me/Formats/fail-truncated.flac', {
        size: 43_000_000,
        mtime: isoAgo(day),
        sampleMedia: 'unplayable',
        mockContentStatus: {
          request_count: 1,
          status: 206,
          content_type: 'audio/flac',
          stream_failed: true,
          ticket_live: true,
        },
      }),
      makeEntry('/home/me/Formats/no-route.flac', {
        size: 43_000_000,
        mtime: isoAgo(day),
        mockMintError: {
          code: 'not_found',
          message: 'Machine tower is not paired on this device.',
          hint: 'Pair with that machine or refresh learned fleet routes first.',
        },
      }),
    ]);
    textFiles['tower:/home/me/Formats/README'] =
      'Formats fixture README\n\nPlain text without an extension.\n';
    trees.studio.set('/home/me/Formats', [
      makeEntry('/home/me/Formats/relay-mid-fail.flac', {
        size: 43_000_000,
        mtime: isoAgo(day),
        sampleMedia: 'unplayable',
        mockContentStatus: {
          request_count: 1,
          status: 206,
          content_type: 'audio/flac',
          stream_failed: true,
          ticket_live: true,
        },
      }),
      makeEntry('/home/me/Formats/relay-mid-fail.png', {
        size: 6_200_000,
        mtime: isoAgo(day),
        sampleMedia: 'unplayable',
        mockContentStatus: {
          request_count: 1,
          status: 200,
          content_type: 'image/png',
          stream_failed: true,
          ticket_live: true,
        },
      }),
      makeEntry('/home/me/Formats/relay-too-old.flac', {
        size: 43_000_000,
        mtime: isoAgo(day),
        mockMintError: {
          code: 'unsupported',
          machine: 'tower',
          message: 'tower needs an update before it can pass files along from studio.',
          hint: 'Update legion on tower so it supports relayed streams.',
          details: {
            reason: 'relay_too_old',
            relay: 'tower',
            target: 'studio',
          },
        },
      }),
    ]);
  }

  let favCounter = 10;
  let favourites = [
    {
      id: 'fav-1',
      machine: 'tower',
      path: '/home/me/Pictures',
      kind: 'dir',
      category: null,
      label: 'Photos',
      pinned: true,
      exists: true,
      added_at: isoAgo(day * 20),
      added_by: 'phone',
    },
    {
      id: 'fav-2',
      machine: 'pi',
      path: '/home/me/Films',
      kind: 'dir',
      category: null,
      label: 'Films',
      pinned: true,
      exists: true,
      added_at: isoAgo(day * 18),
      added_by: 'phone',
    },
    {
      id: 'fav-3',
      machine: 'pi',
      path: '/home/me/Documents/Passport.pdf',
      kind: 'file',
      category: 'document',
      label: 'Passport.pdf',
      pinned: true,
      exists: true,
      added_at: isoAgo(day * 15),
      added_by: 'phone',
    },
    ...(!isMixed
      ? [
          {
            id: 'fav-4',
            machine: 'studio',
            path: '/home/me/Projects',
            kind: 'dir',
            category: null,
            label: 'Projects',
            pinned: true,
            exists: true,
            added_at: isoAgo(day * 12),
            added_by: 'phone',
          },
        ]
      : []),
    {
      id: 'fav-5',
      machine: 'tower',
      path: '/home/me/Videos/Holiday/Arrival.mp4',
      kind: 'file',
      category: 'video',
      label: 'Arrival.mp4',
      pinned: false,
      exists: true,
      added_at: isoAgo(day * 3),
      added_by: 'phone',
    },
    {
      id: 'fav-6',
      machine: 'tower',
      path: '/home/me/Videos/Holiday',
      kind: 'dir',
      category: null,
      label: 'Holiday',
      pinned: false,
      exists: true,
      added_at: isoAgo(day * 3),
      added_by: 'phone',
    },
    {
      id: 'fav-7',
      machine: 'tower',
      path: '/home/me/Videos/Holiday/Tickets.pdf',
      kind: 'file',
      category: 'document',
      label: 'Tickets.pdf',
      pinned: false,
      exists: true,
      added_at: isoAgo(day * 6),
      added_by: 'phone',
    },
    {
      id: 'fav-8',
      machine: 'tower',
      path: '/home/me/Documents/long-log.txt',
      kind: 'file',
      category: 'text',
      label: 'long-log.txt',
      pinned: false,
      exists: true,
      added_at: isoAgo(day * 2),
      added_by: 'phone',
    },
    {
      id: 'fav-9',
      machine: 'studio',
      path: '/home/me/Documents/design-notes.md',
      kind: 'file',
      category: 'text',
      label: 'design-notes.md',
      pinned: false,
      exists: true,
      added_at: isoAgo(day * 4),
      added_by: 'phone',
    },
  ];

  let syncFolders = [
    {
      id: 'sync-documents',
      name: 'Documents',
      file_count: 84,
      total_bytes: 185_000_000,
      paused: false,
      summary: 'tower, pi and studio · Up to date',
      members: [
        { machine: 'tower', path: '/home/me/Documents', state: 'up_to_date', sentence: 'Up to date' },
        { machine: 'pi', path: '/home/me/Documents', state: 'up_to_date', sentence: 'Up to date' },
        { machine: 'studio', path: '/home/me/Documents', state: 'up_to_date', sentence: 'Up to date' },
      ],
      conflicts: [],
      bin: [],
    },
    {
      id: 'sync-camera',
      name: 'Camera',
      file_count: 1204,
      total_bytes: 38_000_000_000,
      paused: false,
      summary: '14 files waiting for laptop, which is off',
      members: [
        { machine: 'tower', path: '/home/me/Pictures/Camera', state: 'up_to_date', sentence: 'Up to date' },
        { machine: 'pi', path: '/home/me/Pictures/Camera', state: 'up_to_date', sentence: 'Up to date' },
        {
          machine: 'studio',
          path: '/home/me/Pictures/Camera',
          state: 'syncing',
          files_left: 3,
          bytes_left: 410_000_000,
          progress_pct: 62,
          sentence: 'Getting 3 files · 410 MB left',
        },
        {
          machine: 'laptop',
          path: '/home/me/Pictures/Camera',
          state: 'waiting',
          files_left: 14,
          sentence: 'Off · 14 files waiting for it',
        },
      ],
      conflicts: [
        {
          path: 'PXL_1003.jpg',
          other_path: 'PXL_1003 (changed on studio 2026-11-23).jpg',
          machines: ['tower', 'studio'],
          summary: 'Changed on tower and on studio while apart. Both are kept.',
        },
      ],
      bin: [
        { path: 'PXL_0991.jpg', size: 3_400_000, deleted_at: isoAgo(day * 2), days_left: 28 },
        { path: 'PXL_0992.jpg', size: 3_900_000, deleted_at: isoAgo(day * 3), days_left: 27 },
        { path: 'PXL_0994.jpg', size: 4_100_000, deleted_at: isoAgo(day * 5), days_left: 25 },
        { path: 'PXL_0995.jpg', size: 2_800_000, deleted_at: isoAgo(day * 6), days_left: 24 },
        { path: 'PXL_0998.mp4', size: 84_000_000, deleted_at: isoAgo(day * 9), days_left: 21 },
        { path: 'PXL_0999.jpg', size: 3_600_000, deleted_at: isoAgo(day * 11), days_left: 19 },
      ],
    },
  ];

  // Seed historical machine transfer jobs in tower (terminal so existing runningJobs=2 test stays green!).
  if (machines.tower?.jobs) {
    machines.tower.jobs.push(
      {
        id: 'j-xfer-done-1',
        kind: 'transfer',
        name: 'Moved 12 items to nas',
        title: 'Moved 12 items to nas',
        state: 'succeeded',
        created: isoAgo(7440),
        finished: isoAgo(7200),
        command: 'fs.transfer --move /home/me/Videos/2025 nas:/volume1/Videos',
        from: '/home/me/Videos/2025',
        to: 'nas:/volume1/Videos',
        move: true,
        exit_code: 0,
        duration_s: 240,
        progress: { bytes_done: 8_400_000_000, bytes_total: 8_400_000_000, files_done: 12, files_total: 12 },
        hash: 'blake3:4a9c7d22e1f08b63a91c5d882e44b190',
        logs: 'Verified 12 files by BLAKE3 hash and removed sources.\n',
      },
      {
        id: 'j-xfer-stop-2',
        kind: 'transfer',
        name: 'Copying Films to laptop',
        title: 'Copying Films to laptop',
        state: 'failed',
        created: isoAgo(10800),
        finished: isoAgo(10500),
        command: 'fs.transfer /home/me/Videos laptop:/home/me/Films',
        from: '/home/me/Videos',
        to: 'laptop:/home/me/Films',
        error_summary: 'Stopped when laptop went off. It continues from where it was when you start it again.',
        exit_code: 1,
        duration_s: 300,
        progress: { bytes_done: 1_450_000_000, bytes_total: 4_200_000_000, files_done: 2, files_total: 6 },
        logs: 'Connection to laptop lost after 1.45 GB. Partial file kept for resume.\n',
      },
    );
    if (isTransfersDemo) {
      machines.tower.jobs.unshift({
        id: 'j-xfer-live-1',
        kind: 'transfer',
        name: 'Copying Arrival.mp4 to pi',
        title: 'Copying Arrival.mp4 to pi',
        state: 'running',
        created: isoAgo(48),
        command: 'fs.transfer /home/me/Videos/Holiday/Arrival.mp4 pi:/home/me/Films/Arrival.mp4',
        from: '/home/me/Videos/Holiday/Arrival.mp4',
        to: 'pi:/home/me/Films/Arrival.mp4',
        duration_s: 48,
        progress: {
          bytes_done: 498_000_000,
          bytes_total: 1_200_000_000,
          files_done: 0,
          files_total: 1,
          current: 'Arrival.mp4',
          rate: 12_000_000,
        },
        logs: 'Streaming Arrival.mp4 to pi:/home/me/Films/Arrival.mp4...\n',
      });
    }
  }

  // Device-side transfer queue (downloads and uploads on this device).
  let transferSeq = 20;
  const transferListeners = new Set();
  const deviceTransfers = [
    ...(isTransfersDemo
      ? [
          {
            id: 'dt-live-1',
            direction: 'download',
            machine: 'tower',
            path: '/home/me/Videos/Holiday/Beach.mp4',
            name: 'Beach.mp4',
            localPath: 'Downloads/Legion/Beach.mp4',
            mime: 'video/mp4',
            state: 'running',
            bytes_done: 210_000_000,
            bytes_total: 840_000_000,
            rate: 14_000_000,
            error: null,
            created: isoAgo(30),
            finished: null,
            hash: null,
            canOpen: false,
          },
        ]
      : []),
    {
      id: 'dt-done-1',
      direction: 'download',
      machine: 'tower',
      path: '/home/me/Videos/Holiday/Tickets.pdf',
      name: 'Tickets.pdf',
      localPath: 'Downloads/Legion/Tickets.pdf',
      mime: 'application/pdf',
      state: 'done',
      bytes_done: 310_000,
      bytes_total: 310_000,
      rate: 0,
      error: null,
      created: isoAgo(3600),
      finished: isoAgo(3596),
      hash: 'blake3:88f12c09aa7b34e2c51d0e899a441f2c',
      canOpen: true,
    },
    {
      id: 'dt-failed-1',
      direction: 'download',
      machine: 'tower',
      path: '/home/me/Videos/Holiday/photos-backup.zip',
      name: 'photos-backup.zip',
      localPath: 'Downloads/Legion/photos-backup.zip',
      mime: 'application/zip',
      state: 'failed',
      bytes_done: 164_000_000,
      bytes_total: 412_000_000,
      rate: 0,
      error: {
        code: 'unreachable',
        message: 'Connection dropped after 164 MB. Resume continues from where it stopped.',
      },
      created: isoAgo(5400),
      finished: isoAgo(5280),
      hash: null,
      canOpen: false,
    },
  ];

  function snapshotTransfers() {
    return deviceTransfers.map((t) => ({ ...t }));
  }

  function emitTransfers() {
    const snap = snapshotTransfers();
    for (const fn of transferListeners) {
      try {
        fn(snap);
      } catch {
        // Keep notifying remaining listeners.
      }
    }
  }

  let pumpTimer = null;
  function ensureTransferPump() {
    if (pumpTimer) return;
    const hasActive = () =>
      deviceTransfers.some((t) => t.state === 'running' || t.state === 'queued') ||
      Object.values(machines).some((m) =>
        (m.jobs || []).some((j) => j.kind === 'transfer' && j.state === 'running' && j._mockLive),
      );
    if (!hasActive()) return;
    pumpTimer = setInterval(() => {
      let changed = false;

      // Advance at most 2 concurrent device transfers; promote queued ones.
      let runningCount = deviceTransfers.filter((t) => t.state === 'running').length;
      for (const t of deviceTransfers) {
        if (runningCount >= 2) break;
        if (t.state === 'queued') {
          t.state = 'running';
          runningCount += 1;
          changed = true;
        }
      }
      for (const t of deviceTransfers) {
        if (t.state !== 'running') continue;
        // Keep the demo row moving gently so screenshots stay near ~25-45% while user-started ones progress clearly.
        const step = t.id === 'dt-live-1' ? Math.max(1_500_000, Math.floor(t.bytes_total * 0.01)) : Math.max(256_000, Math.floor(t.bytes_total * 0.18));
        t.bytes_done = Math.min(t.bytes_total, t.bytes_done + step);
        t.rate = step * 3;
        changed = true;
        if (isFailUpload && t.direction === 'upload' && t.bytes_done >= Math.floor(t.bytes_total * 0.3)) {
          t.state = 'failed';
          t.rate = 0;
          t.finished = new Date().toISOString();
          t.error = { code: 'unreachable', message: 'Connection dropped.' };
          continue;
        }
        if (t.bytes_done >= t.bytes_total) {
          if (t._mockPendingUpload) {
            const { target, normDir, destPath, size, sampleMedia } = t._mockPendingUpload;
            delete t._mockPendingUpload;
            if (trees[target]?.has(normDir)) {
              const destList = trees[target].get(normDir);
              if (!destList.some((e) => e.path === destPath)) {
                destList.push(
                  makeEntry(destPath, {
                    size,
                    mtime: new Date().toISOString(),
                    sampleMedia,
                  }),
                );
              }
            }
          }
          t.state = 'done';
          t.rate = 0;
          t.finished = new Date().toISOString();
          t.hash = 'blake3:9d20a1f8c44e77b2109f3e6a11b2084d';
          t.canOpen = t.direction === 'download';
          if (t.openExternally && isNoHandler) {
            t.openError = {
              code: 'unsupported',
              reason: 'no_handler',
              message: 'No app on this phone can open this file type.',
              hint: 'Download the file to save it in Downloads instead.',
            };
          }
        }
      }

      // Advance background machine transfer jobs started by the user.
      for (const m of Object.values(machines)) {
        for (const job of m.jobs || []) {
          if (job.kind !== 'transfer' || job.state !== 'running' || !job._mockLive) continue;
          const total = job.progress?.bytes_total || 1_000_000;
          const step = Math.max(100_000, Math.floor(total * 0.16));
          job.progress.bytes_done = Math.min(total, (job.progress.bytes_done || 0) + step);
          job.progress.rate = step * 3;
          job.duration_s = (job.duration_s || 0) + 0.35;
          if (job.progress.bytes_done >= total) {
            if (job._mockPendingTransfer) {
              const pt = job._mockPendingTransfer;
              delete job._mockPendingTransfer;
              if (trees[pt.destMachine]?.has(pt.destParent)) {
                const destList = trees[pt.destMachine].get(pt.destParent);
                if (!destList.some((e) => e.path === pt.destPath)) {
                  destList.push(
                    makeEntry(pt.destPath, {
                      kind: pt.kind,
                      size: pt.totalBytes,
                      mtime: new Date().toISOString(),
                      palette: pt.palette,
                      sampleMedia: pt.sampleMedia,
                    }),
                  );
                }
              }
              if (pt.move && trees[pt.srcMachine]) {
                const srcParent = parentFilePath(pt.srcPath);
                const srcList = trees[pt.srcMachine].get(srcParent);
                if (srcList) {
                  const sIdx = srcList.findIndex((e) => e.path === pt.srcPath);
                  if (sIdx !== -1) srcList.splice(sIdx, 1);
                }
              }
            }
            job.state = 'succeeded';
            job.exit_code = 0;
            job.finished = new Date().toISOString();
            job.progress.files_done = job.progress.files_total;
            job.hash = 'blake3:5f12bc89e01a7c4420e9b3a17c60981f';
            job.logs += 'Transfer verified and finished.\n';
          }
        }
      }

      if (changed) emitTransfers();
      if (!hasActive()) {
        clearInterval(pumpTimer);
        pumpTimer = null;
      }
    }, 350);
  }

  let cachedVideoBlobUrl = null;
  let cachedAudioBlobUrl = null;
  let cachedUnplayableVideoBlobUrl = null;
  function getVideoUrl() {
    if (!cachedVideoBlobUrl) cachedVideoBlobUrl = base64ToBlobUrl(SAMPLE_VIDEO_WEBM_BASE64, 'video/webm');
    return cachedVideoBlobUrl;
  }
  function getAudioUrl() {
    if (!cachedAudioBlobUrl) cachedAudioBlobUrl = base64ToBlobUrl(SAMPLE_AUDIO_OGG_BASE64, 'audio/ogg');
    return cachedAudioBlobUrl;
  }
  function getUnplayableVideoUrl() {
    if (!cachedUnplayableVideoBlobUrl) {
      cachedUnplayableVideoBlobUrl = base64ToBlobUrl(SAMPLE_UNPLAYABLE_VIDEO_BASE64, 'video/mp4');
    }
    return cachedUnplayableVideoBlobUrl;
  }
  const thumbAttemptByPath = new Map();
  const mockStatusByUrl = new Map();
  const revokedMockUrls = new Set();
  let mockUrlSeq = 1;

  function normalizePath(p) {
    if (!p || p === '~') return '/home/me';
    if (p.startsWith('~/')) return `/home/me/${p.slice(2)}`.replace(/\/+$/, '') || '/home/me';
    if (p === '/') return '/';
    return p.replace(/\/+$/, '') || '/';
  }

  function formatEntry(entry, oldMachine = isOld) {
    const out = {
      name: entry.name,
      path: entry.path,
      kind: entry.kind,
      size: entry.size,
      mtime: entry.mtime,
      mode: entry.mode,
    };
    if (!oldMachine && entry.kind === 'file') {
      out.category = categoryFromName(entry.name);
      out.mime = mimeFromName(entry.name);
      if (entry.duration_s != null) out.duration_s = entry.duration_s;
    }
    return out;
  }

  function findEntryOnMachine(machineName, rawPath) {
    const target = normalizePath(rawPath);
    const tree = trees[machineName];
    if (!tree) return null;
    if (tree.has(target)) {
      return makeEntry(target, { kind: 'dir' });
    }
    const parent = parentFilePath(target);
    if (!parent) return null;
    const list = tree.get(parent);
    if (!list) return null;
    return list.find((e) => e.path === target) || null;
  }

  function collectAllUnder(machineName, rootPath) {
    const tree = trees[machineName];
    if (!tree) return [];
    const normRoot = normalizePath(rootPath);
    const results = [];
    const seen = new Set();
    const visit = (dir) => {
      const list = tree.get(dir);
      if (!list) return;
      for (const e of list) {
        if (seen.has(e.path)) continue;
        seen.add(e.path);
        results.push(e);
        if (e.kind === 'dir') visit(e.path);
      }
    };
    visit(normRoot);
    return results;
  }

  function handleTool(machineName, tool, args, onEvent) {
    const tree = trees[machineName];
    const oldMachine = isMachineOld(machineName);
    switch (tool) {
      case 'fs.list': {
        const requested = normalizePath(args?.path || '~');
        if (requested === '/home/me/Private' || requested.startsWith('/home/me/Private/')) {
          throw {
            code: 'denied',
            message: "You're not allowed to open this folder.",
            hint: 'Ask the machine owner to grant access in policy.toml.',
            machine: machineName,
          };
        }
        const rawEntries = tree?.get(requested);
        if (!rawEntries) {
          throw { code: 'not_found', message: `${requested} does not exist.`, hint: null, machine: machineName };
        }
        const includeHidden = args?.hidden !== false;
        let filtered = rawEntries.filter((e) => includeHidden || !isHiddenName(e.name));
        if (!oldMachine && Array.isArray(args?.categories) && args.categories.length > 0) {
          const catSet = new Set(args.categories);
          filtered = filtered.filter((e) => e.kind === 'dir' || catSet.has(categoryFromName(e.name)));
        }
        const sorted = !oldMachine
          ? sortEntries(filtered, { sort: args?.sort || 'name', order: args?.order || 'asc' })
          : sortEntries(filtered, { sort: 'name', order: 'asc' });

        const total = sorted.length;
        const offset = !oldMachine && Number.isInteger(args?.offset) && args.offset > 0 ? args.offset : 0;
        const limit = Number.isInteger(args?.limit) && args.limit > 0 ? args.limit : 500;
        const slice = sorted.slice(offset, offset + limit).map((e) => formatEntry(e, oldMachine));
        const truncated = offset + slice.length < total;
        const res = {
          path: requested,
          entries: slice,
          truncated,
        };
        if (!oldMachine) {
          res.total = total;
          res.total_exact = !(isInexact && requested === '/home/me/Archive5000');
          res.offset = offset;
        }
        return res;
      }

      case 'fs.read': {
        const requested = normalizePath(args?.path || '');
        const key = `${machineName}:${requested}`;
        let content = textFiles[key];
        if (content === undefined && requested.startsWith('/home/me/Archive5000/') && requested.endsWith('.txt')) {
          content = `Sample archive entry: ${requested}\n`;
        }
        if (content === undefined) {
          const entry = findEntryOnMachine(machineName, requested);
          if (entry && entry.kind === 'file') {
            const isSmallPng =
              categoryFromName(entry.name) === 'image' && entry.size <= 1048576;
            const b64 = isSmallPng ? SAMPLE_PNG_BASE64 : 'AAECAwQFBgcICQoLDA0ODw==';
            const eof = entry.size <= (Number.isInteger(args?.length) ? args.length : 1048576);
            return {
              path: requested,
              size: entry.size,
              offset: 0,
              length: isSmallPng ? entry.size : 16,
              encoding: 'base64',
              content: b64,
              eof,
            };
          }
          throw { code: 'not_found', message: `${requested} does not exist.`, hint: null, machine: machineName };
        }
        const offset = Number.isInteger(args?.offset) && args.offset >= 0 ? args.offset : 0;
        const maxLen = Number.isInteger(args?.length) && args.length >= 0 ? args.length : 1048576;
        const chunk = content.slice(offset, offset + maxLen);
        return {
          path: requested,
          size: content.length,
          offset,
          length: chunk.length,
          encoding: 'utf8',
          content: chunk,
          eof: offset + chunk.length >= content.length,
        };
      }

      case 'fs.stat': {
        const requested = normalizePath(args?.path || '~');
        const entry = findEntryOnMachine(machineName, requested);
        if (!entry) {
          throw { code: 'not_found', message: `${requested} does not exist.`, hint: null, machine: machineName };
        }
        return {
          ...formatEntry(entry, oldMachine),
          readonly: false,
        };
      }

      case 'fs.hash': {
        const requested = normalizePath(args?.path || '');
        const entry = findEntryOnMachine(machineName, requested);
        if (!entry || entry.kind !== 'file') {
          throw { code: 'not_found', message: `${requested} does not exist.`, hint: null, machine: machineName };
        }
        let h = 2166136261;
        for (let i = 0; i < requested.length; i += 1) {
          h = Math.imul(h ^ requested.charCodeAt(i), 16777619) >>> 0;
        }
        const hex = h.toString(16).padStart(8, '0').repeat(8);
        return {
          path: requested,
          size: entry.size,
          hash: `blake3:${hex}`,
        };
      }

      case 'fs.thumb': {
        if (oldMachine) {
          throw { code: 'unknown_tool', message: 'Unknown tool: fs.thumb', hint: null, machine: machineName };
        }
        const paths = Array.isArray(args?.paths) ? args.paths : [];
        if (paths.length === 0 || paths.length > 32) {
          throw {
            code: 'invalid_args',
            message: `fs.thumb accepts between 1 and 32 paths per call, and ${paths.length} were given.`,
            hint: 'Pass a non-empty list of up to 32 file paths.',
            machine: machineName,
          };
        }
        const results = [];
        for (const p of paths) {
          const norm = normalizePath(p);
          const entry = findEntryOnMachine(machineName, norm);
          if (!entry || entry.kind !== 'file') {
            results.push({ path: p, error: { code: 'not_found', message: 'File not found.' } });
            continue;
          }
          if (entry.thumbFail) {
            results.push({ path: p, error: { code: 'unsupported', message: 'Thumbnail decoding failed.' } });
            continue;
          }
          if (isThumbRetry && entry.name === 'IMG_2042.jpg') {
            const tries = thumbAttemptByPath.get(norm) || 0;
            thumbAttemptByPath.set(norm, tries + 1);
            if (tries < 1) {
              results.push({ path: p, error: { code: 'busy', message: 'Thumbnail worker busy.', retryable: true } });
              continue;
            }
          }
          const cat = categoryFromName(entry.name);
          if (cat !== 'image' && cat !== 'video' && !entry.palette) {
            results.push({ path: p, error: { code: 'unsupported', message: 'No thumbnail for this kind.' } });
            continue;
          }
          const pal = entry.palette || paletteFor(norm);
          results.push({
            path: p,
            ok: {
              mime: 'image/svg+xml',
              content: makeSampleImageBase64({ ...pal, w: 128, h: 128 }),
              width: 128,
              height: 128,
            },
          });
        }
        return { results };
      }

      case 'fs.find': {
        const requested = normalizePath(args?.path || '~');
        const all = collectAllUnder(machineName, requested);
        const includeHidden = Boolean(args?.hidden);
        let q = args?.name || '';
        if (!q && typeof args?.glob === 'string') {
          // Support both simple `*word*` globs and case-insensitive character-class globs like `*[nN][oO]*`.
          q = args.glob
            .replace(/\[([a-zA-Z])[a-zA-Z]\]/g, '$1')
            .replace(/[*?]/g, '');
        }
        const categories = Array.isArray(args?.categories) ? [...args.categories] : [];
        if (args?.kind === 'dir' && categories.length === 0) {
          categories.push('folder');
        }
        const matched = filterEntries(all, {
          hidden: includeHidden,
          query: q,
          categories,
          minSize: args?.min_size ?? null,
          maxSize: args?.max_size ?? null,
          modifiedAfter: args?.modified_after ?? null,
        });
        const sorted = sortEntries(matched, {
          sort: args?.sort || 'name',
          order: args?.order || 'asc',
        });
        const limit = Number.isInteger(args?.limit) && args.limit > 0 ? args.limit : 200;
        const slice = sorted.slice(0, limit).map((e) => formatEntry(e, oldMachine));
        const payload = {
          path: requested,
          entries: slice,
          matches: slice,
          truncated: sorted.length > slice.length,
          reason: sorted.length > slice.length ? 'limit' : undefined,
        };
        if (typeof onEvent === 'function') {
          onEvent({ kind: 'find_machine', machine: machineName, data: payload });
        }
        return payload;
      }

      case 'fs.mkdir': {
        const target = normalizePath(args?.path || '');
        if (!target || target === '/') {
          throw { code: 'invalid_args', message: 'Enter a folder name.', hint: null, machine: machineName };
        }
        const parent = parentFilePath(target);
        const list = tree?.get(parent);
        if (!list) {
          throw { code: 'not_found', message: `Parent folder ${parent} does not exist.`, hint: null, machine: machineName };
        }
        const existing = list.find((e) => e.path === target);
        if (existing) {
          return { path: target, created: false };
        }
        list.push(makeEntry(target, { kind: 'dir', mtime: new Date().toISOString() }));
        tree.set(target, []);
        return { path: target, created: true };
      }

      case 'fs.copy': {
        const from = normalizePath(args?.from || '');
        const to = normalizePath(args?.to || '');
        const fromParent = parentFilePath(from);
        const toParent = parentFilePath(to);
        const fromList = tree?.get(fromParent);
        const toList = tree?.get(toParent);
        if (!fromList || !toList) {
          throw { code: 'not_found', message: 'Source or destination folder does not exist.', hint: null, machine: machineName };
        }
        const item = fromList.find((e) => e.path === from);
        if (!item) {
          throw { code: 'not_found', message: `${from} does not exist.`, hint: null, machine: machineName };
        }
        if (toList.some((e) => e.path === to) && !args?.overwrite) {
          throw { code: 'conflict', message: `${baseFileName(to)} already exists.`, hint: 'Choose a different name.', machine: machineName };
        }
        const copied = { ...item, name: baseFileName(to), path: to, mtime: new Date().toISOString() };
        const existingIdx = toList.findIndex((e) => e.path === to);
        if (existingIdx >= 0) toList[existingIdx] = copied;
        else toList.push(copied);
        if (item.kind === 'dir' && tree.has(from)) {
          const children = tree.get(from);
          tree.set(
            to,
            children.map((c) => ({ ...c, path: joinFilePath(to, c.name) })),
          );
        }
        const oldKey = `${machineName}:${from}`;
        if (Object.prototype.hasOwnProperty.call(textFiles, oldKey)) {
          textFiles[`${machineName}:${to}`] = textFiles[oldKey];
        }
        return { from, to };
      }

      case 'fs.move': {
        const from = normalizePath(args?.from || '');
        const to = normalizePath(args?.to || '');
        const fromParent = parentFilePath(from);
        const toParent = parentFilePath(to);
        const fromList = tree?.get(fromParent);
        const toList = tree?.get(toParent);
        if (!fromList || !toList) {
          throw { code: 'not_found', message: 'Source or destination folder does not exist.', hint: null, machine: machineName };
        }
        const idx = fromList.findIndex((e) => e.path === from);
        if (idx === -1) {
          throw { code: 'not_found', message: `${from} does not exist.`, hint: null, machine: machineName };
        }
        if (toList.some((e) => e.path === to) && !args?.overwrite) {
          throw { code: 'conflict', message: `${baseFileName(to)} already exists.`, hint: 'Choose a different name.', machine: machineName };
        }
        const [item] = fromList.splice(idx, 1);
        const updated = { ...item, name: baseFileName(to), path: to, mtime: new Date().toISOString() };
        toList.push(updated);
        if (item.kind === 'dir' && tree.has(from)) {
          const children = tree.get(from);
          tree.delete(from);
          tree.set(
            to,
            children.map((c) => ({ ...c, path: joinFilePath(to, c.name) })),
          );
        }
        const oldKey = `${machineName}:${from}`;
        if (Object.prototype.hasOwnProperty.call(textFiles, oldKey)) {
          textFiles[`${machineName}:${to}`] = textFiles[oldKey];
          delete textFiles[oldKey];
        }
        // Update matching favourites path if any.
        favourites = favourites.map((f) =>
          f.machine === machineName && f.path === from ? { ...f, path: to, label: updated.name } : f,
        );
        return { from, to };
      }

      case 'fs.remove': {
        const target = normalizePath(args?.path || '');
        const parent = parentFilePath(target);
        const list = tree?.get(parent);
        const idx = list ? list.findIndex((e) => e.path === target) : -1;
        if (idx === -1) {
          throw { code: 'not_found', message: `${target} does not exist.`, hint: null, machine: machineName };
        }
        if (args?.dry_run) {
          return { path: target, removed: 1, dry_run: true };
        }
        list.splice(idx, 1);
        tree.delete(target);
        delete textFiles[`${machineName}:${target}`];
        favourites = favourites.filter((f) => !(f.machine === machineName && f.path === target));
        return { path: target, removed: 1 };
      }

      case 'fs.transfer': {
        const fromRaw = String(args?.from || '');
        const toRaw = String(args?.to || '');
        if (!fromRaw || !toRaw) {
          throw { code: 'invalid_args', message: 'Both source and destination are required.', hint: null, machine: machineName };
        }
        const toColon = toRaw.indexOf(':');
        const destMachine = toColon > 1 ? toRaw.slice(0, toColon) : machineName;
        const destPath = normalizePath(toColon > 1 ? toRaw.slice(toColon + 1) : toRaw);
        const fromColon = fromRaw.indexOf(':');
        const srcMachine = fromColon > 1 ? fromRaw.slice(0, fromColon) : machineName;
        const srcPath = normalizePath(fromColon > 1 ? fromRaw.slice(fromColon + 1) : fromRaw);

        if (machines[destMachine] && !machines[destMachine].reachable) {
          throw machines[destMachine].error;
        }
        const srcEntry = findEntryOnMachine(srcMachine, srcPath);
        const totalBytes = srcEntry?.size || 2_400_000;
        const baseName = baseFileName(srcPath);
        const verb = args?.move ? 'Moving' : 'Copying';
        const title = `${verb} ${baseName} to ${destMachine}`;

        const destParent = parentFilePath(destPath);
        if (!oldMachine && args?.background && !args?.dry_run) {
          const id = `j-xfer-${transferSeq++}`;
          const job = {
            id,
            kind: 'transfer',
            name: title,
            title,
            state: 'running',
            created: new Date().toISOString(),
            command: `fs.transfer ${args?.move ? '--move ' : ''}${srcPath} ${destMachine}:${destPath}`,
            from: srcPath,
            to: `${destMachine}:${destPath}`,
            move: Boolean(args?.move),
            duration_s: 0.2,
            progress: {
              bytes_done: Math.max(64_000, Math.floor(totalBytes * 0.12)),
              bytes_total: totalBytes,
              files_done: 0,
              files_total: 1,
              current: baseName,
              rate: Math.max(512_000, Math.floor(totalBytes * 0.4)),
            },
            logs: `Started ${title}\n`,
            _mockLive: true,
            _mockPendingTransfer: {
              srcMachine,
              srcPath,
              destMachine,
              destPath,
              destParent,
              move: Boolean(args?.move),
              kind: srcEntry?.kind || 'file',
              totalBytes,
              palette: srcEntry?.palette || null,
              sampleMedia: srcEntry?.sampleMedia || null,
            },
          };
          machines[machineName].jobs.unshift(job);
          ensureTransferPump();
          return { job: { id, machine: machineName }, title };
        }

        // Synchronous transfer: apply destination mutation immediately.
        if (!args?.dry_run && trees[destMachine]?.has(destParent)) {
          const destList = trees[destMachine].get(destParent);
          if (!destList.some((e) => e.path === destPath)) {
            destList.push(
              makeEntry(destPath, {
                kind: srcEntry?.kind || 'file',
                size: totalBytes,
                mtime: new Date().toISOString(),
                palette: srcEntry?.palette || null,
                sampleMedia: srcEntry?.sampleMedia || null,
              }),
            );
          }
          if (args?.move && trees[srcMachine]) {
            const srcParent = parentFilePath(srcPath);
            const srcList = trees[srcMachine].get(srcParent);
            if (srcList) {
              const sIdx = srcList.findIndex((e) => e.path === srcPath);
              if (sIdx !== -1) srcList.splice(sIdx, 1);
            }
          }
        }

        return {
          transferred: !args?.dry_run,
          dry_run: Boolean(args?.dry_run) || undefined,
          from: fromRaw,
          to: toRaw,
          bytes: totalBytes,
          move: Boolean(args?.move) || undefined,
        };
      }

      case 'fav.list': {
        if (isOld) {
          throw { code: 'unknown_tool', message: 'Unknown tool: fav.list', hint: null, machine: machineName };
        }
        let list = favourites;
        if (args?.machine) list = list.filter((f) => f.machine === args.machine);
        if (typeof args?.pinned === 'boolean') list = list.filter((f) => Boolean(f.pinned) === args.pinned);
        return { entries: list.map((f) => ({ ...f })) };
      }

      case 'fav.add': {
        if (isOld) {
          throw { code: 'unknown_tool', message: 'Unknown tool: fav.add', hint: null, machine: machineName };
        }
        const mName = args?.machine || machineName;
        const norm = normalizePath(args?.path || '');
        const existing = favourites.find((f) => f.machine === mName && f.path === norm);
        if (existing) {
          if (typeof args?.pinned === 'boolean') existing.pinned = args.pinned;
          if (args?.label) existing.label = args.label;
          return { entry: { ...existing } };
        }
        const entry = findEntryOnMachine(mName, norm);
        const kind = entry?.kind || 'file';
        const created = {
          id: `fav-${favCounter++}`,
          machine: mName,
          path: norm,
          kind,
          category: kind === 'dir' ? null : categoryFromName(norm),
          label: args?.label || baseFileName(norm) || norm,
          pinned: Boolean(args?.pinned),
          exists: Boolean(entry),
          added_at: new Date().toISOString(),
          added_by: 'phone',
        };
        favourites = [created, ...favourites];
        return { entry: { ...created } };
      }

      case 'fav.set': {
        if (isOld) {
          throw { code: 'unknown_tool', message: 'Unknown tool: fav.set', hint: null, machine: machineName };
        }
        const item = favourites.find((f) => f.id === args?.id);
        if (!item) {
          throw { code: 'not_found', message: `Favourite ${args?.id} not found.`, hint: null, machine: machineName };
        }
        if (typeof args?.pinned === 'boolean') item.pinned = args.pinned;
        if (typeof args?.label === 'string' && args.label.trim()) item.label = args.label.trim();
        return { entry: { ...item } };
      }

      case 'fav.remove': {
        if (isOld) {
          throw { code: 'unknown_tool', message: 'Unknown tool: fav.remove', hint: null, machine: machineName };
        }
        favourites = favourites.filter((f) => f.id !== args?.id);
        return { removed: true, id: args?.id };
      }

      case 'sync.list': {
        if (isNoSync) {
          throw { code: 'unknown_tool', message: 'Unknown tool: sync.list', hint: null, machine: machineName };
        }
        return {
          folders: syncFolders.map((f) => ({
            id: f.id,
            name: f.name,
            file_count: f.file_count,
            total_bytes: f.total_bytes,
            paused: f.paused,
            summary: f.paused ? 'Paused' : f.summary,
            members: f.members.map((mem) => ({ ...mem })),
            conflicts_count: f.conflicts.length,
            bin_count: f.bin.length,
          })),
        };
      }

      case 'sync.status': {
        if (isNoSync) {
          throw { code: 'unknown_tool', message: 'Unknown tool: sync.status', hint: null, machine: machineName };
        }
        const folder = syncFolders.find((f) => f.id === args?.id);
        if (!folder) {
          throw { code: 'not_found', message: `Sync folder ${args?.id} not found.`, hint: null, machine: machineName };
        }
        return {
          ...folder,
          summary: folder.paused ? 'Paused' : folder.summary,
          members: folder.members.map((mem) => ({
            ...mem,
            state: folder.paused ? 'paused' : mem.state,
            sentence: folder.paused ? 'Paused' : mem.sentence,
          })),
          conflicts_count: folder.conflicts.length,
          bin_count: folder.bin.length,
        };
      }

      case 'sync.pause':
      case 'sync.resume': {
        const folder = syncFolders.find((f) => f.id === args?.id);
        if (!folder) throw { code: 'not_found', message: `Sync folder ${args?.id} not found.`, hint: null, machine: machineName };
        folder.paused = tool === 'sync.pause';
        return { id: folder.id, paused: folder.paused };
      }

      case 'sync.conflicts': {
        const folder = syncFolders.find((f) => f.id === args?.id);
        if (!folder) throw { code: 'not_found', message: `Sync folder ${args?.id} not found.`, hint: null, machine: machineName };
        return { id: folder.id, conflicts: folder.conflicts.map((c) => ({ ...c })) };
      }

      case 'sync.resolve': {
        const folder = syncFolders.find((f) => f.id === args?.id);
        if (!folder) throw { code: 'not_found', message: `Sync folder ${args?.id} not found.`, hint: null, machine: machineName };
        folder.conflicts = folder.conflicts.filter((c) => c.path !== args?.path);
        return { id: folder.id, resolved: true, path: args?.path };
      }

      case 'sync.bin': {
        const folder = syncFolders.find((f) => f.id === args?.id);
        if (!folder) throw { code: 'not_found', message: `Sync folder ${args?.id} not found.`, hint: null, machine: machineName };
        return { id: folder.id, entries: folder.bin.map((b) => ({ ...b })) };
      }

      case 'sync.restore': {
        const folder = syncFolders.find((f) => f.id === args?.id);
        if (!folder) throw { code: 'not_found', message: `Sync folder ${args?.id} not found.`, hint: null, machine: machineName };
        folder.bin = folder.bin.filter((b) => b.path !== args?.path);
        return { id: folder.id, restored: true, path: args?.path };
      }

      case 'sync.add': {
        const folder = syncFolders.find((f) => f.id === args?.id);
        if (!folder) throw { code: 'not_found', message: `Sync folder ${args?.id} not found.`, hint: null, machine: machineName };
        if (!folder.members.some((m) => m.machine === args?.machine)) {
          folder.members.push({
            machine: args.machine,
            path: normalizePath(args.path || `/home/me/${folder.name}`),
            state: machines[args.machine]?.reachable ? 'up_to_date' : 'waiting',
            sentence: machines[args.machine]?.reachable ? 'Up to date' : 'Off · waiting for it to answer',
          });
        }
        return { id: folder.id, members: folder.members.map((m) => ({ ...m })) };
      }

      case 'sync.remove': {
        const folder = syncFolders.find((f) => f.id === args?.id);
        if (!folder) throw { code: 'not_found', message: `Sync folder ${args?.id} not found.`, hint: null, machine: machineName };
        folder.members = folder.members.filter((m) => m.machine !== args?.machine);
        return { id: folder.id, removed: true };
      }

      case 'sync.delete': {
        syncFolders = syncFolders.filter((f) => f.id !== args?.id);
        return { id: args?.id, deleted: true };
      }

      case 'sync.now':
        return { id: args?.id, triggered: true };

      default:
        return undefined;
    }
  }

  // Optional link methods (omitted completely when `?mock=old`).
  const optionalLinkMethods = isOld
    ? {}
    : {
        async contentUrl(options = {}) {
          const { machine, path, signal } = options || {};
          if (signal?.aborted) {
            throw { code: 'cancelled', message: 'Loading was stopped.', hint: null, machine: machine || null };
          }
          if (!isAuthedRef()) {
            throw {
              code: 'denied',
              message: 'This browser does not have a token yet.',
              hint: null,
              machine: null,
              status: 401,
            };
          }
          if (typeof path !== 'string' || path.trim() === '') {
            throw { code: 'invalid_args', message: 'Choose a file path first.', hint: null, machine: machine || null };
          }
          const target = machine || activeNodeRef();
          const m = machines[target];
          if (!m) {
            throw { code: 'not_found', message: `There is no machine called ${target} in fleet.toml.`, hint: null, machine: target };
          }
          if (!m.reachable) {
            throw m.error || { code: 'unreachable', message: `${target} is off.`, hint: null, machine: target };
          }
          if (!canReadRef(target)) {
            throw { code: 'denied', message: 'Not allowed to read files on this machine.', hint: null, machine: target };
          }
          if (isMachineOld(target)) {
            throw {
              code: 'unsupported',
              message: `${target} does not support content streaming yet.`,
              hint: null,
              machine: target,
            };
          }
          const norm = normalizePath(path);
          const entry = findEntryOnMachine(target, norm);
          if (entry?.mockMintError) {
            throw { machine: target, ...entry.mockMintError };
          }
          const name = entry?.name || baseFileName(norm);
          const ext = name.includes('.') ? name.slice(name.lastIndexOf('.') + 1).toLowerCase() : '';

          const cat = categoryFromName(name);
          // Unplayable video/audio/image is driven strictly by payload (`sampleMedia === 'unplayable'`), never by extension.
          if (entry?.sampleMedia === 'unplayable') {
            const url =
              cat === 'image'
                ? `data:image/png;base64,AA==#mock-${mockUrlSeq++}`
                : `${getUnplayableVideoUrl()}#mock-${mockUrlSeq++}`;
            if (entry.mockContentStatus) {
              mockStatusByUrl.set(url, { ...entry.mockContentStatus });
            } else {
              mockStatusByUrl.set(url, {
                request_count: 1,
                status: 206,
                content_type: mimeFromName(name),
                stream_failed: false,
                ticket_live: true,
              });
            }
            return {
              url,
              expires_in: 900,
            };
          }
          if (cat === 'video') {
            return { url: `${getVideoUrl()}#mock-${mockUrlSeq++}`, expires_in: 900 };
          }
          if (cat === 'audio') {
            return { url: `${getAudioUrl()}#mock-${mockUrlSeq++}`, expires_in: 900 };
          }
          if (cat === 'image') {
            if (entry) {
              const pal = entry.palette || paletteFor(norm);
              const url = makeSampleImageDataUrl({ ...pal, w: 720, h: 540 });
              revokedMockUrls.delete(url);
              mockStatusByUrl.delete(url);
              return {
                url,
                expires_in: 900,
              };
            }
            if (ext === 'svg') {
              const url =
                'data:image/svg+xml;charset=utf-8,' +
                encodeURIComponent('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" fill="#3b6f7a"/></svg>');
              revokedMockUrls.delete(url);
              mockStatusByUrl.delete(url);
              return {
                url,
                expires_in: 900,
              };
            }
            const url = `data:image/png;base64,${SAMPLE_PNG_BASE64}`;
            revokedMockUrls.delete(url);
            mockStatusByUrl.delete(url);
            return { url, expires_in: 900 };
          }
          if (ext === 'vtt') {
            return {
              url: `data:text/vtt;charset=utf-8,${encodeURIComponent('WEBVTT\n\n00:00.000 --> 00:01.000\nMock caption\n')}`,
              expires_in: 900,
            };
          }
          if (ext === 'pdf') {
            return { url: `data:application/pdf;base64,${SAMPLE_PDF_BASE64}`, expires_in: 900 };
          }
          if (cat === 'text' || ext === 'html' || ext === 'htm') {
            const text = textFiles[`${target}:${norm}`] || `Mock preview for ${norm}\n`;
            return { url: `data:text/plain;charset=utf-8,${encodeURIComponent(text)}`, expires_in: 900 };
          }
          return {
            url: 'data:application/octet-stream;base64,TGVnaW9uIG1vY2sgZmlsZQ==',
            expires_in: 900,
          };
        },

        async download({ machine, path, name, size, mime }) {
          const target = machine || activeNodeRef();
          const norm = normalizePath(path);
          const entry = findEntryOnMachine(target, norm);
          const id = `dt-${transferSeq++}`;
          const fileName = name || baseFileName(norm);
          const total = size || entry?.size || 2_400_000;
          const activeCount = deviceTransfers.filter((t) => t.state === 'running').length;
          deviceTransfers.unshift({
            id,
            direction: 'download',
            machine: target,
            path: norm,
            name: fileName,
            localPath: `Downloads/Legion/${fileName}`,
            mime: mime || mimeFromName(fileName),
            state: activeCount < 2 ? 'running' : 'queued',
            bytes_done: 0,
            bytes_total: total,
            rate: 0,
            error: null,
            created: new Date().toISOString(),
            finished: null,
            hash: null,
            canOpen: false,
          });
          emitTransfers();
          ensureTransferPump();
          return { id };
        },

        async upload({ machine, dir, files }) {
          if (isSlowUpload) {
            await new Promise((resolve) => setTimeout(resolve, 450));
          }
          const target = machine || activeNodeRef();
          const normDir = normalizePath(dir || '~');
          const picked =
            Array.isArray(files) && files.length > 0
              ? files
              : [{ name: `Upload-${transferSeq}.jpg`, size: 3_200_000 }];
          const ids = [];
          for (const f of picked) {
            const id = `dt-${transferSeq++}`;
            const fileName = f.name || `Upload-${transferSeq}.bin`;
            const destPath = joinFilePath(normDir, fileName);
            const activeCount = deviceTransfers.filter((t) => t.state === 'running').length;
            deviceTransfers.unshift({
              id,
              direction: 'upload',
              machine: target,
              path: destPath,
              name: fileName,
              localPath: f.path || `Device/${fileName}`,
              mime: mimeFromName(fileName),
              state: activeCount < 2 ? 'running' : 'queued',
              bytes_done: 0,
              bytes_total: f.size || 2_000_000,
              rate: 0,
              error: null,
              created: new Date().toISOString(),
              finished: null,
              hash: null,
              canOpen: false,
              _mockPendingUpload: {
                target,
                normDir,
                destPath,
                size: f.size || 2_000_000,
                sampleMedia: categoryFromName(fileName) === 'image' ? 'image' : null,
              },
            });
            ids.push(id);
          }
          emitTransfers();
          ensureTransferPump();
          return { id: ids[0], ids, count: ids.length };
        },

        async contentStatus(url) {
          if (revokedMockUrls.has(url)) {
            return {
              request_count: 0,
              status: 404,
              content_type: null,
              stream_failed: false,
              ticket_live: false,
            };
          }
          if (mockStatusByUrl.has(url)) {
            return { ...mockStatusByUrl.get(url) };
          }
          return {
            request_count: 1,
            status: 200,
            content_type: 'application/octet-stream',
            stream_failed: false,
            ticket_live: true,
          };
        },

        async revokeContentUrl(url) {
          const key = String(url || '').trim();
          if (!key) return false;
          const wasFresh = !revokedMockUrls.has(key);
          revokedMockUrls.add(key);
          if (typeof window !== 'undefined') {
            if (!Array.isArray(window.__LEGION_MOCK_REVOKED_URLS__)) {
              window.__LEGION_MOCK_REVOKED_URLS__ = [];
            }
            window.__LEGION_MOCK_REVOKED_URLS__.push(key);
          }
          const existing = mockStatusByUrl.get(key);
          if (existing) {
            mockStatusByUrl.set(key, { ...existing, status: 404, ticket_live: false });
          }
          return wasFresh;
        },

        async openExternally({ machine, path, name, size, mime }) {
          const target = machine || activeNodeRef();
          const norm = normalizePath(path);
          const entry = findEntryOnMachine(target, norm);
          const fileName = name || baseFileName(norm);
          if (!canOpenExternally(fileName)) {
            throw {
              code: 'denied',
              message: 'Legion refuses to open this file type directly.',
              hint: 'Download it first if you want to inspect it.',
              machine: target,
            };
          }
          const active = deviceTransfers.find(
            (t) =>
              t.openExternally &&
              t.direction === 'download' &&
              t.machine === target &&
              t.path === norm &&
              (t.state === 'queued' || t.state === 'running'),
          );
          if (active) {
            return { id: active.id, ok: true, openedImmediately: false };
          }
          const cached = deviceTransfers.find(
            (t) =>
              t.openExternally &&
              t.direction === 'download' &&
              t.machine === target &&
              t.path === norm &&
              t.state === 'done',
          );
          if (cached) {
            if (isNoHandler) {
              throw {
                code: 'unsupported',
                reason: 'no_handler',
                message: 'No app on this phone can open this file type.',
                hint: 'Download the file to save it in Downloads instead.',
                machine: target,
              };
            }
            return { id: cached.id, ok: true, openedImmediately: true };
          }
          const id = `dt-${transferSeq++}`;
          const total = size || entry?.size || 2_400_000;
          const activeCount = deviceTransfers.filter((t) => t.state === 'running').length;
          deviceTransfers.unshift({
            id,
            direction: 'download',
            machine: target,
            path: norm,
            name: fileName,
            localPath: `cache/open-externally/${id}-${fileName}`,
            mime: mime || mimeFromName(fileName),
            state: activeCount < 2 ? 'running' : 'queued',
            bytes_done: 0,
            bytes_total: total,
            rate: 0,
            error: null,
            openError: null,
            openExternally: true,
            created: new Date().toISOString(),
            finished: null,
            hash: null,
            canOpen: false,
          });
          emitTransfers();
          ensureTransferPump();
          return { id, ok: true, openedImmediately: false };
        },

        async transfers() {
          if (isEmpty) return [];
          if (isTransfersDemo) ensureTransferPump();
          return snapshotTransfers();
        },

        async transferControl(id, action) {
          const item = deviceTransfers.find((t) => t.id === id);
          if (!item) {
            throw { code: 'not_found', message: `Transfer ${id} not found.`, hint: null, machine: null };
          }
          if (action === 'pause') {
            if (item.state === 'running' || item.state === 'queued') {
              item.state = 'paused';
              item.rate = 0;
            }
          } else if (action === 'resume') {
            if (item.state === 'paused' || item.state === 'failed') {
              item.state = 'running';
              item.error = null;
              ensureTransferPump();
            }
          } else if (action === 'cancel') {
            delete item._mockPendingUpload;
            item.state = 'cancelled';
            item.rate = 0;
            item.finished = new Date().toISOString();
          }
          emitTransfers();
          return { ...item };
        },

        async openTransfer(id) {
          const item = deviceTransfers.find((t) => t.id === id);
          if (!item || item.state !== 'done') {
            throw { code: 'invalid_state', message: 'This download is not finished yet.', hint: null, machine: null };
          }
          return { ok: true, viewer: { machine: item.machine, path: item.path } };
        },

        onTransferEvent(listener) {
          if (typeof listener !== 'function') return () => {};
          transferListeners.add(listener);
          return () => {
            transferListeners.delete(listener);
          };
        },
      };

  if (typeof window !== 'undefined') {
    window.__LEGION_MOCK__ = {
      addExternalFile(machineName, dirPath, name, opts = {}) {
        const normDir = normalizePath(dirPath || '~');
        const list = trees[machineName]?.get(normDir);
        if (!list) return null;
        const full = joinFilePath(normDir, name);
        if (list.some((e) => e.path === full)) return full;
        list.push(
          makeEntry(full, {
            kind: opts.kind || 'file',
            size: opts.size ?? 4096,
            mtime: opts.mtime || new Date().toISOString(),
            sampleMedia: opts.sampleMedia || null,
          }),
        );
        if (opts.kind === 'dir' && !trees[machineName].has(full)) {
          trees[machineName].set(full, []);
        }
        return full;
      },
      addExternalJob(machineName, job) {
        const m = machines[machineName];
        if (!m) return null;
        const entry = {
          id: job.id || `j-ext-${Date.now()}`,
          kind: job.kind || 'exec',
          name: job.name || 'External task',
          state: job.state || 'succeeded',
          created: job.created || new Date().toISOString(),
          finished: job.finished || new Date().toISOString(),
          exit_code: job.exit_code ?? 0,
          command: job.command || job.name || 'echo ok',
          ...job,
        };
        m.jobs.unshift(entry);
        return entry;
      },
      simulateUploadTransition(machineName, dirPath, name, states) {
        const normDir = normalizePath(dirPath || '~');
        const destPath = joinFilePath(normDir, name);
        const id = `dt-sim-${transferSeq++}`;
        const item = {
          id,
          direction: 'upload',
          machine: machineName,
          path: destPath,
          name,
          localPath: `Device/${name}`,
          mime: mimeFromName(name),
          state: 'queued',
          bytes_done: 0,
          bytes_total: 1024,
          rate: 0,
          error: null,
          created: new Date().toISOString(),
          finished: null,
          hash: null,
          canOpen: false,
        };
        deviceTransfers.unshift(item);
        for (const st of states || []) {
          item.state = st;
          if (st === 'done') {
            item.bytes_done = item.bytes_total;
            item.finished = new Date().toISOString();
            const list = trees[machineName]?.get(normDir);
            if (list && !list.some((e) => e.path === destPath)) {
              list.push(makeEntry(destPath, { size: item.bytes_total, mtime: item.finished }));
            }
          } else if (st === 'cancelled' || st === 'failed') {
            item.finished = new Date().toISOString();
          }
          emitTransfers();
        }
        return id;
      },
    };
  }

  return {
    isOld,
    isNoSync,
    handleTool,
    optionalLinkMethods,
  };
}
