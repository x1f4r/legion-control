import assert from 'node:assert/strict';
import test from 'node:test';
import { createSseParser } from '../sse.js';

test('createSseParser parses chunked events, ignores keep-alive comments, and emits final frame', () => {
  const frames = [];
  const parser = createSseParser((frame) => frames.push(frame));

  parser.push(': keep-alive\n\n');
  parser.push('event: event\ndata: {"kind":"stdout","da');
  parser.push('ta":"hello\\n"}\n\n');
  parser.push('event: result\r\ndata: {"exit_code":0}\r\n\r\n');
  parser.finish();

  assert.equal(frames.length, 2);
  assert.equal(frames[0].event, 'event');
  assert.deepEqual(frames[0].data, { kind: 'stdout', data: 'hello\n' });
  assert.equal(frames[1].event, 'result');
  assert.deepEqual(frames[1].data, { exit_code: 0 });
});

test('createSseParser handles trailing frame without blank line on finish', () => {
  const frames = [];
  const parser = createSseParser((frame) => frames.push(frame));
  parser.push('event: error\ndata: {"code":"busy","message":"Busy."}');
  parser.finish();

  assert.equal(frames.length, 1);
  assert.equal(frames[0].event, 'error');
  assert.equal(frames[0].data.code, 'busy');
});
