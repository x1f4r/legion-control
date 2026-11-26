// Pure helpers for Files: categories, client-side degrade rules, sorting, selection, paths, and history.

import { computeFileCapabilities } from './capabilities.js';
import { routeHref } from './route.js';
import { thumbCacheKey } from './thumb-cache.js';

export const FILE_CATEGORIES = ['image', 'video', 'audio', 'document', 'text', 'archive', 'other'];

// Generated from `crates/legion-core/src/media.rs` (`node scripts/sync-media-table.mjs`).
// Each entry is `[category, mime, viewerMode, openExternallyAllowed]`.
// BEGIN_MEDIA_TABLE
const MEDIA_TABLE = {
  jpg: ['image', 'image/jpeg', 'inline', true],
  jpeg: ['image', 'image/jpeg', 'inline', true],
  jpe: ['image', 'image/jpeg', 'inline', true],
  jfif: ['image', 'image/jpeg', 'inline', true],
  png: ['image', 'image/png', 'inline', true],
  gif: ['image', 'image/gif', 'inline', true],
  webp: ['image', 'image/webp', 'inline', true],
  avif: ['image', 'image/avif', 'inline', true],
  bmp: ['image', 'image/bmp', 'inline', true],
  dib: ['image', 'image/bmp', 'inline', true],
  ico: ['image', 'image/x-icon', 'inline', true],
  cur: ['image', 'image/x-icon', 'inline', true],
  svg: ['image', 'image/svg+xml', 'inline', false],
  heic: ['image', 'image/heic', 'thumb_or_fallback', true],
  heif: ['image', 'image/heif', 'thumb_or_fallback', true],
  tif: ['image', 'image/tiff', 'thumb_or_fallback', true],
  tiff: ['image', 'image/tiff', 'thumb_or_fallback', true],
  jxl: ['image', 'image/jxl', 'thumb_or_fallback', true],
  psd: ['image', 'image/vnd.adobe.photoshop', 'thumb_or_fallback', false],
  dng: ['image', 'image/x-adobe-dng', 'thumb_or_fallback', true],
  cr2: ['image', 'image/x-canon-cr2', 'thumb_or_fallback', true],
  cr3: ['image', 'image/x-canon-cr3', 'thumb_or_fallback', true],
  nef: ['image', 'image/x-nikon-nef', 'thumb_or_fallback', true],
  arw: ['image', 'image/x-sony-arw', 'thumb_or_fallback', true],
  raf: ['image', 'image/x-fuji-raf', 'thumb_or_fallback', true],
  orf: ['image', 'image/x-olympus-orf', 'thumb_or_fallback', true],
  rw2: ['image', 'image/x-panasonic-rw2', 'thumb_or_fallback', true],
  pef: ['image', 'image/x-pentax-pef', 'thumb_or_fallback', true],
  srw: ['image', 'image/x-samsung-srw', 'thumb_or_fallback', true],
  dcr: ['image', 'image/x-kodak-dcr', 'thumb_or_fallback', true],
  kdc: ['image', 'image/x-kodak-kdc', 'thumb_or_fallback', true],
  mrw: ['image', 'image/x-minolta-mrw', 'thumb_or_fallback', true],
  x3f: ['image', 'image/x-sigma-x3f', 'thumb_or_fallback', true],
  tga: ['image', 'image/x-tga', 'thumb_or_fallback', false],
  ppm: ['image', 'image/x-portable-pixmap', 'thumb_or_fallback', false],
  pgm: ['image', 'image/x-portable-graymap', 'thumb_or_fallback', false],
  pbm: ['image', 'image/x-portable-bitmap', 'thumb_or_fallback', false],
  pnm: ['image', 'image/x-portable-anymap', 'thumb_or_fallback', false],
  qoi: ['image', 'image/qoi', 'thumb_or_fallback', false],
  dds: ['image', 'image/vnd.ms-dds', 'thumb_or_fallback', false],
  exr: ['image', 'image/x-exr', 'thumb_or_fallback', false],
  hdr: ['image', 'image/vnd.radiance', 'thumb_or_fallback', false],
  mp4: ['video', 'video/mp4', 'inline', true],
  m4v: ['video', 'video/x-m4v', 'inline', true],
  mov: ['video', 'video/quicktime', 'inline', true],
  qt: ['video', 'video/quicktime', 'inline', true],
  webm: ['video', 'video/webm', 'inline', true],
  mkv: ['video', 'video/x-matroska', 'inline', true],
  '3gp': ['video', 'video/3gpp', 'inline', true],
  '3g2': ['video', 'video/3gpp2', 'inline', true],
  ogv: ['video', 'video/ogg', 'inline', true],
  mpg: ['video', 'video/mpeg', 'inline', true],
  mpeg: ['video', 'video/mpeg', 'inline', true],
  mpe: ['video', 'video/mpeg', 'inline', true],
  mts: ['video', 'video/mp2t', 'inline', true],
  m2ts: ['video', 'video/mp2t', 'inline', true],
  avi: ['video', 'video/x-msvideo', 'fallback', true],
  wmv: ['video', 'video/x-ms-wmv', 'fallback', true],
  flv: ['video', 'video/x-flv', 'fallback', true],
  asf: ['video', 'video/x-ms-asf', 'fallback', true],
  rm: ['video', 'video/vnd.rn-realvideo', 'fallback', true],
  rmvb: ['video', 'video/vnd.rn-realvideo', 'fallback', true],
  vob: ['video', 'video/dvd', 'fallback', true],
  flac: ['audio', 'audio/flac', 'inline', true],
  mp3: ['audio', 'audio/mpeg', 'inline', true],
  mpga: ['audio', 'audio/mpeg', 'inline', true],
  m4a: ['audio', 'audio/mp4', 'inline', true],
  m4b: ['audio', 'audio/mp4', 'inline', true],
  mp4a: ['audio', 'audio/mp4', 'inline', true],
  alac: ['audio', 'audio/mp4', 'inline', true],
  aac: ['audio', 'audio/aac', 'inline', true],
  ogg: ['audio', 'audio/ogg', 'inline', true],
  oga: ['audio', 'audio/ogg', 'inline', true],
  spx: ['audio', 'audio/ogg', 'inline', true],
  opus: ['audio', 'audio/opus', 'inline', true],
  wav: ['audio', 'audio/wav', 'inline', true],
  wave: ['audio', 'audio/wav', 'inline', true],
  weba: ['audio', 'audio/webm', 'inline', true],
  aif: ['audio', 'audio/aiff', 'inline', true],
  aiff: ['audio', 'audio/aiff', 'inline', true],
  aifc: ['audio', 'audio/aiff', 'inline', true],
  mka: ['audio', 'audio/x-matroska', 'inline', true],
  amr: ['audio', 'audio/amr', 'inline', true],
  wma: ['audio', 'audio/x-ms-wma', 'fallback', true],
  mid: ['audio', 'audio/midi', 'fallback', true],
  midi: ['audio', 'audio/midi', 'fallback', true],
  ape: ['audio', 'audio/x-ape', 'fallback', true],
  wv: ['audio', 'audio/x-wavpack', 'fallback', true],
  caf: ['audio', 'audio/x-caf', 'fallback', true],
  pdf: ['document', 'application/pdf', 'fallback', true],
  epub: ['document', 'application/epub+zip', 'fallback', true],
  docx: ['document', 'application/vnd.openxmlformats-officedocument.wordprocessingml.document', 'fallback', true],
  xlsx: ['document', 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet', 'fallback', true],
  pptx: ['document', 'application/vnd.openxmlformats-officedocument.presentationml.presentation', 'fallback', true],
  odt: ['document', 'application/vnd.oasis.opendocument.text', 'fallback', true],
  ods: ['document', 'application/vnd.oasis.opendocument.spreadsheet', 'fallback', true],
  odp: ['document', 'application/vnd.oasis.opendocument.presentation', 'fallback', true],
  rtf: ['document', 'application/rtf', 'fallback', false],
  doc: ['document', 'application/msword', 'fallback', false],
  xls: ['document', 'application/vnd.ms-excel', 'fallback', false],
  ppt: ['document', 'application/vnd.ms-powerpoint', 'fallback', false],
  mobi: ['document', 'application/x-mobipocket-ebook', 'fallback', false],
  azw: ['document', 'application/vnd.amazon.ebook', 'fallback', false],
  azw3: ['document', 'application/vnd.amazon.ebook', 'fallback', false],
  djvu: ['document', 'image/vnd.djvu', 'fallback', false],
  djv: ['document', 'image/vnd.djvu', 'fallback', false],
  odg: ['document', 'application/vnd.oasis.opendocument.graphics', 'fallback', false],
  pages: ['document', 'application/vnd.apple.pages', 'fallback', false],
  numbers: ['document', 'application/vnd.apple.numbers', 'fallback', false],
  key: ['document', 'application/vnd.apple.keynote', 'fallback', false],
  ps: ['document', 'application/postscript', 'fallback', false],
  eps: ['document', 'application/postscript', 'fallback', false],
  xps: ['document', 'application/vnd.ms-xpsdocument', 'fallback', false],
  cbz: ['document', 'application/vnd.comicbook+zip', 'fallback', false],
  cbr: ['document', 'application/vnd.comicbook-rar', 'fallback', false],
  txt: ['text', 'text/plain', 'text', true],
  md: ['text', 'text/markdown', 'text', true],
  log: ['text', 'text/plain', 'text', true],
  toml: ['text', 'application/toml', 'text', true],
  yaml: ['text', 'application/yaml', 'text', true],
  yml: ['text', 'application/yaml', 'text', true],
  ini: ['text', 'text/plain', 'text', true],
  conf: ['text', 'text/plain', 'text', true],
  vtt: ['text', 'text/vtt', 'text', true],
  csv: ['text', 'text/csv', 'text', true],
  json: ['text', 'application/json', 'text', true],
  text: ['text', 'text/plain', 'text', false],
  srt: ['text', 'text/plain', 'text', false],
  markdown: ['text', 'text/markdown', 'text', false],
  mdx: ['text', 'text/markdown', 'text', false],
  rst: ['text', 'text/x-rst', 'text', false],
  adoc: ['text', 'text/asciidoc', 'text', false],
  asciidoc: ['text', 'text/asciidoc', 'text', false],
  org: ['text', 'text/org', 'text', false],
  tex: ['text', 'text/x-tex', 'text', false],
  latex: ['text', 'text/x-tex', 'text', false],
  tsv: ['text', 'text/tab-separated-values', 'text', false],
  ics: ['text', 'text/calendar', 'text', false],
  vcf: ['text', 'text/vcard', 'text', false],
  diff: ['text', 'text/x-diff', 'text', false],
  patch: ['text', 'text/x-diff', 'text', false],
  jsonc: ['text', 'application/json', 'text', false],
  json5: ['text', 'application/json', 'text', false],
  jsonl: ['text', 'application/json', 'text', false],
  ndjson: ['text', 'application/json', 'text', false],
  xml: ['text', 'application/xml', 'text', false],
  xsl: ['text', 'application/xml', 'text', false],
  xsd: ['text', 'application/xml', 'text', false],
  plist: ['text', 'application/xml', 'text', false],
  html: ['text', 'text/html', 'text', false],
  htm: ['text', 'text/html', 'text', false],
  xhtml: ['text', 'text/html', 'text', false],
  css: ['text', 'text/css', 'text', false],
  scss: ['text', 'text/css', 'text', false],
  sass: ['text', 'text/css', 'text', false],
  less: ['text', 'text/css', 'text', false],
  cfg: ['text', 'text/plain', 'text', false],
  env: ['text', 'text/plain', 'text', false],
  lock: ['text', 'text/plain', 'text', false],
  properties: ['text', 'text/plain', 'text', false],
  editorconfig: ['text', 'text/plain', 'text', false],
  cmake: ['text', 'text/plain', 'text', false],
  mk: ['text', 'text/plain', 'text', false],
  make: ['text', 'text/plain', 'text', false],
  dockerfile: ['text', 'text/plain', 'text', false],
  tf: ['text', 'text/plain', 'text', false],
  tfvars: ['text', 'text/plain', 'text', false],
  hcl: ['text', 'text/plain', 'text', false],
  vue: ['text', 'text/plain', 'text', false],
  svelte: ['text', 'text/plain', 'text', false],
  astro: ['text', 'text/plain', 'text', false],
  qml: ['text', 'text/plain', 'text', false],
  glsl: ['text', 'text/plain', 'text', false],
  wgsl: ['text', 'text/plain', 'text', false],
  hlsl: ['text', 'text/plain', 'text', false],
  vert: ['text', 'text/plain', 'text', false],
  frag: ['text', 'text/plain', 'text', false],
  sv: ['text', 'text/plain', 'text', false],
  vhd: ['text', 'text/plain', 'text', false],
  vhdl: ['text', 'text/plain', 'text', false],
  sol: ['text', 'text/plain', 'text', false],
  prisma: ['text', 'text/plain', 'text', false],
  graphql: ['text', 'application/graphql', 'text', false],
  gql: ['text', 'application/graphql', 'text', false],
  sql: ['text', 'application/sql', 'text', false],
  rs: ['text', 'text/x-rust', 'text', false],
  c: ['text', 'text/x-c', 'text', false],
  h: ['text', 'text/x-c', 'text', false],
  cc: ['text', 'text/x-c++', 'text', false],
  cpp: ['text', 'text/x-c++', 'text', false],
  cxx: ['text', 'text/x-c++', 'text', false],
  hh: ['text', 'text/x-c++', 'text', false],
  hpp: ['text', 'text/x-c++', 'text', false],
  hxx: ['text', 'text/x-c++', 'text', false],
  cs: ['text', 'text/x-csharp', 'text', false],
  go: ['text', 'text/x-go', 'text', false],
  java: ['text', 'text/x-java-source', 'text', false],
  kt: ['text', 'text/x-kotlin', 'text', false],
  kts: ['text', 'text/x-kotlin', 'text', false],
  scala: ['text', 'text/x-scala', 'text', false],
  sc: ['text', 'text/x-scala', 'text', false],
  swift: ['text', 'text/x-swift', 'text', false],
  m: ['text', 'text/x-objective-c', 'text', false],
  mm: ['text', 'text/x-objective-c', 'text', false],
  py: ['text', 'text/x-python', 'text', false],
  pyi: ['text', 'text/x-python', 'text', false],
  pyw: ['text', 'text/x-python', 'text', false],
  rb: ['text', 'text/x-ruby', 'text', false],
  rake: ['text', 'text/x-ruby', 'text', false],
  gemspec: ['text', 'text/x-ruby', 'text', false],
  php: ['text', 'text/x-php', 'text', false],
  pl: ['text', 'text/x-perl', 'text', false],
  pm: ['text', 'text/x-perl', 'text', false],
  lua: ['text', 'text/x-lua', 'text', false],
  js: ['text', 'text/javascript', 'text', false],
  mjs: ['text', 'text/javascript', 'text', false],
  cjs: ['text', 'text/javascript', 'text', false],
  jsx: ['text', 'text/javascript', 'text', false],
  ts: ['text', 'text/typescript', 'text', false],
  cts: ['text', 'text/typescript', 'text', false],
  tsx: ['text', 'text/typescript', 'text', false],
  sh: ['text', 'text/x-shellscript', 'text', false],
  bash: ['text', 'text/x-shellscript', 'text', false],
  zsh: ['text', 'text/x-shellscript', 'text', false],
  fish: ['text', 'text/x-shellscript', 'text', false],
  ksh: ['text', 'text/x-shellscript', 'text', false],
  csh: ['text', 'text/x-shellscript', 'text', false],
  ps1: ['text', 'text/x-powershell', 'text', false],
  psm1: ['text', 'text/x-powershell', 'text', false],
  psd1: ['text', 'text/x-powershell', 'text', false],
  bat: ['text', 'text/x-bat', 'text', false],
  cmd: ['text', 'text/x-bat', 'text', false],
  zig: ['text', 'text/x-zig', 'text', false],
  nim: ['text', 'text/x-nim', 'text', false],
  ex: ['text', 'text/x-elixir', 'text', false],
  exs: ['text', 'text/x-elixir', 'text', false],
  erl: ['text', 'text/x-erlang', 'text', false],
  hrl: ['text', 'text/x-erlang', 'text', false],
  hs: ['text', 'text/x-haskell', 'text', false],
  lhs: ['text', 'text/x-haskell', 'text', false],
  ml: ['text', 'text/x-ocaml', 'text', false],
  mli: ['text', 'text/x-ocaml', 'text', false],
  clj: ['text', 'text/x-clojure', 'text', false],
  cljs: ['text', 'text/x-clojure', 'text', false],
  cljc: ['text', 'text/x-clojure', 'text', false],
  edn: ['text', 'text/x-clojure', 'text', false],
  el: ['text', 'text/x-lisp', 'text', false],
  lisp: ['text', 'text/x-lisp', 'text', false],
  scm: ['text', 'text/x-lisp', 'text', false],
  rkt: ['text', 'text/x-lisp', 'text', false],
  dart: ['text', 'text/x-dart', 'text', false],
  r: ['text', 'text/x-r', 'text', false],
  jl: ['text', 'text/x-julia', 'text', false],
  groovy: ['text', 'text/x-groovy', 'text', false],
  gradle: ['text', 'text/x-groovy', 'text', false],
  nix: ['text', 'text/x-nix', 'text', false],
  proto: ['text', 'text/x-protobuf', 'text', false],
  asm: ['text', 'text/x-asm', 'text', false],
  s: ['text', 'text/x-asm', 'text', false],
  zip: ['archive', 'application/zip', 'fallback', false],
  whl: ['archive', 'application/zip', 'fallback', false],
  tar: ['archive', 'application/x-tar', 'fallback', false],
  gz: ['archive', 'application/gzip', 'fallback', false],
  gzip: ['archive', 'application/gzip', 'fallback', false],
  tgz: ['archive', 'application/gzip', 'fallback', false],
  bz2: ['archive', 'application/x-bzip2', 'fallback', false],
  bzip2: ['archive', 'application/x-bzip2', 'fallback', false],
  tbz2: ['archive', 'application/x-bzip2', 'fallback', false],
  xz: ['archive', 'application/x-xz', 'fallback', false],
  txz: ['archive', 'application/x-xz', 'fallback', false],
  zst: ['archive', 'application/zstd', 'fallback', false],
  zstd: ['archive', 'application/zstd', 'fallback', false],
  tzst: ['archive', 'application/zstd', 'fallback', false],
  lz4: ['archive', 'application/x-lz4', 'fallback', false],
  lz: ['archive', 'application/x-lzip', 'fallback', false],
  lzma: ['archive', 'application/x-lzma', 'fallback', false],
  '7z': ['archive', 'application/x-7z-compressed', 'fallback', false],
  rar: ['archive', 'application/vnd.rar', 'fallback', false],
  cab: ['archive', 'application/vnd.ms-cab-compressed', 'fallback', false],
  iso: ['archive', 'application/x-iso9660-image', 'fallback', false],
  dmg: ['archive', 'application/x-apple-diskimage', 'fallback', false],
  deb: ['archive', 'application/vnd.debian.binary-package', 'fallback', false],
  rpm: ['archive', 'application/x-rpm', 'fallback', false],
  apk: ['archive', 'application/vnd.android.package-archive', 'fallback', false],
  jar: ['archive', 'application/java-archive', 'fallback', false],
  war: ['archive', 'application/java-archive', 'fallback', false],
  ear: ['archive', 'application/java-archive', 'fallback', false],
  cpio: ['archive', 'application/x-cpio', 'fallback', false],
  ar: ['archive', 'application/x-archive', 'fallback', false],
  br: ['archive', 'application/x-brotli', 'fallback', false],
  sz: ['archive', 'application/x-snappy-framed', 'fallback', false],
};
// END_MEDIA_TABLE

const CATEGORY_SET = new Set(FILE_CATEGORIES);

/**
 * Extracts the lowercase file extension from a file name or path (excluding dotfiles without a
 * second dot).
 */
export function extensionFromName(name) {
  const base = baseFileName(name);
  const dot = base.lastIndexOf('.');
  if (dot <= 0 || dot === base.length - 1) return '';
  return base.slice(dot + 1).toLowerCase();
}

/**
 * Returns `true` when `name` has no file extension (e.g. `README`, `Makefile`, `.gitignore`).
 */
export function isExtensionlessName(name) {
  return extensionFromName(name) === '';
}

/**
 * Looks up a canonical entry `[category, mime, viewerMode, canOpen]` from `MEDIA_TABLE`.
 */
function lookupMedia(name) {
  const ext = extensionFromName(name);
  if (!ext || !Object.prototype.hasOwnProperty.call(MEDIA_TABLE, ext)) return null;
  return MEDIA_TABLE[ext];
}

/**
 * Maps a filename to one of Legion's seven file categories by extension only (case-insensitive).
 */
export function categoryFromName(name) {
  const row = lookupMedia(name);
  return row ? row[0] : 'other';
}

/**
 * Derives a fallback MIME type from a filename when an older node omits `mime`.
 */
export function mimeFromName(name) {
  const row = lookupMedia(name);
  return row ? row[1] : 'application/octet-stream';
}

/**
 * Returns how the viewer should open a file (`'inline'`, `'thumb_or_fallback'`, `'text'`, or
 * `'fallback'`) based on its extension. Unknown extensions return `'fallback'` (extensionless
 * files are probed as text in `FilesViewer`).
 */
export function viewerModeFromName(name) {
  const row = lookupMedia(name);
  return row ? row[2] : 'fallback';
}

/**
 * Returns `true` ONLY when `entryOrName` has an extension on Legion's deliberate direct-open
 * allow-list (matching `file_policy.rs` and `TransferPathPolicy.kt` 1-to-1).
 *
 * Refused executables, scripts, HTML, XML, SVG, archives, legacy/macro Office formats, unknown
 * extensions, and extensionless files always return `false`.
 */
export function canOpenExternally(entryOrName) {
  if (!entryOrName) return false;
  const name =
    typeof entryOrName === 'string'
      ? entryOrName
      : entryOrName.name || entryOrName.path || '';
  const row = lookupMedia(name);
  return Boolean(row && row[3]);
}

/**
 * Resolves the effective category of a listing or search entry, using the server's `category`
 * when present and falling back to the client extension table for older nodes.
 */
export function entryCategory(entry) {
  if (!entry) return 'other';
  if (entry.kind === 'dir') return 'folder';
  if (typeof entry.category === 'string' && CATEGORY_SET.has(entry.category.toLowerCase())) {
    return entry.category.toLowerCase();
  }
  return categoryFromName(entry.name || entry.path || '');
}

/**
 * Plain-language label for an entry's kind ("Folder", "Picture", "Video", "Music", "Document",
 * "Text", "Archive", "File").
 */
export function plainCategoryLabel(entry) {
  const cat = typeof entry === 'string' ? entry : entryCategory(entry);
  switch (cat) {
    case 'folder':
    case 'dir':
      return 'Folder';
    case 'image':
      return 'Picture';
    case 'video':
      return 'Video';
    case 'audio':
      return 'Music';
    case 'document':
      return 'Document';
    case 'text':
      return 'Text';
    case 'archive':
      return 'Archive';
    default:
      return 'File';
  }
}

/**
 * Icon symbol name in `icons.js` for a file or folder entry.
 */
export function entryIconName(entry) {
  const cat = typeof entry === 'string' ? entry : entryCategory(entry);
  switch (cat) {
    case 'folder':
    case 'dir':
      return 'files';
    case 'image':
      return 'image';
    case 'video':
      return 'video';
    case 'audio':
      return 'music';
    case 'archive':
      return 'archive';
    default:
      return 'doc';
  }
}

/**
 * Whether an entry can request a server thumbnail (`fs.thumb` supports pictures, video posters,
 * and — when `allowAudio` is enabled for a node reporting `hasAudioThumb` — embedded or folder
 * audio covers).
 */
export function isThumbEligible(entry, { allowAudio = false } = {}) {
  if (!entry || entry.kind === 'dir') return false;
  const cat = entryCategory(entry);
  return cat === 'image' || cat === 'video' || (allowAudio && cat === 'audio');
}

/**
 * Whether a file/folder name is hidden (Unix dotfile).
 */
export function isHiddenName(name) {
  const base = baseFileName(name);
  return base.startsWith('.') && base !== '.' && base !== '..';
}

/**
 * Feature detection from a machine's tool catalogue, caller's allowed tool set, and link surface.
 * Rebuilt on `computeFileCapabilities` in `capabilities.js`: unknown capabilities assume the least.
 */
export function detectFileCapabilities({
  tools = [],
  allowedSet = null,
  link = null,
  machine = null,
  known = undefined,
} = {}) {
  const isKnown =
    typeof known === 'boolean' ? known : Array.isArray(tools) && tools.length > 0;
  return computeFileCapabilities({ known: isKnown, tools, allowedSet, link, machine });
}

/**
 * Sorts folder entries with directories always first, then by `name`, `modified`, or `size`.
 */
export function sortEntries(entries, { sort = 'name', order = 'asc' } = {}) {
  const list = Array.isArray(entries) ? [...entries] : [];
  const dir = order === 'desc' ? -1 : 1;

  const cmpName = (a, b) => {
    const an = String(a?.name || '').toLowerCase();
    const bn = String(b?.name || '').toLowerCase();
    if (an < bn) return -1;
    if (an > bn) return 1;
    const rawA = String(a?.name || '');
    const rawB = String(b?.name || '');
    return rawA < rawB ? -1 : rawA > rawB ? 1 : 0;
  };

  list.sort((a, b) => {
    const aDir = a?.kind === 'dir' ? 0 : 1;
    const bDir = b?.kind === 'dir' ? 0 : 1;
    if (aDir !== bDir) return aDir - bDir;

    if (sort === 'modified') {
      const ta = a?.mtime ? Date.parse(a.mtime) : 0;
      const tb = b?.mtime ? Date.parse(b.mtime) : 0;
      const diff = (Number.isFinite(ta) ? ta : 0) - (Number.isFinite(tb) ? tb : 0);
      if (diff !== 0) return diff * dir;
      return cmpName(a, b);
    }

    if (sort === 'size') {
      const sa = Number.isFinite(a?.size) ? a.size : 0;
      const sb = Number.isFinite(b?.size) ? b.size : 0;
      if (sa !== sb) return (sa - sb) * dir;
      return cmpName(a, b);
    }

    return cmpName(a, b) * dir;
  });

  return list;
}

/**
 * Filters entries on the client (hidden toggle, optional search query, categories, size/date).
 */
export function filterEntries(
  entries,
  { hidden = false, query = '', categories = [], minSize = null, maxSize = null, modifiedAfter = null, keepDirs = false } = {},
) {
  const list = Array.isArray(entries) ? entries : [];
  const words = String(query || '')
    .trim()
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean);
  const catSet = Array.isArray(categories) && categories.length > 0 ? new Set(categories) : null;
  const afterMs = modifiedAfter ? Date.parse(modifiedAfter) : NaN;

  return list.filter((entry) => {
    if (!entry) return false;
    if (!hidden && isHiddenName(entry.name || entry.path || '')) return false;

    if (words.length > 0) {
      const lowerName = String(entry.name || baseFileName(entry.path || '')).toLowerCase();
      for (const w of words) {
        if (!lowerName.includes(w)) return false;
      }
    }

    if (catSet) {
      const cat = entryCategory(entry);
      if (cat === 'folder') {
        if (!keepDirs && !catSet.has('folder')) return false;
      } else if (!catSet.has(cat)) {
        return false;
      }
    }

    if (entry.kind !== 'dir') {
      if (minSize != null && (entry.size ?? 0) < minSize) return false;
      if (maxSize != null && (entry.size ?? 0) > maxSize) return false;
    }

    if (Number.isFinite(afterMs)) {
      const ts = entry.mtime ? Date.parse(entry.mtime) : NaN;
      if (!Number.isFinite(ts) || ts < afterMs) return false;
    }

    return true;
  });
}

/**
 * Chooses grid view by default when most non-folder entries in a folder are pictures or video.
 */
export function shouldDefaultToGrid(entries) {
  const files = (entries || []).filter(
    (e) => e && e.kind !== 'dir' && !isHiddenName(e.name || e.path || ''),
  );
  if (files.length < 2) return false;
  let media = 0;
  for (const f of files) {
    const cat = entryCategory(f);
    if (cat === 'image' || cat === 'video') media += 1;
  }
  return media / files.length > 0.5;
}

/**
 * Pure selection state helpers (Set of full paths).
 */
export function toggleSelection(selectedSet, path) {
  const next = new Set(selectedSet || []);
  if (next.has(path)) next.delete(path);
  else next.add(path);
  return next;
}

export function selectAllEntries(entries) {
  return new Set((entries || []).filter((e) => e?.path).map((e) => e.path));
}

export function selectionSummary(entries, selectedSet) {
  const set = selectedSet instanceof Set ? selectedSet : new Set(selectedSet || []);
  const picked = (entries || []).filter((e) => e && set.has(e.path));
  let totalBytes = 0;
  let fileCount = 0;
  let folderCount = 0;
  let allSizesKnown = picked.length > 0;
  for (const item of picked) {
    if (item.kind === 'dir') {
      folderCount += 1;
      allSizesKnown = false;
    } else {
      fileCount += 1;
      if (Number.isFinite(item.size)) totalBytes += item.size;
      else allSizesKnown = false;
    }
  }
  return {
    count: picked.length,
    fileCount,
    folderCount,
    totalBytes,
    allSizesKnown,
    items: picked,
  };
}

/**
 * Formats a folder's item count, respecting `total_exact === false` as a lower bound.
 */
export function formatFolderTotal(total, totalExact = true) {
  if (!Number.isFinite(total) || total < 0) return '';
  if (totalExact === false) {
    return `More than ${total.toLocaleString()} items`;
  }
  return total === 1 ? '1 item' : `${total.toLocaleString()} items`;
}

/**
 * Formats the folder pager status line, respecting `total_exact === false`.
 */
export function formatShowingCount(shown, total, totalExact = true) {
  const shownText = Number.isFinite(shown) ? shown.toLocaleString() : '0';
  if (!Number.isFinite(total) || total <= 0) return `Showing ${shownText}`;
  if (totalExact === false) {
    return `Showing ${shownText} of more than ${total.toLocaleString()}`;
  }
  return `Showing ${shownText} of ${total.toLocaleString()}`;
}

export const THUMB_BATCH_MAX = 32;
export const THUMB_MAX_RETRIES = 3;
export const THUMB_RETRY_BASE_MS = 350;

const RETRYABLE_THUMB_CODES = new Set([
  'busy',
  'rate_limited',
  'temporarily_unavailable',
  'timeout',
  'try_again',
  'overloaded',
]);

/**
 * Returns true when an `fs.thumb` per-path error means the node is temporarily busy decoding
 * and the caller should quietly retry after a short backoff.
 */
export function isRetryableThumbError(err) {
  if (!err || typeof err !== 'object') return false;
  if (err.retryable === true) return true;
  const code = typeof err.code === 'string' ? err.code.toLowerCase() : '';
  return RETRYABLE_THUMB_CODES.has(code);
}

/**
 * Extracts per-path entries from `fs.thumb`'s `{ results: [{ path, ok | error }] }` wire reply
 * into a null-prototype map keyed by path. Ok entries map to `{ mime, content, width, height }`;
 * error entries map to `{ error }`.
 */
export function extractThumbMap(res) {
  const map = Object.create(null);
  if (!res || typeof res !== 'object' || !Array.isArray(res.results)) return map;
  for (const item of res.results) {
    if (!item || typeof item !== 'object' || typeof item.path !== 'string' || !item.path) continue;
    if (item.ok && typeof item.ok === 'object' && !Array.isArray(item.ok)) {
      map[item.path] = item.ok;
    } else if (item.error) {
      map[item.path] = { error: item.error };
    }
  }
  return map;
}

/**
 * Creates a per-machine batched `fs.thumb` loader (up to 32 paths per call) with quiet
 * exponential backoff retries for retryable per-path or batch errors (such as `busy`).
 */
export function createThumbQueue({
  machine,
  link,
  capsStore = null,
  thumbCache,
  onUpdate = null,
  size = 128,
  fit = 'cover',
  debounceMs = 24,
  batchGapMs = 30,
  schedule = (fn, ms) => setTimeout(fn, ms),
  cancel = (id) => clearTimeout(id),
}) {
  let alive = true;
  const queue = new Map();
  const inFlight = new Set();
  const pendingRetry = new Set();
  const retryCount = new Map();
  const retryTimers = new Set();
  let flushTimer = null;

  function scheduleFlush(delayMs) {
    if (!alive || flushTimer !== null) return;
    flushTimer = schedule(flush, delayMs);
  }

  function scheduleRetry(item) {
    const attempt = retryCount.get(item.path) || 0;
    if (attempt >= THUMB_MAX_RETRIES) return false;
    retryCount.set(item.path, attempt + 1);
    pendingRetry.add(item.path);
    const delayMs = THUMB_RETRY_BASE_MS * (2 ** attempt);
    const tid = schedule(() => {
      retryTimers.delete(tid);
      pendingRetry.delete(item.path);
      if (!alive || inFlight.has(item.path)) return;
      queue.set(item.path, item);
      scheduleFlush(debounceMs);
    }, delayMs);
    retryTimers.add(tid);
    return true;
  }

  function machineAllowsAudioThumb() {
    if (!capsStore) return true;
    if (typeof capsStore.fileCapabilities === 'function') {
      return Boolean(capsStore.fileCapabilities(machine)?.hasAudioThumb);
    }
    return capsStore.hasTool(machine, 'fs.tags');
  }

  async function flush() {
    flushTimer = null;
    if (!alive || !machine || queue.size === 0) return;

    const batch = [];
    for (const item of queue.values()) {
      if (batch.length >= THUMB_BATCH_MAX) break;
      batch.push(item);
    }
    for (const item of batch) {
      queue.delete(item.path);
      inFlight.add(item.path);
    }
    if (batch.length === 0) return;

    let activeBatch = batch;
    try {
      if (capsStore) {
        if (!capsStore.isKnown(machine)) {
          await capsStore.ensure(machine).catch(() => null);
        }
        if (!alive) return;
        if (!capsStore.hasTool(machine, 'fs.thumb')) {
          for (const item of batch) inFlight.delete(item.path);
          queue.clear();
          return;
        }
        if (!machineAllowsAudioThumb()) {
          const kept = [];
          for (const item of batch) {
            if (item.isAudio) inFlight.delete(item.path);
            else kept.push(item);
          }
          for (const [p, queued] of queue.entries()) {
            if (queued.isAudio) queue.delete(p);
          }
          if (kept.length === 0) return;
          activeBatch = kept;
        }
      }

      const paths = activeBatch.map((item) => item.path);
      const res = capsStore
        ? (await capsStore.call('fs.thumb', { paths, size, fit }, { on: machine })).value
        : await link.call('fs.thumb', { paths, size, fit }, { on: machine });
      if (!alive) return;

      const map = extractThumbMap(res);
      let updated = false;
      for (const item of activeBatch) {
        inFlight.delete(item.path);
        const key = thumbCacheKey(machine, item.path, item.mtime, size);
        const t = map[item.path];
        if (t?.content && t?.mime) {
          retryCount.delete(item.path);
          thumbCache.set(key, { url: `data:${t.mime};base64,${t.content}` });
          updated = true;
        } else if (isRetryableThumbError(t?.error) && scheduleRetry(item)) {
          // Leave placeholder visible while waiting quietly for backoff retry.
        } else {
          retryCount.delete(item.path);
          thumbCache.set(key, { failed: true });
        }
      }
      if (updated && onUpdate) onUpdate();
    } catch (err) {
      if (!alive) return;
      const retryable = isRetryableThumbError(err);
      for (const item of activeBatch) {
        inFlight.delete(item.path);
        if (retryable && scheduleRetry(item)) continue;
        retryCount.delete(item.path);
        const key = thumbCacheKey(machine, item.path, item.mtime, size);
        thumbCache.set(key, { failed: true });
      }
    } finally {
      if (alive && queue.size > 0) {
        scheduleFlush(batchGapMs);
      }
    }
  }

  return {
    enqueue(item) {
      if (!alive || !machine || !item?.path) return;
      const isAudio = Boolean(item.isAudio || entryCategory(item) === 'audio');
      if (capsStore && capsStore.isKnown(machine)) {
        if (!capsStore.hasTool(machine, 'fs.thumb')) return;
        if (isAudio && !machineAllowsAudioThumb()) return;
      }
      const mtime = item.mtime || '';
      const key = thumbCacheKey(machine, item.path, mtime, size);
      if (
        thumbCache.get(key) ||
        inFlight.has(item.path) ||
        queue.has(item.path) ||
        pendingRetry.has(item.path)
      ) {
        return;
      }
      queue.set(item.path, { path: item.path, mtime, isAudio });
      scheduleFlush(debounceMs);
    },

    dispose() {
      alive = false;
      if (flushTimer !== null) {
        cancel(flushTimer);
        flushTimer = null;
      }
      for (const tid of retryTimers) cancel(tid);
      retryTimers.clear();
      pendingRetry.clear();
      queue.clear();
      inFlight.clear();
      retryCount.clear();
    },
  };
}

/**
 * Path manipulation for Unix, `~`, Windows drive (`C:\...`), and UNC (`\\server\share`) paths.
 */
export function isWindowsPath(filePath) {
  return typeof filePath === 'string' && (/^[A-Za-z]:[\\/]/.test(filePath) || /^[A-Za-z]:$/.test(filePath) || filePath.startsWith('\\\\'));
}

export function baseFileName(filePath) {
  if (!filePath || typeof filePath !== 'string') return '';
  if (filePath === '/' || filePath === '~') return filePath;
  if (/^[A-Za-z]:\\?$/.test(filePath)) return filePath.slice(0, 2);
  const cleaned = filePath.replace(/[/\\]+$/, '');
  const idx = Math.max(cleaned.lastIndexOf('/'), cleaned.lastIndexOf('\\'));
  return idx === -1 ? cleaned : cleaned.slice(idx + 1);
}

export function parentFilePath(filePath) {
  if (!filePath || typeof filePath !== 'string') return null;
  if (filePath === '/' || filePath === '~') return null;
  if (/^[A-Za-z]:\\?$/.test(filePath)) return null;
  if (isWindowsPath(filePath)) {
    const cleaned = filePath.replace(/\\+$/, '');
    const idx = cleaned.lastIndexOf('\\');
    if (idx <= 2) return `${cleaned.slice(0, 2)}\\`;
    return cleaned.slice(0, idx);
  }
  const cleaned = filePath.replace(/\/+$/, '');
  const idx = cleaned.lastIndexOf('/');
  if (idx < 0) return null;
  if (idx === 0) return '/';
  return cleaned.slice(0, idx);
}

export function joinFilePath(dir, name) {
  const cleanName = String(name || '').replace(/^[/\\]+/, '');
  if (!dir) return cleanName;
  if (!cleanName) return dir;
  if (isWindowsPath(dir)) {
    const sep = dir.endsWith('\\') ? '' : '\\';
    return `${dir}${sep}${cleanName}`;
  }
  const sep = dir.endsWith('/') ? '' : '/';
  return `${dir}${sep}${cleanName}`;
}

/**
 * Turns a machine + path into clickable breadcrumb items for the `.where` path line.
 * In plain mode, collapses `/home/<user>` or `/Users/<user>` or `C:\Users\<user>` to `Home`.
 */
export function splitFileCrumbs(machine, fullPath, { technical = false, homePath = null } = {}) {
  const crumbs = [];
  if (machine) {
    crumbs.push({ label: machine, path: '~', isMachine: true });
  }
  if (!fullPath || fullPath === '~') {
    crumbs.push({ label: technical ? '~' : 'Home', path: '~' });
    return crumbs;
  }

  if (isWindowsPath(fullPath)) {
    const norm = fullPath.replace(/\//g, '\\');
    const homeMatch = !technical ? norm.match(/^([A-Za-z]:\\Users\\[^\\]+)(?:\\(.*))?$/i) : null;
    if (homeMatch) {
      const homeRoot = homeMatch[1];
      crumbs.push({ label: 'Home', path: homeRoot });
      const rest = (homeMatch[2] || '').split('\\').filter(Boolean);
      let acc = homeRoot;
      for (const seg of rest) {
        acc = `${acc}\\${seg}`;
        crumbs.push({ label: seg, path: acc });
      }
      return crumbs;
    }
    const parts = norm.split(/\\+/).filter(Boolean);
    let acc = '';
    for (let i = 0; i < parts.length; i += 1) {
      acc = i === 0 ? `${parts[i]}\\` : `${acc}${acc.endsWith('\\') ? '' : '\\'}${parts[i]}`;
      crumbs.push({ label: parts[i], path: acc });
    }
    return crumbs;
  }

  if (fullPath === '/') {
    crumbs.push({ label: '/', path: '/' });
    return crumbs;
  }

  const normalized = fullPath.replace(/\/+$/, '');
  const detectedHome =
    !technical &&
    (homePath && (normalized === homePath || normalized.startsWith(`${homePath}/`))
      ? homePath
      : normalized.match(/^\/(?:home|Users)\/[^/]+/)?.[0] || null);

  if (detectedHome) {
    crumbs.push({ label: 'Home', path: detectedHome });
    const rest = normalized.slice(detectedHome.length).split('/').filter(Boolean);
    let acc = detectedHome;
    for (const seg of rest) {
      acc += `/${seg}`;
      crumbs.push({ label: seg, path: acc });
    }
    return crumbs;
  }

  if (normalized.startsWith('~/')) {
    crumbs.push({ label: technical ? '~' : 'Home', path: '~' });
    const rest = normalized.slice(2).split('/').filter(Boolean);
    let acc = '~';
    for (const seg of rest) {
      acc += `/${seg}`;
      crumbs.push({ label: seg, path: acc });
    }
    return crumbs;
  }

  crumbs.push({ label: '/', path: '/' });
  const parts = normalized.split('/').filter(Boolean);
  let acc = '';
  for (const seg of parts) {
    acc += `/${seg}`;
    crumbs.push({ label: seg, path: acc });
  }
  return crumbs;
}

/**
 * Formats a folder path concisely for search results and destination summaries
 * (e.g. "Home › Videos › Holiday" in plain mode, or raw path in technical mode).
 */
export function formatPathForDisplay(fullPath, { technical = false, homePath = null } = {}) {
  if (!fullPath) return '';
  if (technical) return fullPath;
  const crumbs = splitFileCrumbs(null, fullPath, { technical: false, homePath });
  return crumbs.map((c) => c.label).join(' › ');
}

/**
 * Friendly folder heading from a path (`Holiday`, `Home`, `/`, `C:`).
 */
export function folderDisplayName(fullPath, { technical = false, homePath = null } = {}) {
  if (!fullPath || fullPath === '~') return technical ? '~' : 'Home';
  if (!technical) {
    if (homePath && fullPath === homePath) return 'Home';
    if (/^\/(?:home|Users)\/[^/]+\/?$/.test(fullPath)) return 'Home';
    if (/^[A-Za-z]:\\Users\\[^\\]+\\?$/i.test(fullPath)) return 'Home';
  }
  return baseFileName(fullPath) || fullPath;
}

/**
 * Computes the logical fallback back-step route for Files when there is no in-session browser
 * history (e.g. after reloading or deep-linking a URL).
 */
export function filesFallbackBackRoute(route, { hasSheet = false, selecting = false } = {}) {
  if (!route || route.section !== 'files') return null;
  if (hasSheet) return { action: 'close-sheet' };
  if (selecting) return { action: 'end-select' };
  if (route.viewer) {
    return { action: 'navigate', href: routeHref({ ...route, viewer: null }) };
  }
  if (route.view === 'folder' && route.machine) {
    const parent = parentFilePath(route.path);
    const isHome =
      !parent ||
      route.path === '~' ||
      /^\/(?:home|Users)\/[^/]+\/?$/.test(route.path || '') ||
      /^[A-Za-z]:\\Users\\[^\\]+\\?$/i.test(route.path || '');
    if (isHome) {
      return { action: 'navigate', href: '#/files' };
    }
    return {
      action: 'navigate',
      href: routeHref({ section: 'files', view: 'folder', machine: route.machine, path: parent }),
    };
  }
  if (route.view === 'search' || route.view === 'favourites' || route.view === 'recent' || route.view === 'sync') {
    if (route.view === 'search' && route.machine && route.path) {
      return {
        action: 'navigate',
        href: routeHref({ section: 'files', view: 'folder', machine: route.machine, path: route.path }),
      };
    }
    return { action: 'navigate', href: '#/files' };
  }
  return { action: 'leave-files' };
}

/**
 * Translates a search size preset into `{ min_size, max_size }` bytes.
 */
export function sizePresetBounds(preset) {
  switch (preset) {
    case 'small':
      return { min_size: null, max_size: 1024 * 1024 };
    case 'medium':
      return { min_size: 1024 * 1024, max_size: 100 * 1024 * 1024 };
    case 'large':
      return { min_size: 100 * 1024 * 1024, max_size: null };
    default:
      return { min_size: null, max_size: null };
  }
}

/**
 * Translates a search changed-time preset into an ISO cutoff timestamp.
 */
export function changedPresetCutoff(preset, nowMs = Date.now()) {
  const day = 86_400_000;
  switch (preset) {
    case 'day':
      return new Date(nowMs - day).toISOString();
    case 'week':
      return new Date(nowMs - 7 * day).toISOString();
    case 'month':
      return new Date(nowMs - 30 * day).toISOString();
    case 'year':
      return new Date(nowMs - 365 * day).toISOString();
    default:
      return null;
  }
}

/**
 * Human-readable sort label for the dock's Sort button accessible name.
 */
export function sortDescription({ sort = 'name', order = 'asc' } = {}) {
  if (sort === 'modified') return order === 'desc' ? 'newest first' : 'oldest first';
  if (sort === 'size') return order === 'desc' ? 'largest first' : 'smallest first';
  return order === 'desc' ? 'by name (Z–A)' : 'by name';
}

/**
 * Computes destination path when sending an item to a destination folder.
 */
export function buildTransferDestination(destDir, sourcePath) {
  const name = baseFileName(sourcePath);
  return joinFilePath(destDir || '~', name);
}

const DEFAULT_VIRTUAL_ROW_HEIGHT = 56;

/**
 * Pure variable-height virtual list layout index.
 *
 * Tracks measured heights per stable item key, falls back to a live one-line estimated height for
 * unmeasured items, maintains exact prefix offsets in a `Float64Array`, and computes scroll-top
 * corrections when rows above the visible anchor change height so visible content never jumps.
 */
export function createVirtualListLayout({ estimatedHeight = DEFAULT_VIRTUAL_ROW_HEIGHT } = {}) {
  let keys = [];
  let keyToIndex = new Map();
  const measured = new Map();
  let estimate =
    Number.isFinite(estimatedHeight) && estimatedHeight > 0
      ? estimatedHeight
      : DEFAULT_VIRTUAL_ROW_HEIGHT;
  let offsets = new Float64Array(1);
  let dirtyFrom = 0;

  function ensureOffsets() {
    const count = keys.length;
    if (dirtyFrom > count) {
      dirtyFrom = count;
    }
    if (offsets.length < count + 1) {
      const next = new Float64Array(Math.max(count + 1, offsets.length * 2));
      const copyLen = Math.min(dirtyFrom + 1, offsets.length);
      if (copyLen > 0) {
        next.set(offsets.subarray(0, copyLen));
      }
      offsets = next;
    }
    if (dirtyFrom === 0) {
      offsets[0] = 0;
    }
    for (let i = dirtyFrom; i < count; i += 1) {
      const h = measured.get(keys[i]);
      offsets[i + 1] = offsets[i] + (h !== undefined ? h : estimate);
    }
    dirtyFrom = count;
  }

  function syncItems(nextKeys) {
    const list = Array.isArray(nextKeys) ? nextKeys : [];
    const prevCount = keys.length;
    const nextCount = list.length;

    // Fast path: append-only paging (existing prefix unchanged).
    let appendOnly = prevCount > 0 && nextCount >= prevCount;
    if (appendOnly) {
      if (keys[0] !== list[0] || keys[prevCount - 1] !== list[prevCount - 1]) {
        appendOnly = false;
      } else {
        for (let i = 1; i < prevCount - 1; i += 1) {
          if (keys[i] !== list[i]) {
            appendOnly = false;
            break;
          }
        }
      }
    }

    if (appendOnly) {
      for (let i = prevCount; i < nextCount; i += 1) {
        keyToIndex.set(list[i], i);
      }
      keys = list;
      dirtyFrom = Math.min(dirtyFrom, prevCount);
    } else {
      keys = list;
      keyToIndex = new Map();
      for (let i = 0; i < nextCount; i += 1) {
        keyToIndex.set(list[i], i);
      }
      dirtyFrom = 0;
      // Keep measured cache bounded across directory/filter churn.
      if (measured.size > Math.max(nextCount * 2, 10_000)) {
        for (const k of measured.keys()) {
          if (!keyToIndex.has(k)) measured.delete(k);
        }
      }
    }
  }

  function getHeight(index) {
    if (index < 0 || index >= keys.length) return estimate;
    const h = measured.get(keys[index]);
    return h !== undefined ? h : estimate;
  }

  function getOffset(index) {
    ensureOffsets();
    const count = keys.length;
    if (count === 0 || index <= 0) return 0;
    if (index >= count) return offsets[count];
    return offsets[index];
  }

  function getTotalHeight() {
    ensureOffsets();
    return offsets[keys.length];
  }

  function findIndexAtOffset(y) {
    ensureOffsets();
    const count = keys.length;
    if (count === 0 || !Number.isFinite(y) || y <= 0) return 0;
    if (y >= offsets[count]) return count;
    let lo = 0;
    let hi = count - 1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (offsets[mid + 1] <= y) {
        lo = mid + 1;
      } else if (offsets[mid] > y) {
        hi = mid - 1;
      } else {
        return mid;
      }
    }
    return Math.max(0, Math.min(count - 1, lo));
  }

  // Returns the first item whose top edge is at or below `scrollOffset` (minus subpixel tolerance).
  // Anchoring here ensures rows partially or fully above the viewport top absorb height deltas into
  // scrollTop rather than pushing visible rows below them downward.
  function findAnchorIndex(scrollOffset) {
    const count = keys.length;
    if (count === 0 || !Number.isFinite(scrollOffset) || scrollOffset <= 1) return 0;
    ensureOffsets();
    const target = scrollOffset - 0.5;
    let lo = 0;
    let hi = count - 1;
    let ans = count - 1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (offsets[mid] >= target) {
        ans = mid;
        hi = mid - 1;
      } else {
        lo = mid + 1;
      }
    }
    return ans;
  }

  function applyMeasurements({ updates = [], estimatedHeight: nextEstimate = null, scrollOffset = 0 } = {}) {
    const count = keys.length;
    const estimateChanged =
      nextEstimate != null &&
      Number.isFinite(nextEstimate) &&
      nextEstimate > 0 &&
      Math.abs(nextEstimate - estimate) >= 0.25;

    if (!estimateChanged && (!Array.isArray(updates) || updates.length === 0)) {
      return { changed: false, anchorIndex: 0, scrollDelta: 0, totalHeight: getTotalHeight() };
    }

    ensureOffsets();
    const shouldAnchor = count > 0 && Number.isFinite(scrollOffset) && scrollOffset > 1;
    const anchorIndex = shouldAnchor ? findAnchorIndex(scrollOffset) : 0;
    const oldAnchorOffset = anchorIndex > 0 ? offsets[anchorIndex] : 0;

    let changed = false;
    let minDirty = count;

    if (estimateChanged) {
      estimate = nextEstimate;
      changed = true;
      minDirty = 0;
    }

    if (Array.isArray(updates)) {
      for (let i = 0; i < updates.length; i += 1) {
        const u = updates[i];
        if (!u || !Number.isFinite(u.height) || u.height <= 0) continue;
        const key = u.key !== undefined ? u.key : keys[u.index];
        if (key === undefined) continue;
        const idx =
          typeof u.index === 'number' && keys[u.index] === key ? u.index : keyToIndex.get(key);
        const prev = measured.get(key);
        if (prev === undefined || Math.abs(u.height - prev) >= 0.25) {
          measured.set(key, u.height);
          changed = true;
          if (idx !== undefined && idx >= 0 && idx < minDirty) {
            minDirty = idx;
          }
        }
      }
    }

    if (!changed) {
      return { changed: false, anchorIndex, scrollDelta: 0, totalHeight: offsets[count] };
    }

    dirtyFrom = Math.min(dirtyFrom, minDirty);
    ensureOffsets();

    const newAnchorOffset = anchorIndex > 0 ? offsets[anchorIndex] : 0;
    const scrollDelta = shouldAnchor && anchorIndex > 0 ? newAnchorOffset - oldAnchorOffset : 0;
    return {
      changed: true,
      anchorIndex,
      scrollDelta,
      totalHeight: offsets[count],
    };
  }

  function resetMeasurements({ estimatedHeight: nextEstimate = null } = {}) {
    measured.clear();
    if (nextEstimate != null && Number.isFinite(nextEstimate) && nextEstimate > 0) {
      estimate = nextEstimate;
    }
    dirtyFrom = 0;
  }

  function computeWindow({
    scrollOffset = 0,
    viewportHeight = 720,
    overscanPx = 600,
    minBufferPx = 0,
    prevWindow = null,
  } = {}) {
    ensureOffsets();
    const count = keys.length;
    if (count === 0) {
      return {
        start: 0,
        end: 0,
        topPad: 0,
        bottomPad: 0,
        totalHeight: 0,
        visibleStart: 0,
        visibleEnd: 0,
      };
    }

    const vp = Number.isFinite(viewportHeight) && viewportHeight > 0 ? viewportHeight : 720;
    const off = Number.isFinite(scrollOffset) ? scrollOffset : 0;
    const os = Number.isFinite(overscanPx) && overscanPx > 0 ? overscanPx : 0;
    const totalHeight = offsets[count];

    const visTop = Math.max(0, Math.min(totalHeight, off));
    const visBottom = Math.max(0, Math.min(totalHeight, off + vp));
    const visibleStart = visTop >= totalHeight ? Math.max(0, count - 1) : findIndexAtOffset(visTop);
    const visibleEnd =
      visBottom <= 0
        ? Math.min(count, 1)
        : Math.min(count, findIndexAtOffset(Math.max(0, visBottom - 0.01)) + 1);

    // Hysteresis: keep the existing mounted range when it still provides at least `minBufferPx`
    // of mounted coverage above and below the live viewport, avoiding redundant DOM churn during
    // smooth frame-by-frame scrolling.
    if (
      prevWindow &&
      Number.isFinite(minBufferPx) &&
      minBufferPx > 0 &&
      prevWindow.start >= 0 &&
      prevWindow.end <= count &&
      prevWindow.end > prevWindow.start
    ) {
      const mountedTop = offsets[prevWindow.start];
      const mountedBottom = offsets[prevWindow.end];
      const hasTop = prevWindow.start === 0 || mountedTop <= visTop - minBufferPx;
      const hasBottom = prevWindow.end === count || mountedBottom >= visBottom + minBufferPx;
      const maxReasonableRows = Math.ceil((vp + os * 3) / Math.max(20, estimate)) + 20;
      if (hasTop && hasBottom && prevWindow.end - prevWindow.start <= maxReasonableRows) {
        return {
          start: prevWindow.start,
          end: prevWindow.end,
          topPad: Math.round(mountedTop),
          bottomPad: Math.round(Math.max(0, totalHeight - mountedBottom)),
          totalHeight: Math.round(totalHeight),
          visibleStart,
          visibleEnd,
        };
      }
    }

    const winTop = Math.max(0, off - os);
    const winBottom = Math.max(0, Math.min(totalHeight, off + vp + os));
    let start = winTop <= 0 ? 0 : findIndexAtOffset(winTop);
    let end =
      winBottom <= 0
        ? Math.min(count, 1)
        : Math.min(count, findIndexAtOffset(Math.max(0, winBottom - 0.01)) + 1);

    start = Math.min(start, visibleStart);
    end = Math.max(end, visibleEnd);
    start = Math.max(0, Math.min(count, start));
    end = Math.max(start, Math.min(count, end));

    return {
      start,
      end,
      topPad: Math.round(offsets[start]),
      bottomPad: Math.round(Math.max(0, totalHeight - offsets[end])),
      totalHeight: Math.round(totalHeight),
      visibleStart,
      visibleEnd,
    };
  }

  function computeScrollTargetForIndex(
    index,
    {
      listTop = 0,
      viewportHeight = 720,
      topInset = 0,
      bottomInset = 0,
      currentScrollTop = null,
      onlyIfOffscreen = false,
      align = 'center',
    } = {},
  ) {
    const count = keys.length;
    if (count === 0 || index < 0 || index >= count) return null;
    ensureOffsets();
    const vp = Number.isFinite(viewportHeight) && viewportHeight > 0 ? viewportHeight : 720;
    const tInset = Number.isFinite(topInset) && topInset > 0 ? topInset : 0;
    const bInset = Number.isFinite(bottomInset) && bottomInset > 0 ? bottomInset : 0;
    const itemTop = listTop + offsets[index];
    const itemBottom = itemTop + getHeight(index);

    if (onlyIfOffscreen && Number.isFinite(currentScrollTop)) {
      const visTop = currentScrollTop + tInset;
      const visBottom = currentScrollTop + Math.max(1, vp - bInset);
      if (itemTop >= visTop - 1 && itemBottom <= visBottom + 1) {
        return null;
      }
    }

    const usable = Math.max(1, vp - tInset - bInset);
    let target;
    if (align === 'start') {
      target = itemTop - tInset;
    } else if (align === 'end') {
      target = itemBottom - Math.max(1, vp - bInset);
    } else {
      target = itemTop - tInset - Math.max(0, (usable - (itemBottom - itemTop)) / 2);
    }
    return Math.max(0, Math.round(target));
  }

  return {
    syncItems,
    getCount: () => keys.length,
    getEstimatedHeight: () => estimate,
    getHeight,
    isMeasured: (index) => index >= 0 && index < keys.length && measured.has(keys[index]),
    indexOfKey: (key) => (keyToIndex.has(key) ? keyToIndex.get(key) : -1),
    getOffset,
    getTotalHeight,
    findIndexAtOffset,
    findAnchorIndex,
    applyMeasurements,
    resetMeasurements,
    computeWindow,
    computeScrollTargetForIndex,
  };
}

/**
 * Computes the visible `[start, end)` row slice and top/bottom spacer heights in pixels
 * for a uniform-row-height virtualized list (convenience wrapper).
 */
export function computeVirtualWindow({
  total,
  rowHeight = DEFAULT_VIRTUAL_ROW_HEIGHT,
  scrollOffset = 0,
  viewportHeight = 720,
  overscan = 20,
}) {
  if (!Number.isFinite(total) || total <= 0) {
    return { start: 0, end: 0, topPad: 0, bottomPad: 0 };
  }
  const count = Math.floor(total);
  const h = Number.isFinite(rowHeight) && rowHeight > 0 ? rowHeight : DEFAULT_VIRTUAL_ROW_HEIGHT;
  const vp = Number.isFinite(viewportHeight) && viewportHeight > 0 ? viewportHeight : 720;
  const off = Number.isFinite(scrollOffset) && scrollOffset > 0 ? scrollOffset : 0;
  const pad = Number.isFinite(overscan) && overscan > 0 ? Math.floor(overscan) : 0;

  const firstVisible = Math.floor(off / h);
  const visibleCount = Math.ceil(vp / h) + 1;
  const start = Math.max(0, Math.min(count, firstVisible - pad));
  const end = Math.max(start, Math.min(count, firstVisible + visibleCount + pad));
  return {
    start,
    end,
    topPad: Math.round(start * h),
    bottomPad: Math.round((count - end) * h),
  };
}

/**
 * Computes the visible cell range and full-span top/bottom spacer heights for a uniform square-cell
 * multi-column virtual grid. Spacer heights subtract one `rowGap` when non-zero because CSS Grid
 * inserts `rowGap` between the spacer track and the adjacent cell row.
 */
export function computeVirtualGridWindow({
  total,
  columns = 3,
  rowHeight = 124,
  rowGap = 3,
  scrollOffset = 0,
  viewportHeight = 720,
  overscanRows = 4,
}) {
  if (!Number.isFinite(total) || total <= 0) {
    return {
      startRow: 0,
      endRow: 0,
      startIndex: 0,
      endIndex: 0,
      topPad: 0,
      bottomPad: 0,
      totalHeight: 0,
    };
  }
  const count = Math.floor(total);
  const cols = Number.isFinite(columns) && columns >= 1 ? Math.floor(columns) : 3;
  const cellH = Number.isFinite(rowHeight) && rowHeight > 0 ? rowHeight : 124;
  const gap = Number.isFinite(rowGap) && rowGap >= 0 ? rowGap : 0;
  const pitch = cellH + gap;
  const vp = Number.isFinite(viewportHeight) && viewportHeight > 0 ? viewportHeight : 720;
  const off = Number.isFinite(scrollOffset) && scrollOffset > 0 ? scrollOffset : 0;
  const pad = Number.isFinite(overscanRows) && overscanRows >= 0 ? Math.floor(overscanRows) : 0;

  const totalRows = Math.ceil(count / cols);
  const totalHeight = Math.round(totalRows * cellH + Math.max(0, totalRows - 1) * gap);
  const firstVisibleRow = Math.min(totalRows, Math.floor(off / pitch));
  const visibleRows = Math.ceil((vp + gap) / pitch) + 1;
  const startRow = Math.max(0, Math.min(totalRows, firstVisibleRow - pad));
  const endRow = Math.max(startRow, Math.min(totalRows, firstVisibleRow + visibleRows + pad));

  const topPad = startRow > 0 ? Math.max(0, Math.round(startRow * pitch - gap)) : 0;
  const trailingRows = totalRows - endRow;
  const bottomPad = trailingRows > 0 ? Math.max(0, Math.round(trailingRows * pitch - gap)) : 0;

  return {
    startRow,
    endRow,
    startIndex: Math.min(count, startRow * cols),
    endIndex: Math.min(count, endRow * cols),
    topPad,
    bottomPad,
    totalHeight,
  };
}
