import assert from 'node:assert/strict';
import test from 'node:test';
import { createCache } from '../cache.js';

function memoryStorage() {
  const map = new Map();
  return {
    get length() {
      return map.size;
    },
    key(i) {
      return Array.from(map.keys())[i] ?? null;
    },
    getItem(k) {
      return map.has(k) ? map.get(k) : null;
    },
    setItem(k, v) {
      map.set(k, String(v));
    },
    removeItem(k) {
      map.delete(k);
    },
  };
}

test('createCache persists to storage, restores stale data, and checks freshness', () => {
  const storage = memoryStorage();
  const c1 = createCache(storage);
  c1.set('machines', [{ name: 'tower' }], 1000);
  assert.equal(c1.isFresh('machines', 5000, 3000), true);
  assert.equal(c1.isFresh('machines', 5000, 7000), false);

  const c2 = createCache(storage);
  assert.deepEqual(c2.get('machines'), { value: [{ name: 'tower' }], updatedAt: 1000 });

  c2.delete('machines');
  assert.equal(c2.get('machines'), null);

  c2.set('a', 1);
  c2.set('b', 2);
  storage.setItem('unrelated', 'keep');
  c2.clear();
  assert.equal(c2.get('a'), null);
  assert.equal(storage.getItem('unrelated'), 'keep');
});
