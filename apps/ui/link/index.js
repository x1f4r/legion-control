// Chooses the active link implementation: mock (?mock), native app (__TAURI__), or HTTP.

import { createAppLink } from './app.js';
import { createHttpLink } from './http.js';
import { createMockLink } from './mock.js';

export function createLink() {
  if (typeof window !== 'undefined') {
    const params = new URLSearchParams(window.location.search);
    if (params.has('mock')) {
      return createMockLink(params.get('mock') || '');
    }
    if (window.__TAURI__) {
      return createAppLink();
    }
  }
  return createHttpLink();
}
