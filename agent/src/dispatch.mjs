// The restricted ssh dispatcher.
//
// A key in authorized_keys normally gets a shell, which is far more authority
// than a phone needs to press "restart". With
//
//   command="…/agent/install/dispatch.sh",restrict,no-pty ssh-ed25519 AAAA… phone
//
// the key gets this instead: sshd puts the requested command in
// SSH_ORIGINAL_COMMAND, runs the wrapper, and the wrapper execs the agent with
// `dispatch`. What arrives here is a STRING THE CLIENT CHOSE, so it is treated
// as such.
//
// THERE IS NO SHELL ON THIS PATH. Not a restricted one, not a careful one, none.
// The string is tokenised by the parser below and the resulting argv is run in
// this process. Nothing expands a variable, a glob, a tilde or a backtick,
// because nothing here is capable of it.
//
// The grammar every client serialises against:
//
//   tokens separated by spaces or tabs
//   BARE     ^[A-Za-z0-9][A-Za-z0-9._:@/\\~=+-]*$   (contract Part 1 rule 4)
//   QUOTED   'single quoted'; a literal quote is written '\'' — close, escaped
//            quote, reopen, which is the POSIX idiom, so the same string also
//            works verbatim on an unrestricted account
//   refused  $ ` ; & | < > ( ) { } * ? ! # newline CR NUL outside quotes,
//            a double quote outside quotes, an unterminated quote, and any
//            backslash outside quotes except the \' of the idiom above
//
// The first two tokens are the interpreter and the agent entry point, and they
// are NOT held to the bare grammar: a real interpreter path starts with "/" or a
// drive letter, which the grammar excludes because it exists for values a client
// interpolates. They are checked by something stronger instead — equality with
// this process's own executable and with this agent's own resolved entry point.
//
// Inside a quoted token every byte is literal, including double quotes and
// spaces, because a Windows path really can contain both.

import path from 'node:path';
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';
import { REASON, TOKEN_PATTERN } from './contract.mjs';

export const MAX_COMMAND_BYTES = 16 * 1024;
export const MAX_TOKENS = 64;
export const MAX_TOKEN_BYTES = 4096;

/** Characters that never appear outside a quoted token. */
const FORBIDDEN = new Set(['$', '`', ';', '&', '|', '<', '>', '(', ')', '{', '}', '*', '?', '!', '#', '"', '\\', '\n', '\r', '\0']);

/**
 * Split a command line into tokens, or say why it cannot be.
 *
 * Returns `{ ok, tokens }` or `{ ok: false, error, argument }`.
 */
export function parseCommandLine(line) {
  const bad = (error, argument = null) => ({ ok: false, tokens: null, error, argument });

  if (typeof line !== 'string' || line.length === 0) return bad('no command was sent');
  if (Buffer.byteLength(line, 'utf8') > MAX_COMMAND_BYTES) return bad(`the command line is longer than ${MAX_COMMAND_BYTES} bytes`);

  const tokens = [];
  let index = 0;
  while (index < line.length) {
    const character = line[index];
    if (character === ' ' || character === '\t') {
      index += 1;
      continue;
    }

    let token = '';
    let quoted = false;
    while (index < line.length && line[index] !== ' ' && line[index] !== '\t') {
      if (line[index] === "'") {
        quoted = true;
        index += 1;
        const close = line.indexOf("'", index);
        if (close === -1) return bad('a quoted argument was never closed');
        token += line.slice(index, close);
        index = close + 1;
        continue;
      }
      // The one escape there is: \' outside quotes, which is how POSIX writes a
      // literal quote inside an otherwise quoted word ('it'\''s'). Supporting
      // exactly this and nothing else keeps the string a client sends identical
      // to the one a shell would have accepted, without introducing escaping.
      if (line[index] === '\\' && line[index + 1] === "'") {
        token += "'";
        quoted = true;
        index += 2;
        continue;
      }
      if (FORBIDDEN.has(line[index])) {
        return bad(
          `the character ${JSON.stringify(line[index])} is not allowed outside a quoted argument`,
          line.slice(0, 80),
        );
      }
      token += line[index];
      index += 1;
    }

    if (Buffer.byteLength(token, 'utf8') > MAX_TOKEN_BYTES) return bad(`an argument is longer than ${MAX_TOKEN_BYTES} bytes`, token.slice(0, 40));
    if (token.length > 0 || quoted) tokens.push(token);
    if (tokens.length > MAX_TOKENS) return bad(`more than ${MAX_TOKENS} arguments`);
  }

  if (tokens.length === 0) return bad('no command was sent');
  return { ok: true, tokens, error: null };
}

/** This agent's own entry point, resolved through symlinks. */
export function ownEntry() {
  const entry = fileURLToPath(new URL('./index.mjs', import.meta.url));
  try {
    return fs.realpathSync(entry);
  } catch {
    return entry;
  }
}

/** The stable launcher is an alias for routing, never a requested executable. */
function stableEntryAlias(entry) {
  try {
    const resolvedEntry = fs.realpathSync(entry);
    const agent = path.dirname(path.dirname(resolvedEntry));
    if (path.basename(agent) !== 'agent' || path.basename(path.dirname(resolvedEntry)) !== 'src' || path.basename(resolvedEntry) !== 'index.mjs') return null;
    const base = path.dirname(agent);
    const launcher = path.join(base, 'bin', 'launcher.mjs');
    if (!fs.lstatSync(launcher).isFile()) return null;
    const resolved = fs.realpathSync(launcher);
    return path.dirname(resolved) === path.join(base, 'bin') ? resolved : null;
  } catch { return null; }
}

/**
 * Commands a restricted key may run.
 *
 * `self-update --stdin` is here because it is the ONLY way a restricted session
 * can hand over a bundle: there is no shell, so `cat > file` does not exist. The
 * bytes are verified against the pinned key before anything is extracted, so
 * allowing the upload does not allow running what was uploaded.
 *
 * `self-update --from PATH` and `--install` are refused: both would run a tree
 * whose provenance is a path this same session chose.
 */
export const RESTRICTED_COMMANDS = new Set([
  'status',
  'busy',
  'update',
  'restart',
  'boot',
  'sleep',
  'run',
  'cycle',
  'op',
  'cancel',
  'history',
  'logs',
  'doctor',
  'bundle',
  'config',
  'policy',
  'auto-update',
  'self-update',
  'version',
  'help',
]);

/** What a restricted session may never do, whatever the flags say. */
function refuseRestrictedShape(command, rest) {
  if (command === 'dispatch') return 'dispatch cannot dispatch itself';
  if (command === 'self-update') {
    if (rest.includes('--install')) return 'self-update --install runs a tree this session provided, so it is not available to a restricted key';
    if (rest.some((token) => token === '--from' || token.startsWith('--from='))) {
      return 'self-update --from runs a tree from a path this session chose; a restricted key uploads with --stdin instead';
    }
    if (!rest.includes('--stdin') && !rest.includes('--check') && !rest.includes('--rollback')) {
      return 'self-update from a restricted key needs --stdin, --check or --rollback';
    }
  }
  return null;
}

/**
 * Validate SSH_ORIGINAL_COMMAND and return the argv the agent should run.
 *
 * Returns `{ ok, argv }` or a refusal carrying the reasonCode and the list of
 * commands this key does have, which is what a person debugging an
 * authorized_keys line actually needs to see.
 */
export function authorize(commandLine, { entry = ownEntry() } = {}) {
  const refuse = (message, argument = null, reasonCode = REASON.restricted) => ({
    ok: false,
    reasonCode,
    message,
    argument,
    accepts: [...RESTRICTED_COMMANDS],
  });

  const parsed = parseCommandLine(commandLine);
  if (!parsed.ok) return refuse(parsed.error, parsed.argument, REASON.badArgument);

  const tokens = parsed.tokens;
  // Everything from the command onwards is a value a client chose, so it is held
  // to the wire grammar here as well as by the CLI. The first two tokens are
  // checked by identity below instead.
  for (const token of tokens.slice(2)) {
    if (token.startsWith('--')) {
      const value = token.slice(2).split('=')[1];
      if (value !== undefined && value.length > 0 && !TOKEN_PATTERN.test(value)) {
        return refuse(`${JSON.stringify(value)} is not a valid option value`, token, REASON.badArgument);
      }
      continue;
    }
    if (!TOKEN_PATTERN.test(token)) {
      return refuse(`${JSON.stringify(token)} is not a valid argument`, token, REASON.badArgument);
    }
  }

  // <interpreter> <agent entry> <command> [args...]
  if (tokens.length < 3) {
    return refuse(
      'a restricted session runs "<node> <agent>/src/index.mjs <command>"; that is not what arrived',
      tokens.join(' ').slice(0, 120),
    );
  }

  // The interpreter is compared through realpath, because the path a client
  // configured is very often a symlink to the one this process reports:
  // /usr/local/bin/node, /opt/homebrew/bin/node and a version manager's shim all
  // resolve to the same binary, and refusing them would make the dispatcher
  // unusable for exactly the setups people have.
  const interpreter = tokens[0];
  const resolve = (value) => {
    try {
      return fs.realpathSync(value);
    } catch {
      return path.resolve(value);
    }
  };
  const own = resolve(process.execPath);
  const acceptable = new Set([own, process.execPath, 'node', path.basename(process.execPath)]);
  if (!acceptable.has(interpreter) && resolve(interpreter) !== own) {
    return refuse(
      `${JSON.stringify(interpreter)} is not the interpreter this agent runs under (${own})`,
      interpreter,
    );
  }

  // Both configured entry forms route to THIS already-running agent. The
  // prefix is validated and discarded; it is never imported or executed.
  let requested;
  try {
    requested = fs.realpathSync(tokens[1]);
  } catch {
    requested = path.resolve(tokens[1]);
  }
  if (requested !== entry && requested !== stableEntryAlias(entry)) {
    return refuse(`this key may only run ${entry}, and ${JSON.stringify(tokens[1])} is not it`, tokens[1]);
  }

  const command = tokens[2];
  if (!RESTRICTED_COMMANDS.has(command)) {
    return refuse(
      `this key may run ${[...RESTRICTED_COMMANDS].join(', ')} on this agent, and nothing else; ${JSON.stringify(command)} was refused`,
      command,
    );
  }

  const rest = tokens.slice(3);
  const shapeProblem = refuseRestrictedShape(command, rest);
  if (shapeProblem) return refuse(shapeProblem, command);

  return { ok: true, argv: tokens.slice(2), command, reasonCode: null, message: null };
}
