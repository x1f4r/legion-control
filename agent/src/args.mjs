// How an argv becomes a command's flags, options and positional arguments.
//
// One parser, used by the CLI and by the restricted dispatcher alike. The
// dispatcher used to judge the shape of a restricted command by matching
// strings in the raw tokens, and the CLI then parsed those tokens its own way:
// `self-update --check --stdin --install=1` had no token equal to `--install`,
// so the dispatcher let it through, and only the CLI refused it later (T10).
// A check on what a command means has to read it the way the command does.

/** Options that take a value: `--name VALUE` or `--name=VALUE`. Everything else is a flag. */
export const VALUE_OPTIONS = new Set([
  'service',
  'op',
  'expires',
  'budget-ms',
  'wait',
  'limit',
  'kind',
  'lines',
  'controller-id',
  'revision',
  'from',
]);

/** Options whose value is a path and therefore not held to the token grammar. */
export const PATH_OPTIONS = new Set(['from']);

/**
 * `{ flags, options, positional }` for an argv after its command. A value
 * written with `=` is an option whatever its name, so `--install=1` is the
 * option `install`, and a flag's name is never mistaken for a value.
 */
export function parseArgs(argv) {
  const flags = new Set();
  const options = new Map();
  const positional = [];
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '-h' || arg === '--help') {
      flags.add('help');
      continue;
    }
    if (!arg.startsWith('--')) {
      positional.push(arg);
      continue;
    }
    const body = arg.slice(2);
    const equals = body.indexOf('=');
    if (equals !== -1) {
      options.set(body.slice(0, equals), body.slice(equals + 1));
      continue;
    }
    if (VALUE_OPTIONS.has(body)) {
      // The value is the next argument, consumed here so it cannot be mistaken
      // for the command's own positional argument.
      options.set(body, argv[index + 1] ?? '');
      index += 1;
      continue;
    }
    flags.add(body);
  }
  return { flags, options, positional };
}
