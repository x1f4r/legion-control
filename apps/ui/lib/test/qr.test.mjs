import assert from 'node:assert/strict';
import test from 'node:test';
import { encodeQr, qrSvgData, reedSolomonRemainder } from '../qr.js';

test('reedSolomonRemainder matches ISO/IEC 18004 Version 1-M reference vector', () => {
  // Standard ISO/IEC 18004 Annex I / "HELLO WORLD" V1-M 16 data codewords -> 10 EC codewords.
  const data = Uint8Array.from([
    32, 91, 11, 120, 209, 114, 220, 77, 67, 64, 236, 17, 236, 17, 236, 17,
  ]);
  const expected = Uint8Array.from([196, 35, 39, 119, 235, 215, 231, 226, 93, 23]);
  assert.deepEqual(reedSolomonRemainder(data, 10), expected);
});

test('encodeQr produces valid finder, timing, dark module, and format strips for Version 1', () => {
  const qr = encodeQr('HELLO WORLD');
  assert.equal(qr.version, 1);
  assert.equal(qr.size, 21);

  // Top-left 7x7 finder outer border is all dark; separator column 7 is all light.
  for (let i = 0; i < 7; i += 1) {
    assert.equal(qr.modules[0][i], 1);
    assert.equal(qr.modules[6][i], 1);
    assert.equal(qr.modules[i][0], 1);
    assert.equal(qr.modules[i][6], 1);
    assert.equal(qr.modules[i][7], 0);
  }
  // Center 3x3 of top-left finder is dark.
  for (let r = 2; r <= 4; r += 1) {
    for (let c = 2; c <= 4; c += 1) {
      assert.equal(qr.modules[r][c], 1);
    }
  }
  // Dark module at (4*V + 9, 8) = (13, 8).
  assert.equal(qr.modules[13][8], 1);
  // Timing patterns alternate on row 6 and col 6.
  for (let i = 8; i <= 12; i += 1) {
    assert.equal(qr.modules[6][i], i % 2 === 0 ? 1 : 0);
    assert.equal(qr.modules[i][6], i % 2 === 0 ? 1 : 0);
  }
});

test('qrSvgData encodes realistic 360-byte Legion invite payloads cleanly', () => {
  const invite = `legion-invite-1.${'A'.repeat(340)}`;
  const svg = qrSvgData(invite, 4);
  assert.ok(svg.version >= 10 && svg.version <= 16, `unexpected version ${svg.version}`);
  assert.equal(svg.viewBoxSize, svg.size + 8);
  assert.ok(svg.path.startsWith('M4,4h7v1h-7z'), 'top-left finder should start at quiet zone offset');
});
