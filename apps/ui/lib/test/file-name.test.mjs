// Unit and property tests for `apps/ui/lib/file-name.js`.

import assert from 'node:assert/strict';
import test from 'node:test';
import { applySearchHighlight, fitName, fitNameList, isNaturalBreak, splitNameEnding, toGraphemes } from '../file-name.js';

// Deterministic fake measurer: proportional widths per grapheme so tests don't need a browser.
function makeFakeMeasure(baseWidth = 7) {
  return (text) => {
    let w = 0;
    for (const g of toGraphemes(text)) {
      if (g === ' ') w += baseWidth * 0.45;
      else if (g === '.' || g === 'i' || g === 'l' || g === '1') w += baseWidth * 0.55;
      else if (g === 'W' || g === 'M' || g === 'm' || g === 'w') w += baseWidth * 1.25;
      else if (g === '…') w += baseWidth * 1.0;
      else if (g.length > 2) w += baseWidth * 1.5; // wide emoji / ZWJ cluster
      else w += baseWidth;
    }
    return w;
  };
}

function assertValidFit(res, measure, width, maxLines, label = '') {
  assert.ok(res.lines.length >= 1 && res.lines.length <= maxLines, `${label}: line count ${res.lines.length} > ${maxLines}`);
  assert.ok(!res.text.endsWith('…'), `${label}: trailing ellipsis in "${res.text}"`);
  for (let i = 0; i < res.lineTexts.length; i += 1) {
    const lw = measure(res.lineTexts[i]);
    assert.ok(
      lw <= width + 1e-6,
      `${label}: line ${i} "${res.lineTexts[i]}" measured ${lw} > ${width}`,
    );
  }
  const reconstructed = res.lines.map((spans) => spans.map((s) => s.text).join('')).join('');
  assert.equal(reconstructed, res.text, `${label}: span text must equal res.text`);
}

function dimPrefixText(res) {
  let out = '';
  for (const spans of res.lines) {
    for (const s of spans) {
      if (s.dim) out += s.text;
      else return out;
    }
  }
  return out;
}

test('toGraphemes preserves emoji ZWJ clusters, combining marks, and RTL text', () => {
  const emoji = toGraphemes('🎉_👨‍👩‍👧‍👦_👍🏽.mp4');
  assert.deepEqual(emoji, ['🎉', '_', '👨‍👩‍👧‍👦', '_', '👍🏽', '.', 'm', 'p', '4']);

  const combining = toGraphemes('cafe\u0301_re\u0301sume\u0301.pdf');
  assert.equal(combining[3], 'e\u0301');
  assert.equal(combining[6], 'e\u0301');
  assert.equal(combining[10], 'e\u0301');

  const rtl = toGraphemes('تسجيل_الشاشة_2026.mp4');
  assert.equal(rtl.join(''), 'تسجيل_الشاشة_2026.mp4');
});

test('isNaturalBreak identifies space, _, -, ., letter/digit, and lower/upper transitions', () => {
  const g = toGraphemes('myFile_2026-11.txt');
  const breaks = [];
  for (let i = 1; i < g.length; i += 1) {
    if (isNaturalBreak(g, i)) breaks.push(g.slice(0, i).join('') + '|' + g.slice(i).join(''));
  }
  assert.deepEqual(breaks, [
    'my|File_2026-11.txt',
    'myFile_|2026-11.txt',
    'myFile_2026-|11.txt',
    'myFile_2026-11.|txt',
  ]);

  const ld = toGraphemes('Report2026Final');
  const ldBreaks = [];
  for (let i = 1; i < ld.length; i += 1) {
    if (isNaturalBreak(ld, i)) ldBreaks.push(ld.slice(0, i).join('') + '|' + ld.slice(i).join(''));
  }
  assert.deepEqual(ldBreaks, ['Report|2026Final', 'Report2026|Final']);
});

test('splitNameEnding handles normal files, extensionless names, dotfiles, and multi-dot names', () => {
  const normal = toGraphemes('IMG_20261125_140311.jpg');
  const s1 = splitNameEnding(normal);
  assert.equal(normal.slice(s1.endingStart).join(''), '5_140311.jpg');

  const noExt = toGraphemes('deployment-checklist-rev1');
  const s2 = splitNameEnding(noExt);
  assert.equal(s2.extLen, 0);
  assert.equal(noExt.slice(s2.endingStart).join(''), 'ist-rev1');

  const dotfile = toGraphemes('.gitignore');
  const s3 = splitNameEnding(dotfile);
  assert.equal(s3.extLen, 0);
  assert.equal(dotfile.slice(s3.endingStart).join(''), 'itignore');

  const dotWithExt = toGraphemes('.env.production.local');
  const s4 = splitNameEnding(dotWithExt);
  assert.equal(dotWithExt.slice(s4.dotIndex).join(''), '.local');
  assert.equal(dotWithExt.slice(s4.endingStart).join(''), 'oduction.local');
});

test('shared lead >= 10 dims snapped to natural break even when full name fits', () => {
  const measure = makeFakeMeasure(7);
  const a = 'IMG_20261125_140311.jpg';
  const b = 'IMG_20261125_140312.jpg';

  const fit = fitName(a, null, b, measure, 300, 2);
  assertValidFit(fit, measure, 300, 2, 'IMG full fit');
  assert.equal(fit.truncated, false);
  assert.equal(fit.text, a);
  assert.equal(dimPrefixText(fit), 'IMG_20261125_');
  assert.deepEqual(fit.lines, [
    [
      { text: 'IMG_20261125_', dim: true },
      { text: '140311.jpg', dim: false },
    ],
  ]);

  // Search results disable dimming while keeping layout identical.
  const searchFit = fitName(a, null, b, measure, 300, 2, { dim: false });
  assert.equal(dimPrefixText(searchFit), '');
  assert.equal(searchFit.text, a);
});

test('shared lead < 10 does not dim; shared lead >= 8 cuts after first 6 chars', () => {
  const measure = makeFakeMeasure(7);
  // Shared lead of 9 chars ("Report_20") -> cuts at 6 ("Report…"), does NOT dim (< 10).
  const a = 'Report_2026_alpha_quarterly_summary_final_v1.pdf';
  const b = 'Report_2019_other_summary.pdf';
  const fit = fitName(a, null, b, measure, 180, 1);
  assertValidFit(fit, measure, 180, 1, 'shared 9');
  assert.equal(fit.truncated, true);
  assert.equal(dimPrefixText(fit), '');
  assert.ok(fit.text.startsWith('Report…'), `Expected cut after 6 chars, got "${fit.text}"`);
  assert.ok(fit.text.endsWith('final_v1.pdf'), `Expected ending intact, got "${fit.text}"`);
});

test('shared lead < 8 puts ellipsis in the middle of the stem', () => {
  const measure = makeFakeMeasure(7);
  const a = 'Alpha_photogrammetry_orthomosaic_north_ridge_verified_v12.txt';
  const b = 'Beta_unrelated_document.pdf';
  const fit = fitName(a, null, b, measure, 180, 1);
  assertValidFit(fit, measure, 180, 1, 'middle ellipsis');
  assert.equal(fit.truncated, true);
  assert.equal(dimPrefixText(fit), '');
  const idx = fit.text.indexOf('…');
  assert.ok(idx > 6, `Expected middle-of-stem ellipsis past index 6, got "${fit.text}"`);
  assert.ok(fit.text.endsWith('fied_v12.txt'), `Expected ending intact, got "${fit.text}"`);
});

test('differences at start, middle, end, and only in extension all stay distinguishable', () => {
  const measure = makeFakeMeasure(7);

  // Difference at start.
  const startA = 'A_very_long_annual_infrastructure_cost_breakdown_2026.pdf';
  const startB = 'B_very_long_annual_infrastructure_cost_breakdown_2026.pdf';
  const sA = fitName(startA, null, startB, measure, 180, 1);
  const sB = fitName(startB, startA, null, measure, 180, 1);
  assertValidFit(sA, measure, 180, 1, 'start A');
  assertValidFit(sB, measure, 180, 1, 'start B');
  assert.notEqual(sA.text, sB.text);
  assert.ok(sA.text.startsWith('A_'));
  assert.ok(sB.text.startsWith('B_'));

  // Difference in middle with shared lead AND shared ending, even on narrow 1-line width.
  const midA = 'Quarterly_Architecture_Review_Q1_2026_Final_Approved.pdf';
  const midB = 'Quarterly_Architecture_Review_Q2_2026_Final_Approved.pdf';
  for (const w of [140, 200, 280]) {
    const mA = fitName(midA, null, midB, measure, w, 1);
    const mB = fitName(midB, midA, null, measure, w, 1);
    assertValidFit(mA, measure, w, 1, `mid A @${w}`);
    assertValidFit(mB, measure, w, 1, `mid B @${w}`);
    assert.notEqual(mA.text, mB.text, `Middle difference collapsed at ${w}px: "${mA.text}"`);
    assert.ok(mA.text.endsWith('Approved.pdf'), `Ending missing at ${w}px: "${mA.text}"`);
  }

  // Difference at end (Screen recordings).
  const endA = 'Screen Recording 2026-11-25 at 14.01.12.mov';
  const endB = 'Screen Recording 2026-11-25 at 14.02.45.mov';
  const eA = fitName(endA, null, endB, measure, 160, 1);
  const eB = fitName(endB, endA, null, measure, 160, 1);
  assertValidFit(eA, measure, 160, 1, 'end A');
  assertValidFit(eB, measure, 160, 1, 'end B');
  assert.notEqual(eA.text, eB.text);
  assert.ok(eA.text.startsWith('Screen…'));
  assert.ok(eA.text.endsWith('14.01.12.mov'));
  assert.ok(eB.text.endsWith('14.02.45.mov'));

  // Difference only in extension.
  const extA = 'Screen Recording 2026-11-25 at 14.01.12.mov';
  const extB = 'Screen Recording 2026-11-25 at 14.01.12.mp4';
  const xA = fitName(extA, null, extB, measure, 160, 1);
  const xB = fitName(extB, extA, null, measure, 160, 1);
  assertValidFit(xA, measure, 160, 1, 'ext A');
  assertValidFit(xB, measure, 160, 1, 'ext B');
  assert.notEqual(xA.text, xB.text);
  assert.ok(xA.text.endsWith('.mov'));
  assert.ok(xB.text.endsWith('.mp4'));
  // Full fit dims up to the dot so only `mov` / `mp4` stands out.
  const xFull = fitName(extA, null, extB, measure, 400, 1);
  assert.equal(dimPrefixText(xFull), 'Screen Recording 2026-11-25 at 14.01.12.');
});

test('handles no extension, dotfiles, very long extension, overlong word, case-only diff, emoji, combining marks, RTL', () => {
  const measure = makeFakeMeasure(7);

  // No extension.
  const noExtA = 'deployment-checklist-europe-west-primary-cluster-rollout-plan-rev1';
  const noExtB = 'deployment-checklist-europe-west-primary-cluster-rollout-plan-rev2';
  const nA = fitName(noExtA, null, noExtB, measure, 180, 1);
  const nB = fitName(noExtB, noExtA, null, measure, 180, 1);
  assertValidFit(nA, measure, 180, 1, 'noExt A');
  assertValidFit(nB, measure, 180, 1, 'noExt B');
  assert.notEqual(nA.text, nB.text);
  assert.ok(nA.text.endsWith('lan-rev1'));
  assert.ok(nB.text.endsWith('lan-rev2'));

  // Dotfiles.
  const dotA = '.very_long_hidden_configuration_profile_primary_node_alpha';
  const dotB = '.very_long_hidden_configuration_profile_primary_node_beta';
  const dA = fitName(dotA, null, dotB, measure, 160, 1);
  const dB = fitName(dotB, dotA, null, measure, 160, 1);
  assertValidFit(dA, measure, 160, 1, 'dot A');
  assertValidFit(dB, measure, 160, 1, 'dot B');
  assert.notEqual(dA.text, dB.text);
  assert.ok(dA.text.endsWith('de_alpha'));
  assert.ok(dB.text.endsWith('ode_beta'));

  // Very long extension (wider than 120px 1-line!).
  const longExtA = 'backup_snapshot_a.supercalifragilisticexpialidociouscustomextensionv1';
  const longExtB = 'backup_snapshot_b.supercalifragilisticexpialidociouscustomextensionv2';
  const leA = fitName(longExtA, null, longExtB, measure, 120, 1);
  const leB = fitName(longExtB, longExtA, null, measure, 120, 1);
  assertValidFit(leA, measure, 120, 1, 'longExt A');
  assertValidFit(leB, measure, 120, 1, 'longExt B');
  assert.notEqual(leA.text, leB.text);
  assert.ok(leA.text.endsWith('v1'));
  assert.ok(leB.text.endsWith('v2'));

  // One word longer than a line: wraps cleanly inside the word without overflowing width.
  const oneWord = 'supercalifragilisticexpialidociousverylongtoken01.txt';
  const ow = fitName(oneWord, null, null, measure, 140, 2);
  assertValidFit(ow, measure, 140, 2, 'one word');
  assert.equal(ow.lines.length, 2);
  assert.ok(ow.text.endsWith('gtoken01.txt'));

  // Names identical except for case.
  const caseA = 'Quarterly_Report_2026_Final_Archive_Summary.pdf';
  const caseB = 'Quarterly_Report_2026_final_Archive_Summary.pdf';
  const cA = fitName(caseA, null, caseB, measure, 160, 1);
  const cB = fitName(caseB, caseA, null, measure, 160, 1);
  assertValidFit(cA, measure, 160, 1, 'case A');
  assertValidFit(cB, measure, 160, 1, 'case B');
  assert.notEqual(cA.text, cB.text);

  // Emoji (ZWJ family + skin tone modifier) never split mid-grapheme.
  const emA = '🎉_family_👨‍👩‍👧‍👦_holiday_2026_11_25_clip_👍🏽_01.mp4';
  const emB = '🎉_family_👨‍👩‍👧‍👦_holiday_2026_11_25_clip_👍🏽_02.mp4';
  const emFitA = fitName(emA, null, emB, measure, 140, 1);
  const emFitB = fitName(emB, emA, null, measure, 140, 1);
  assertValidFit(emFitA, measure, 140, 1, 'emoji A');
  assertValidFit(emFitB, measure, 140, 1, 'emoji B');
  assert.notEqual(emFitA.text, emFitB.text);
  assert.ok(!/[\uD800-\uDFFF]/.test(emFitA.text.replace(/\p{Extended_Pictographic}|\uFE0F|\u200D|\p{Emoji_Modifier}/gu, '')));
  assert.ok(emFitA.text.includes('👍🏽_01.mp4'));

  // Combining marks stay attached to their base characters.
  const combA = 'cafe\u0301_re\u0301sume\u0301_documentation_2026_e\u0301dition_01.pdf';
  const combB = 'cafe\u0301_re\u0301sume\u0301_documentation_2026_e\u0301dition_02.pdf';
  const cbA = fitName(combA, null, combB, measure, 150, 1);
  const cbB = fitName(combB, combA, null, measure, 150, 1);
  assertValidFit(cbA, measure, 150, 1, 'combining A');
  assertValidFit(cbB, measure, 150, 1, 'combining B');
  assert.notEqual(cbA.text, cbB.text);
  // Every combining acute accent must still follow an 'e', never stand orphaned after '…'.
  assert.ok(!cbA.text.includes('…\u0301'), `Orphaned combining mark in "${cbA.text}"`);
  assert.ok(cbA.text.endsWith('e\u0301dition_01.pdf'));

  // RTL Arabic and Hebrew names.
  const rtlA = 'تسجيل_الشاشة_2026_11_25_الساعة_14_01_12.mp4';
  const rtlB = 'تسجيل_الشاشة_2026_11_25_الساعة_14_02_45.mp4';
  const rA = fitName(rtlA, null, rtlB, measure, 150, 1);
  const rB = fitName(rtlB, rtlA, null, measure, 150, 1);
  assertValidFit(rA, measure, 150, 1, 'RTL A');
  assertValidFit(rB, measure, 150, 1, 'RTL B');
  assert.notEqual(rA.text, rB.text);
  assert.ok(rA.text.endsWith('14_01_12.mp4'));
  assert.ok(rB.text.endsWith('14_02_45.mp4'));
});

test('property test: generated sets with shared leads and shared endings across 120–600px and 1–3 lines', () => {
  // Deterministic PRNG (Mulberry32) so failures are 100% reproducible.
  function mulberry32(seed) {
    let a = seed >>> 0;
    return () => {
      a = (a + 0x6d2b79f5) >>> 0;
      let t = Math.imul(a ^ (a >>> 15), 1 | a);
      t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
  }

  const measure = makeFakeMeasure(6.5);
  const rng = mulberry32(0x20261125);
  const pick = (arr) => arr[Math.floor(rng() * arr.length)];

  const leads = [
    'Screen Recording 2026-11-25 at ',
    'IMG_20261125_',
    'Quarterly_Architecture_Review_',
    'deployment-checklist-europe-west-primary-cluster-',
    'backup_2026_north_region_archive_',
    '.hidden_service_configuration_cluster_',
    'cafe\u0301_re\u0301sume\u0301_archive_2026_',
    'تسجيل_الشاشة_2026_11_25_',
    '',
  ];
  const middles = [
    'Q1', 'Q2', 'Q3', 'Q4', 'alpha', 'beta', 'gamma', 'delta',
    'draft_01', 'draft_02', 'revA', 'revB', 'node01', 'node02', 'Final', 'final',
  ];
  const tails = [
    '_2026_Final_Approved',
    '_verified_release_candidate',
    '_export_master_copy',
    '_140311',
    '_140312',
    '-rollout-plan-rev1',
    '-rollout-plan-rev2',
    '',
  ];
  const exts = ['.pdf', '.mov', '.jpg', '.mp4', '.txt', '.tar.gz', ''];

  const widths = [120, 150, 180, 240, 320, 450, 600];
  const lineCounts = [1, 2, 3];

  for (let trial = 0; trial < 25; trial += 1) {
    const set = new Set();
    const sharedLead = pick(leads);
    const sharedTail = pick(tails);
    const sharedExt = pick(exts);

    // Cluster 1: shared lead + shared tail, differing only in middle.
    for (let i = 0; i < 6; i += 1) {
      set.add(`${sharedLead}${middles[(trial + i) % middles.length]}${sharedTail}${sharedExt}`);
    }
    // Cluster 2: shared lead + differing ends.
    for (let i = 0; i < 6; i += 1) {
      set.add(`${sharedLead}item_${String(i + 1).padStart(2, '0')}_${String(10 + i * 3)}${sharedExt}`);
    }
    // Cluster 3: multi-group shared leads AND shared endings (tests non-adjacent collision resolution!).
    for (const grp of ['G1', 'G2', 'G3']) {
      for (const num of ['01', '02', '03']) {
        set.add(`Project_Long_Shared_Prefix_${grp}_Common_Middle_Section_${num}.pdf`);
      }
    }
    // Cluster 4: random standalone names.
    for (let i = 0; i < 6; i += 1) {
      const l = pick(leads);
      const m = pick(middles);
      const t = pick(tails);
      const e = pick(exts);
      set.add(`${l}${m}_${trial}_${i}${t}${e}`);
    }

    const names = Array.from(set).sort((a, b) => a.localeCompare(b));

    for (const width of widths) {
      for (const maxLines of lineCounts) {
        const fitted = fitNameList(names, measure, width, maxLines);
        assert.equal(fitted.length, names.length);

        const seenDrawn = new Map();
        for (let i = 0; i < names.length; i += 1) {
          const orig = names[i];
          const res = fitted[i];
          const ctx = `trial=${trial} w=${width} lines=${maxLines} name="${orig}" drawn="${res.text}"`;

          assertValidFit(res, measure, width, maxLines, ctx);

          // Invariant 1: no two different names produce the same drawn text.
          if (seenDrawn.has(res.text)) {
            assert.fail(
              `Collision at ${ctx}: same drawn text "${res.text}" as "${seenDrawn.get(res.text)}"`,
            );
          }
          seenDrawn.set(res.text, orig);

          // Invariant 2: the ending (extension + 8 chars before it, or last 8 chars) is present
          // whenever the ending itself fits in the width budget.
          const g = toGraphemes(orig);
          const { endingStart } = splitNameEnding(g);
          const endingStr = g.slice(endingStart).join('');
          if (measure(`…${endingStr}`) <= width * maxLines * 0.78) {
            assert.ok(
              res.text.endsWith(endingStr),
              `Ending "${endingStr}" missing in ${ctx}`,
            );
          }
        }
      }
    }
  }
});

test('identical neighbours do not dim the whole name; non-adjacent collisions in unsorted lists resolve', () => {
  const measure = makeFakeMeasure(7);

  // Identical names (e.g. same filename on two machines in Favourites or Search) must not dim.
  const dup = fitName('Quarterly_Report_2026_Final.pdf', 'Quarterly_Report_2026_Final.pdf', null, measure, 300, 2);
  assert.equal(dimPrefixText(dup), '');
  assert.equal(dup.text, 'Quarterly_Report_2026_Final.pdf');

  // Unsorted / date-sorted listing where colliding reports are separated by unrelated files.
  const unsorted = [
    'Quarterly_Architecture_Review_Q1_2026_Final_Approved.pdf',
    'vacation_photo.jpg',
    'Quarterly_Architecture_Review_Q2_2026_Final_Approved.pdf',
    'notes.txt',
    'Quarterly_Architecture_Review_Q3_2026_Final_Approved.pdf',
  ];
  for (const width of [140, 200, 260]) {
    const fitted = fitNameList(unsorted, measure, width, 1);
    const drawn = fitted.map((r) => r.text);
    assert.equal(new Set(drawn).size, unsorted.length, `Unsorted collision at ${width}px: ${JSON.stringify(drawn)}`);
  }
});

test('applySearchHighlight highlights query terms across wrapped lines without dimming', () => {
  const measure = makeFakeMeasure(7);
  const fit = fitName('Screen Recording 2026-11-25 at 14.01.12.mov', null, null, measure, 180, 2, { dim: false });
  const hi = applySearchHighlight(fit.lines, 'recording 14.01');
  const matched = hi.flat().filter((s) => s.match).map((s) => s.text.toLowerCase());
  assert.ok(matched.join('').includes('recording'));
  assert.ok(matched.join('').includes('14.01'));
  assert.equal(hi.flat().some((s) => s.dim), false);
});

