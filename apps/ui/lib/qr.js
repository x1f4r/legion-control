// Self-contained ISO/IEC 18004 QR Code encoder (Byte mode, Error Correction Level L, Versions 1..20).
// Zero external dependencies; renders to an SVG path string with no inline styles.

// Error Correction Level L parameters for QR Versions 1..20:
// [totalCodewords, ecCodewordsPerBlock, group1Blocks, group1DataCodewords, group2Blocks, group2DataCodewords]
const VERSION_L = [
  null,
  [26, 7, 1, 19, 0, 0], // 1
  [44, 10, 1, 34, 0, 0], // 2
  [70, 15, 1, 55, 0, 0], // 3
  [100, 20, 1, 80, 0, 0], // 4
  [134, 26, 1, 108, 0, 0], // 5
  [172, 18, 2, 68, 0, 0], // 6
  [196, 20, 2, 78, 0, 0], // 7
  [242, 24, 2, 97, 0, 0], // 8
  [292, 30, 2, 116, 0, 0], // 9
  [346, 18, 2, 68, 2, 69], // 10
  [404, 20, 4, 81, 0, 0], // 11
  [466, 24, 2, 92, 2, 93], // 12
  [532, 26, 4, 107, 0, 0], // 13
  [581, 30, 3, 115, 1, 116], // 14
  [655, 22, 5, 87, 1, 88], // 15
  [733, 24, 5, 98, 1, 99], // 16
  [815, 28, 1, 107, 5, 108], // 17
  [901, 30, 5, 120, 1, 121], // 18
  [991, 28, 3, 113, 4, 114], // 19
  [1085, 28, 3, 107, 5, 108], // 20
];

// Alignment pattern center coordinates for Versions 1..20.
const ALIGNMENT_CENTERS = [
  null,
  [], // 1
  [6, 18], // 2
  [6, 22], // 3
  [6, 26], // 4
  [6, 30], // 5
  [6, 34], // 6
  [6, 22, 38], // 7
  [6, 24, 42], // 8
  [6, 26, 46], // 9
  [6, 28, 50], // 10
  [6, 30, 54], // 11
  [6, 32, 58], // 12
  [6, 34, 62], // 13
  [6, 26, 46, 66], // 14
  [6, 26, 48, 70], // 15
  [6, 26, 50, 74], // 16
  [6, 30, 54, 78], // 17
  [6, 30, 56, 82], // 18
  [6, 30, 58, 86], // 19
  [6, 34, 62, 90], // 20
];

// GF(256) with primitive polynomial 0x11d (x^8 + x^4 + x^3 + x^2 + 1).
const GF_EXP = new Uint8Array(512);
const GF_LOG = new Uint8Array(256);
{
  let x = 1;
  for (let i = 0; i < 255; i += 1) {
    GF_EXP[i] = x;
    GF_LOG[x] = i;
    x <<= 1;
    if (x & 0x100) x ^= 0x11d;
  }
  for (let i = 255; i < 512; i += 1) {
    GF_EXP[i] = GF_EXP[i - 255];
  }
}

function gfMul(a, b) {
  if (a === 0 || b === 0) return 0;
  return GF_EXP[GF_LOG[a] + GF_LOG[b]];
}

function rsGeneratorPoly(degree) {
  let poly = new Uint8Array([1]);
  for (let i = 0; i < degree; i += 1) {
    const next = new Uint8Array(poly.length + 1);
    const root = GF_EXP[i];
    for (let j = 0; j < poly.length; j += 1) {
      next[j] ^= poly[j];
      next[j + 1] ^= gfMul(poly[j], root);
    }
    poly = next;
  }
  return poly;
}

export function reedSolomonRemainder(data, ecCount) {
  const gen = rsGeneratorPoly(ecCount);
  const rem = new Uint8Array(ecCount);
  for (let i = 0; i < data.length; i += 1) {
    const factor = data[i] ^ rem[0];
    for (let j = 0; j < ecCount - 1; j += 1) {
      rem[j] = rem[j + 1] ^ gfMul(gen[j + 1], factor);
    }
    rem[ecCount - 1] = gfMul(gen[ecCount], factor);
  }
  return rem;
}

function toUtf8Bytes(text) {
  if (typeof TextEncoder !== 'undefined') {
    return new TextEncoder().encode(String(text));
  }
  const str = unescape(encodeURIComponent(String(text)));
  const out = new Uint8Array(str.length);
  for (let i = 0; i < str.length; i += 1) out[i] = str.charCodeAt(i);
  return out;
}

function chooseVersion(byteLen) {
  for (let v = 1; v < VERSION_L.length; v += 1) {
    const [, , g1b, g1d, g2b, g2d] = VERSION_L[v];
    const dataCodewords = g1b * g1d + g2b * g2d;
    const countBits = v <= 9 ? 8 : 16;
    const neededBits = 4 + countBits + byteLen * 8;
    if (neededBits <= dataCodewords * 8) return v;
  }
  throw new Error(`QR payload too long (${byteLen} bytes).`);
}

function buildCodewords(bytes, version) {
  const [, ecPerBlock, g1b, g1d, g2b, g2d] = VERSION_L[version];
  const totalData = g1b * g1d + g2b * g2d;
  const bits = [];

  const pushBits = (val, len) => {
    for (let i = len - 1; i >= 0; i -= 1) {
      bits.push((val >>> i) & 1);
    }
  };

  // Byte mode indicator (0100) + character count + payload bytes.
  pushBits(0b0100, 4);
  pushBits(bytes.length, version <= 9 ? 8 : 16);
  for (let i = 0; i < bytes.length; i += 1) pushBits(bytes[i], 8);

  // Terminator (up to 4 zero bits) and pad to byte boundary.
  const maxBits = totalData * 8;
  const term = Math.min(4, maxBits - bits.length);
  for (let i = 0; i < term; i += 1) bits.push(0);
  while (bits.length % 8 !== 0) bits.push(0);

  const data = new Uint8Array(totalData);
  let byteIdx = 0;
  for (let i = 0; i < bits.length; i += 8) {
    let b = 0;
    for (let j = 0; j < 8; j += 1) b = (b << 1) | bits[i + j];
    data[byteIdx++] = b;
  }
  let padToggle = true;
  while (byteIdx < totalData) {
    data[byteIdx++] = padToggle ? 0xec : 0x11;
    padToggle = !padToggle;
  }

  // Split into RS blocks and interleave data then EC codewords.
  const dataBlocks = [];
  const ecBlocks = [];
  let offset = 0;
  for (let i = 0; i < g1b; i += 1) {
    const slice = data.subarray(offset, offset + g1d);
    offset += g1d;
    dataBlocks.push(slice);
    ecBlocks.push(reedSolomonRemainder(slice, ecPerBlock));
  }
  for (let i = 0; i < g2b; i += 1) {
    const slice = data.subarray(offset, offset + g2d);
    offset += g2d;
    dataBlocks.push(slice);
    ecBlocks.push(reedSolomonRemainder(slice, ecPerBlock));
  }

  const out = [];
  const maxDataLen = Math.max(g1d, g2d);
  for (let i = 0; i < maxDataLen; i += 1) {
    for (const block of dataBlocks) {
      if (i < block.length) out.push(block[i]);
    }
  }
  for (let i = 0; i < ecPerBlock; i += 1) {
    for (const block of ecBlocks) {
      out.push(block[i]);
    }
  }
  return Uint8Array.from(out);
}

function formatBitsForMask(mask) {
  // Error Correction Level L = 01
  const data = (0b01 << 3) | (mask & 0b111);
  let rem = data << 10;
  const poly = 0b10100110111;
  for (let i = 14; i >= 10; i -= 1) {
    if ((rem >>> i) & 1) rem ^= poly << (i - 10);
  }
  return ((data << 10) | rem) ^ 0b101010000010010;
}

function versionBits(version) {
  let rem = version << 12;
  const poly = 0b1111100100101;
  for (let i = 17; i >= 12; i -= 1) {
    if ((rem >>> i) & 1) rem ^= poly << (i - 12);
  }
  return (version << 12) | rem;
}

function maskBit(mask, r, c) {
  switch (mask) {
    case 0:
      return (r + c) % 2 === 0;
    case 1:
      return r % 2 === 0;
    case 2:
      return c % 3 === 0;
    case 3:
      return (r + c) % 3 === 0;
    case 4:
      return (Math.floor(r / 2) + Math.floor(c / 3)) % 2 === 0;
    case 5:
      return ((r * c) % 2) + ((r * c) % 3) === 0;
    case 6:
      return (((r * c) % 2) + ((r * c) % 3)) % 2 === 0;
    case 7:
      return (((r + c) % 2) + ((r * c) % 3)) % 2 === 0;
    default:
      return false;
  }
}

function createMatrix(version) {
  const size = version * 4 + 17;
  const modules = Array.from({ length: size }, () => new Uint8Array(size));
  const reserved = Array.from({ length: size }, () => new Uint8Array(size));

  const setFunc = (r, c, dark) => {
    if (r < 0 || c < 0 || r >= size || c >= size) return;
    modules[r][c] = dark ? 1 : 0;
    reserved[r][c] = 1;
  };

  // Finder patterns + separators.
  const placeFinder = (top, left) => {
    for (let dr = -1; dr <= 7; dr += 1) {
      for (let dc = -1; dc <= 7; dc += 1) {
        const r = top + dr;
        const c = left + dc;
        if (r < 0 || c < 0 || r >= size || c >= size) continue;
        const inOuter = dr >= 0 && dr <= 6 && dc >= 0 && dc <= 6;
        const onBorder = dr === 0 || dr === 6 || dc === 0 || dc === 6;
        const inCenter = dr >= 2 && dr <= 4 && dc >= 2 && dc <= 4;
        setFunc(r, c, inOuter && (onBorder || inCenter));
      }
    }
  };
  placeFinder(0, 0);
  placeFinder(0, size - 7);
  placeFinder(size - 7, 0);

  // Timing patterns.
  for (let i = 8; i < size - 8; i += 1) {
    setFunc(6, i, i % 2 === 0);
    setFunc(i, 6, i % 2 === 0);
  }

  // Alignment patterns.
  const centers = ALIGNMENT_CENTERS[version] || [];
  for (const cr of centers) {
    for (const cc of centers) {
      if (reserved[cr][cc]) continue;
      for (let dr = -2; dr <= 2; dr += 1) {
        for (let dc = -2; dc <= 2; dc += 1) {
          const dist = Math.max(Math.abs(dr), Math.abs(dc));
          setFunc(cr + dr, cc + dc, dist !== 1);
        }
      }
    }
  }

  // Reserve format info strips + dark module.
  for (let i = 0; i <= 8; i += 1) {
    if (i !== 6) {
      setFunc(8, i, false);
      setFunc(i, 8, false);
    }
  }
  for (let i = size - 8; i < size; i += 1) {
    setFunc(8, i, false);
    setFunc(i, 8, false);
  }
  setFunc(size - 8, 8, true);

  // Version information for Version >= 7.
  if (version >= 7) {
    const vb = versionBits(version);
    for (let i = 0; i < 18; i += 1) {
      const bit = Boolean((vb >>> i) & 1);
      const a = Math.floor(i / 3);
      const b = size - 11 + (i % 3);
      setFunc(a, b, bit);
      setFunc(b, a, bit);
    }
  }

  return { size, modules, reserved };
}

function writeFormat(modules, size, mask) {
  const bits = formatBitsForMask(mask);
  const bitAt = (i) => ((bits >>> i) & 1);

  // Top-left around finder.
  const topLeftRow = [0, 1, 2, 3, 4, 5, 7, 8, 8, 8, 8, 8, 8, 8, 8];
  const topLeftCol = [8, 8, 8, 8, 8, 8, 8, 8, 7, 5, 4, 3, 2, 1, 0];
  for (let i = 0; i < 15; i += 1) {
    modules[topLeftRow[i]][topLeftCol[i]] = bitAt(i);
  }

  // Split across top-right (bits 0..7) and bottom-left (bits 8..14).
  for (let i = 0; i < 8; i += 1) {
    modules[8][size - 1 - i] = bitAt(i);
  }
  for (let i = 0; i < 7; i += 1) {
    modules[size - 7 + i][8] = bitAt(8 + i);
  }
}

function placeData(modules, reserved, size, codewords) {
  let bitIdx = 0;
  const totalBits = codewords.length * 8;
  let upward = true;

  for (let right = size - 1; right >= 1; right -= 2) {
    if (right === 6) right = 5;
    for (let step = 0; step < size; step += 1) {
      const r = upward ? size - 1 - step : step;
      for (let colOff = 0; colOff < 2; colOff += 1) {
        const c = right - colOff;
        if (reserved[r][c]) continue;
        let dark = 0;
        if (bitIdx < totalBits) {
          dark = (codewords[bitIdx >>> 3] >>> (7 - (bitIdx & 7))) & 1;
          bitIdx += 1;
        }
        modules[r][c] = dark;
      }
    }
    upward = !upward;
  }
}

function penaltyScore(modules, size) {
  let score = 0;
  let darkCount = 0;

  // Rule 1: 5+ same-color runs in rows & columns.
  for (let r = 0; r < size; r += 1) {
    let runColor = modules[r][0];
    let runLen = 1;
    if (runColor) darkCount += 1;
    for (let c = 1; c < size; c += 1) {
      const v = modules[r][c];
      if (v) darkCount += 1;
      if (v === runColor) {
        runLen += 1;
      } else {
        if (runLen >= 5) score += 3 + (runLen - 5);
        runColor = v;
        runLen = 1;
      }
    }
    if (runLen >= 5) score += 3 + (runLen - 5);
  }

  for (let c = 0; c < size; c += 1) {
    let runColor = modules[0][c];
    let runLen = 1;
    for (let r = 1; r < size; r += 1) {
      const v = modules[r][c];
      if (v === runColor) {
        runLen += 1;
      } else {
        if (runLen >= 5) score += 3 + (runLen - 5);
        runColor = v;
        runLen = 1;
      }
    }
    if (runLen >= 5) score += 3 + (runLen - 5);
  }

  // Rule 2: 2x2 blocks of same color.
  for (let r = 0; r < size - 1; r += 1) {
    for (let c = 0; c < size - 1; c += 1) {
      const v = modules[r][c];
      if (v === modules[r][c + 1] && v === modules[r + 1][c] && v === modules[r + 1][c + 1]) {
        score += 3;
      }
    }
  }

  // Rule 4: balance of dark vs light.
  const pct = (darkCount * 100) / (size * size);
  const k = Math.floor(Math.abs(pct - 50) / 5);
  score += k * 10;

  return score;
}

/**
 * Encodes `text` as a QR code matrix (Level L, Byte mode).
 */
export function encodeQr(text) {
  const bytes = toUtf8Bytes(text);
  const version = chooseVersion(bytes.length);
  const codewords = buildCodewords(bytes, version);
  const base = createMatrix(version);
  placeData(base.modules, base.reserved, base.size, codewords);

  let bestMask = 0;
  let bestScore = Infinity;
  let bestModules = null;

  for (let mask = 0; mask < 8; mask += 1) {
    const candidate = base.modules.map((row) => row.slice());
    for (let r = 0; r < base.size; r += 1) {
      for (let c = 0; c < base.size; c += 1) {
        if (!base.reserved[r][c] && maskBit(mask, r, c)) {
          candidate[r][c] ^= 1;
        }
      }
    }
    writeFormat(candidate, base.size, mask);
    const s = penaltyScore(candidate, base.size);
    if (s < bestScore) {
      bestScore = s;
      bestMask = mask;
      bestModules = candidate;
    }
  }

  return {
    version,
    mask: bestMask,
    size: base.size,
    modules: bestModules,
  };
}

/**
 * Builds an SVG `<path d="...">` string covering all dark modules with a quiet zone border.
 */
export function qrSvgData(text, quietZone = 4) {
  const qr = encodeQr(text);
  const total = qr.size + quietZone * 2;
  const segments = [];
  for (let r = 0; r < qr.size; r += 1) {
    const row = qr.modules[r];
    let c = 0;
    while (c < qr.size) {
      if (!row[c]) {
        c += 1;
        continue;
      }
      const start = c;
      while (c < qr.size && row[c]) c += 1;
      const len = c - start;
      segments.push(`M${start + quietZone},${r + quietZone}h${len}v1h-${len}z`);
    }
  }
  return {
    version: qr.version,
    size: qr.size,
    viewBoxSize: total,
    path: segments.join(''),
  };
}
