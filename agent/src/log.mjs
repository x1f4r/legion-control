// Append-only log at <base>/legionctl.log, rotated at 1 MB.
//
// Logging must never be able to fail a command, and it must never write to
// stdout: stdout carries exactly one JSON object per invocation.

import fs from 'node:fs';
import { basePath, logPath } from './config.mjs';

const MAX_BYTES = 1024 * 1024;

function rotateIfNeeded(file) {
  try {
    const stat = fs.statSync(file);
    if (stat.size < MAX_BYTES) return;
    // Single generation is enough: the file is a breadcrumb trail, not an audit log.
    fs.rmSync(`${file}.1`, { force: true });
    fs.renameSync(file, `${file}.1`);
  } catch {
    /* file missing, or rotation lost a race: keep appending */
  }
}

/**
 * Append one line. `command` is the CLI verb so lines can be grepped per command.
 */
export function log(message, command = '-') {
  try {
    fs.mkdirSync(basePath(), { recursive: true });
    const file = logPath();
    rotateIfNeeded(file);
    const line = `${new Date().toISOString()} [${command}] ${String(message).replace(/\s*\n\s*/g, ' | ')}\n`;
    fs.appendFileSync(file, line, 'utf8');
  } catch {
    /* never let logging break a command */
  }
}

/** Human-readable progress goes to stderr; stdout stays reserved for JSON. */
export function note(message, command = '-') {
  log(message, command);
  try {
    process.stderr.write(`${message}\n`);
  } catch {
    /* stderr can be closed on a detached SSH session */
  }
}

/**
 * The tail of the agent log, parsed back into fields.
 *
 * Lines the agent wrote carry a timestamp and the command that wrote them.
 * Anything else in the file — a stray line from a child process that inherited
 * the handle — is returned with nulls rather than dropped, because a diagnostic
 * log that quietly hides the lines it does not recognise is worse than useless.
 */
export function readLogLines({ lines = 100, file = logPath() } = {}) {
  let text;
  try {
    text = fs.readFileSync(file, 'utf8');
  } catch (err) {
    if (err.code === 'ENOENT') return { ok: true, path: file, lines: [], truncated: false, total: 0 };
    return { ok: false, path: file, lines: [], truncated: false, total: 0, error: err.message };
  }

  const all = text.split('\n').filter((line) => line.trim().length > 0);
  const shown = all.slice(-lines);
  return {
    ok: true,
    path: file,
    total: all.length,
    truncated: all.length > shown.length,
    lines: shown.map((line) => {
      const match = /^(\S+)\s+\[([^\]]*)\]\s([\s\S]*)$/.exec(line);
      if (!match || Number.isNaN(Date.parse(match[1]))) return { at: null, command: null, line };
      return { at: new Date(match[1]).toISOString(), command: match[2] === '-' ? null : match[2], line: match[3] };
    }),
  };
}
