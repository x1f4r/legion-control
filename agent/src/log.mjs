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
