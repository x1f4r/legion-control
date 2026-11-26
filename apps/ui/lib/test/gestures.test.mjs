import assert from 'node:assert/strict';
import test from 'node:test';
import {
  classifyHorizontalSwipe,
  classifyRowSwipe,
  hasNativeEdgeBack,
  stepTab,
} from '../gestures.js';

test('classifyHorizontalSwipe distinguishes prev, next, edge-back, and ignores vertical drags', () => {
  assert.equal(classifyHorizontalSwipe({ dx: -80, dy: 10, elapsedMs: 180, startX: 200 }), 'next');
  assert.equal(classifyHorizontalSwipe({ dx: 80, dy: 10, elapsedMs: 180, startX: 200 }), 'prev');
  assert.equal(
    classifyHorizontalSwipe({ dx: 70, dy: 8, elapsedMs: 150, startX: 12, allowEdgeBack: true }),
    'edge-back',
  );
  // Edge back disabled when platform handles it natively.
  assert.equal(
    classifyHorizontalSwipe({ dx: 70, dy: 8, elapsedMs: 150, startX: 12, allowEdgeBack: false }),
    'prev',
  );
  // Diagonal / mostly vertical drag must never trigger horizontal navigation.
  assert.equal(classifyHorizontalSwipe({ dx: 60, dy: 55, elapsedMs: 180, startX: 100 }), null);
  // Tiny drag ignored.
  assert.equal(classifyHorizontalSwipe({ dx: 20, dy: 2, elapsedMs: 200, startX: 100 }), null);
});

test('classifyRowSwipe reveals and closes row actions without interfering with scroll', () => {
  assert.equal(classifyRowSwipe({ dx: -56, dy: 4, currentlyOpen: false }), 'reveal');
  assert.equal(classifyRowSwipe({ dx: -30, dy: 4, currentlyOpen: false }), null);
  assert.equal(classifyRowSwipe({ dx: 40, dy: 5, currentlyOpen: true }), 'close');
  assert.equal(classifyRowSwipe({ dx: -60, dy: 50, currentlyOpen: false }), null);
});

test('stepTab and hasNativeEdgeBack behave deterministically', () => {
  const tabs = ['machines', 'activity', 'actions'];
  assert.equal(stepTab(tabs, 'machines', 'next'), 'activity');
  assert.equal(stepTab(tabs, 'machines', 'prev'), null);
  assert.equal(stepTab(tabs, 'actions', 'next'), null);
  assert.equal(stepTab(tabs, 'actions', 'prev'), 'activity');

  assert.equal(hasNativeEdgeBack({ __LEGION_ANDROID__: {} }), true);
  assert.equal(hasNativeEdgeBack({}), false);
});
