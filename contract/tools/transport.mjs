// Command lines and retry safety: the reference of v3.md section 15.
//
// ssh runs a string, not an argv, and its exit status mixes ssh's own failures
// with the remote command's. Before this file the three clients each turned an
// argv into a different string for the same Windows machine (one wrapped it in
// PowerShell quotes, one in cmd carets, one left a backslash bare that the
// dispatcher then refused), and each read a bare exit 255 differently: the Mac
// took it as "never sent" and tried the same mutation on the next route. Here
// is the one serialisation per grammar, the one reading of an attempt, the one
// walk over routes, the status budget and the dial grammar for hosts, users and
// ports. contract/vectors/transport.json pins every function case by case, and
// each client ports this file rather than its own idea of quoting.
//
// The code below is the normative text of the spec it came from; the comments
// say why each rule is there. It has no dependencies and reads no clock, no
// environment and no platform: a port must give the same answer anywhere.

export const FAMILIES = ['transport'];

/** The four grammars a command line is written in (section 15.1). */
export const GRAMMARS = Object.freeze(['posix', 'dispatch', 'cmd', 'powershell']);

// U+0000-U+001F and U+007F. A control character in an argument changes what a
// terminal shows a person and what some shells do with the line, and no shell
// quotes a line feed the same way as another.
const CONTROL = /[\u0000-\u001f\u007f]/;
// What a POSIX shell and the dispatcher both read as one literal word. `~` is
// deliberately missing: argv items are literal, and a bare tilde would be
// expanded by every shell but the dispatcher, so the same document would run a
// different file depending on which of the two received it. `=` may not start
// a word either, which zsh expands, and the first word may not hold one at all,
// which sh reads as a variable assignment.
const POSIX_BARE = /^[A-Za-z0-9._\/:@+,-][A-Za-z0-9._\/:@+,=-]*$/;
// PowerShell's tokenizer treats U+2018 to U+201B as single quotes too. Doubling
// only U+0027 left a typographic quote free to end the string early (T11, A12).
const PS_QUOTE = /['\u2018\u2019\u201a\u201b]/g;
const bytes = (s) => Buffer.byteLength(s, 'utf8');

/**
 * The grammar a system's command line is written in (section 15.1).
 *
 * A system restricted on the route always gets the dispatcher grammar, whatever
 * its `shell` says, because the forced command is not a shell. Otherwise the
 * configured shell wins, and with none the platform's stock OpenSSH shell
 * decides: cmd on Windows, a POSIX shell everywhere else. PowerShell is never
 * inferred and no line is ever retried in another grammar: a guess that is
 * wrong fails loudly, a retry in a second grammar can run the command twice.
 */
export function resolveGrammar({ platform, shell, restricted, restrictedSession = false, agentLength }) {
  if (restricted === true || restrictedSession === true)
    return agentLength === 2 ? { grammar: 'dispatch', refusal: null } : { grammar: null, refusal: 'restricted-agent-shape' };
  if (shell === undefined || shell === null) return { grammar: platform === 'windows' ? 'cmd' : 'posix', refusal: null };
  if (shell === 'posix' || shell === 'cmd' || shell === 'powershell') return { grammar: shell, refusal: null };
  return { grammar: null, refusal: 'unknown-shell' };
}

/**
 * The one line an argv becomes in `grammar`, or why there is none (section 15.2).
 *
 * Elements are checked in order and the first that fails refuses the whole
 * line, with its index: nothing is ever sent with an element dropped or
 * "fixed". posix refuses the backslash sequences fish reads as escapes inside
 * single quotes; dispatch instead enforces the dispatcher's own limits, so a
 * line that leaves here is always one dispatch.mjs accepts. cmd quotes every
 * element and refuses what cmd expands even inside quotes (`%`, `!`) or cannot
 * carry at all (`"`); the `@` prefix makes the line survive whether sshd wrapped
 * it in quotes or not. powershell refuses what its native-argument passing
 * mangles: an embedded `"`, a trailing backslash, two spaces in a row.
 */
export function serialize(grammar, argv) {
  if (!Array.isArray(argv) || argv.length === 0) return { line: null, refusal: 'no-command', index: null };
  if (grammar === 'dispatch' && argv.length > 64) return { line: null, refusal: 'too-many-arguments', index: null };
  const tokens = [];
  for (const [index, value] of argv.entries()) {
    const refuse = (refusal) => ({ line: null, refusal, index });
    if (typeof value !== 'string' || value.length === 0) return refuse('empty-argument');
    if (CONTROL.test(value)) return refuse('control-character');
    if (grammar === 'posix' || grammar === 'dispatch') {
      if (grammar === 'posix' && /\\(?:\\|'|$)/.test(value)) return refuse('posix-backslash');
      if (grammar === 'dispatch' && bytes(value) > 4096) return refuse('argument-too-long');
      tokens.push(POSIX_BARE.test(value) && !(index === 0 && value.includes('=')) ? value : `'${value.replaceAll("'", "'\\''")}'`);
    } else if (grammar === 'cmd') {
      if (value.includes('"')) return refuse('double-quote');
      if (value.includes('%')) return refuse('percent');
      if (value.includes('!')) return refuse('exclamation');
      // The C runtime reads a run of backslashes before a quote as escapes, so
      // the trailing run is doubled and the closing quote stays a quote.
      // Backslashes anywhere else stand for themselves.
      tokens.push(`"${value}${value.match(/\\*$/)[0]}"`);
    } else if (grammar === 'powershell') {
      if (value.includes('"')) return refuse('double-quote');
      if (value.endsWith('\\')) return refuse('trailing-backslash');
      if (value.includes('  ')) return refuse('double-space');
      tokens.push(`'${value.replace(PS_QUOTE, (q) => q + q)}'`);
    } else return { line: null, refusal: 'unknown-shell', index: null };
  }
  const body = tokens.join(' ');
  if (grammar === 'dispatch' && bytes(body) > 16384) return { line: null, refusal: 'line-too-long', index: null };
  return { line: grammar === 'cmd' ? `@${body}` : grammar === 'powershell' ? `& ${body}` : body, refusal: null, index: null };
}

// Blank means only these four, not whatever a language's trim strips.
const BLANK = /^[\t\n\r ]*$/;

/**
 * The lines of ssh's own log (`-E <file>`, `-o LogLevel=ERROR`) that prove the
 * command never left this device, in the order they are tried: the first row
 * with any matching line decides (section 15.4). Only ssh's log is read here.
 * The agent writes raw text to stderr and the launcher passes it through, so a
 * remote program can print any ssh-looking line it likes; reading remote stderr
 * as evidence is how a mutation that ran gets sent a second time.
 */
export const PRE_DISPATCH = [
  ['hostKeyUnknown', [
    /^No [^ ]+ host key is known for .+ and you have requested strict checking\.$/i,
  ]],
  ['hostKeyChanged', [
    /REMOTE HOST IDENTIFICATION HAS CHANGED/i,
    /^Host key for .+ has changed and you have requested strict checking\.$/i,
    /^Host key verification failed\.$/i,
  ]],
  ['authenticationFailed', [
    /^(?:.+: )?Permission denied \([^()]+\)\.?$/i,
    /: Too many authentication failures$/i,
    /^No more authentication methods to try\.$/i,
  ]],
  ['connectionRefused', [
    /^ssh(?:\.exe)?: connect to host .+ port [0-9]+: Connection refused$/i,
    /^(?:kex|ssh)_exchange_identification: /i,
    /^banner exchange: /i,
    /^Unable to negotiate with .+: no matching .+$/i,
    /^channel 0: open failed: /i,
    /^exec request failed on channel 0$/i,
  ]],
  ['hostUnreachable', [
    /^ssh(?:\.exe)?: connect to host .+ port [0-9]+: .+$/i,
    /^ssh(?:\.exe)?: Could not resolve hostname .+: .+$/i,
    /^Connection timed out during banner exchange$/i,
  ]],
  ['launchFailed', [
    /^Bad owner or permissions on .+$/i,
    /: terminating, [0-9]+ bad configuration options?$/i,
    /^command-line line [0-9]+: .+$/i,
    /^Can't open user config file .+$/i,
    /^Cannot execute command-line and remote command\.$/i,
  ]],
];

/** The kinds after which the rest of a route's systems are skipped (section 15.5, step 7). */
export const ROUTE_LEVEL = new Set(['noRoute', 'launchFailed', 'hostUnreachable', 'connectionRefused',
  'authenticationFailed', 'hostKeyChanged', 'hostKeyUnknown', 'timedOut', 'linkLost']);

/**
 * The reply in stdout: from its first `{` to its last `}`, when that parses as a
 * JSON object. Windows PowerShell can put a `#< CLIXML` banner in front of
 * anything a native program prints, and a login script can print a line or
 * two; the slice is what survives both.
 */
export function jsonObjectSlice(stdout) {
  const a = stdout.indexOf('{'), b = stdout.lastIndexOf('}');
  if (a < 0 || b <= a) return null;
  try { const v = JSON.parse(stdout.slice(a, b + 1)); return v && typeof v === 'object' && !Array.isArray(v) ? v : null; }
  catch { return null; }
}

/**
 * What one attempt says (section 15.4): a reply, or a failure with its kind,
 * whether the command may have been dispatched, and whether the whole route is
 * now known to be dead.
 *
 * The order is the rule. A reply wins over everything, even a timeout, because
 * it is the agent's own account of what happened. `never` is only ever claimed
 * on evidence that the command cannot have run: ssh could not start, ssh's own
 * log names a failure before the channel opened, or a shell exited 126, 127 or
 * 9009 with nothing on stdout, which only a shell that could not start the
 * interpreter does (the agent itself exits 0 or 1). Every other ending is
 * `unknown`, and a mutation that ended `unknown` is never sent again.
 */
export function classify({ launchFailed, timedOut, exitCode, stdout, stderr, sshLog }) {
  const fail = (kind, dispatch) => ({ outcome: 'failure', kind, dispatch, routeLevel: ROUTE_LEVEL.has(kind) });
  if (launchFailed) return fail('launchFailed', 'never');
  if (jsonObjectSlice(stdout)) return { outcome: 'reply', kind: null, dispatch: 'acknowledged', routeLevel: false };
  if (timedOut) return fail('timedOut', 'unknown');
  if (exitCode === null) return fail('linkLost', 'unknown');
  const blank = BLANK.test(stdout);
  if (exitCode === 255) {
    if (blank) {
      const lines = sshLog.split('\n').map((l) => l.replace(/\r$/, ''));
      for (const [kind, patterns] of PRE_DISPATCH)
        if (lines.some((l) => patterns.some((p) => p.test(l)))) return fail(kind, 'never');
    }
    // An exit 255 nothing explains: the remote side may have run the command
    // and the link dropped afterwards (M15).
    return fail('linkLost', 'unknown');
  }
  if (blank && [126, 127, 9009].includes(exitCode)) return fail('interpreterMissing', 'never');
  // From here on stderr is read, but only to name the failure for a person. It
  // never makes an attempt `never`: a program that printed "Cannot find module"
  // may have done half of its work first (A4).
  const err = stderr.toLowerCase();
  if (err.includes('cannot find module') || err.includes('err_module_not_found')) return fail('agentMissing', 'unknown');
  if (['is not recognized as', 'commandnotfoundexception', 'cannot find the path specified'].some((m) => err.includes(m)))
    return fail('interpreterMissing', 'unknown');
  return fail('unreadableOutput', 'unknown');
}

/**
 * The `--budget-ms` a status attempt sends, or that no attempt should be made
 * (section 15.5, step 3). Five seconds stay in hand for ssh and the reply, the
 * agent never gets more than 20 s, and it never gets less than 1 s: a first
 * attempt always goes out, a later one only when there is time to use its
 * answer. Before this, every attempt sent 20000 whatever was left, so after two
 * dead routes the working one was cut off by the client's own deadline (C9).
 */
export function statusBudget(remainingMs, attemptsMade) {
  const budget = Math.min(20000, Math.floor(remainingMs) - 5000);
  if (budget >= 1000) return { attempt: true, budgetMs: budget };
  return attemptsMade === 0 ? { attempt: true, budgetMs: 1000 } : { attempt: false, budgetMs: null };
}

/**
 * Which attempts are made, given what each would end in, and why the walk
 * stopped (section 15.5). `attempts` is `[{ route, system, result }]` in the
 * order of section 10.3, each `result` what classify() said or would say.
 *
 * A read goes on after any failure. A mutation goes on only after a failure
 * that is `never`; anything else may have run, so it stops at the first such
 * attempt and is reconciled with `op <id>`. A route-level failure skips the
 * rest of that route's systems, because they share its connection.
 */
export function walk({ safety, attempts }) {
  const sent = [], dead = new Set();
  for (const [i, { route, result }] of attempts.entries()) {
    if (dead.has(route)) continue;
    sent.push(i);
    if (result.outcome === 'reply') return { sent, ending: 'reply' };
    if (safety === 'mutation' && result.dispatch !== 'never') return { sent, ending: 'stopped' };
    if (result.routeLevel) dead.add(route);
  }
  return { sent, ending: 'exhausted' };
}

// ---------------------------------------------------------------------------
// The dial grammar of section 15.6. The setup document's guard patterns in
// common.schema.json refuse only what ssh, ssh-keyscan or known_hosts would read
// as syntax; these are stricter, and a value that passes the guard and fails
// here is accepted in the document and never dialled, scanned or pinned.
// ---------------------------------------------------------------------------

const LABEL = '[A-Za-z0-9_](?:[A-Za-z0-9_-]{0,61}[A-Za-z0-9_])?';
const NAME = new RegExp(`^${LABEL}(?:\\.${LABEL})*$`);
const isIPv4 = (v) => { const p = v.split('.'); return p.length === 4 && p.every((x) => /^(?:0|[1-9][0-9]{0,2})$/.test(x) && +x <= 255); };
function isIPv6(v) {
  if (!/^[0-9A-Fa-f:.]+$/.test(v)) return false;
  const halves = v.split('::'); if (halves.length > 2) return false;
  const g = (s) => (s === '' ? [] : s.split(':'));
  const head = g(halves[0]), tail = halves.length === 2 ? g(halves[1]) : [], all = [...head, ...tail];
  let count = 0;
  for (const [i, x] of all.entries()) {
    if (x.includes('.')) {
      // An embedded IPv4 address (::ffff:192.0.2.1) is the last group or nothing.
      const last = i === all.length - 1 && (halves.length === 1 || tail.length > 0);
      if (!last || !isIPv4(x)) return false; count += 2;
    } else if (/^[0-9A-Fa-f]{1,4}$/.test(x)) count += 1; else return false;
  }
  return halves.length === 2 ? count <= 7 : count === 8;
}

/**
 * `dns`, `ipv4` or `ipv6` for a host a client may dial, or null. A name whose
 * last label is all digits must be a dotted quad: `1234` and `1.2.3` are what a
 * resolver turns into addresses nobody wrote. No brackets, no zone, no trailing
 * dot, no port.
 */
export function hostForm(v) {
  if (typeof v !== 'string' || v.length < 1 || v.length > 253) return null;
  if (v.includes(':')) return isIPv6(v) ? 'ipv6' : null;
  if (!NAME.test(v)) return null;
  return /^[0-9]+$/.test(v.split('.').at(-1)) ? (isIPv4(v) ? 'ipv4' : null) : 'dns';
}

/** A login name: local or domain account, 1 to 64 characters, nothing a shell or ssh reads as syntax. */
export const isUser = (v) => typeof v === 'string' && v.length >= 1 && v.length <= 64
  && /^[A-Za-z0-9_][A-Za-z0-9._@\\ -]*$/.test(v) && /[A-Za-z0-9._-]$/.test(v) && !/ [ -]/.test(v);

/** A TCP port: an integer from 1 to 65535, never a string or a fraction. */
export const isPort = (v) => Number.isInteger(v) && v >= 1 && v <= 65535;

// ---------------------------------------------------------------------------
// The reference interface of the `transport` vector family.
// ---------------------------------------------------------------------------

export function run(group, input) {
  switch (group) {
    case 'grammars': {
      const { grammar, refusal } = resolveGrammar(input);
      return { grammar, refusal };
    }
    case 'commandLines': return serialize(input.grammar, input.argv);
    case 'classifications': return classify(input);
    case 'walks': return walk(input);
    case 'statusBudgets': return statusBudget(input.remainingMs, input.attemptsMade);
    case 'hosts': {
      const form = hostForm(input.value);
      return { valid: form !== null, form };
    }
    case 'users': return { valid: isUser(input.value) };
    case 'ports': return { valid: isPort(input.value) };
    default: throw new Error(`the transport family has no group "${group}"`);
  }
}
