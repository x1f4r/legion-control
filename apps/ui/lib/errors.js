// Centralized plain-language error formatting and terminal-hint filtering.
// Every UI surface that renders errors routes through this module so CLI commands, backticks,
// config file paths, and dotted tool names only appear when Technical details is on.

const TOOL_NAME_RE =
  /\b(?:agent|audit|config|device|env|exec|ext|fav|fleet|fs|job|meta|net|power|proj|svc|sync|sys|token|update)\.[a-z_]+\b/;

const TERMINAL_HINT_PATTERNS = [
  /\blegion(?:\s+help|\s+tools|\s+serve|\s+pair|\s+token|\s+fleet|\s+device|\s+rpc|\s+stream)?\b/i,
  /`[^`]+`/,
  /\b(?:policy|fleet|node|devices|tokens|projects|favorites|sync)\.toml\b/i,
  /\btools\.d\b/i,
  /\b(?:GET|POST|PUT|DELETE)\s+\/v\d+\//,
  /(?:^|[\s(])(?:~\/|\/(?:etc|home|var|usr|opt)\/|[A-Za-z]:\\)/,
  /\b(?:force|dry_run|on)\s*[:=]\s*(?:true|false|["'<])/i,
  /\s--[a-z][a-z0-9-]*/,
  TOOL_NAME_RE,
];

const TECHNICAL_MESSAGE_PATTERNS = [
  TOOL_NAME_RE,
  /`/,
  /\blegion\s+help\b/i,
  /\b(?:policy|fleet|node|devices|tokens|projects|favorites|sync)\.toml\b/i,
  /\b(?:unknown field|missing field|expected one of|invalid type)\b/i,
  /\bPrincipal\s+"[^"]+"\s+\(role\s+"[^"]+"\)/,
];

/**
 * Returns true when a hint string reads like a terminal command, backticked syntax,
 * tool reference, HTTP endpoint, or config file path to edit.
 */
export function isTerminalHint(text) {
  if (typeof text !== 'string') return false;
  const trimmed = text.trim();
  if (!trimmed) return false;
  return TERMINAL_HINT_PATTERNS.some((re) => re.test(trimmed));
}

/**
 * Returns true when an error message contains raw technical details (dotted tool names,
 * backticks, serde field errors, config file names, or principal/role syntax).
 */
export function containsTechnicalText(text) {
  if (typeof text !== 'string') return false;
  const trimmed = text.trim();
  if (!trimmed) return false;
  return TECHNICAL_MESSAGE_PATTERNS.some((re) => re.test(trimmed));
}

/**
 * Plain-language sentence for an error's stable `code`, used whenever Technical details is off
 * and the raw message would expose technical details (or for codes that always need plain wording).
 */
export function plainMessageForCode(code, { machine = null, deviceNoun = 'device' } = {}) {
  const target = machine || 'This machine';
  switch (String(code || '').toLowerCase()) {
    case 'needs_newer_legion':
      return `${target} needs a newer Legion for this.`;
    case 'invalid_args':
      return 'This machine did not understand the request. It may be running an older Legion.';
    case 'unknown_tool':
      return `${target} needs a newer Legion for this.`;
    case 'unsupported':
      return `${target} does not support this yet. It may be running an older Legion.`;
    case 'denied':
      return `This ${deviceNoun} is not allowed to do that here.`;
    case 'not_found':
      return 'That could not be found.';
    case 'exists':
      return 'Something with that name already exists.';
    case 'conflict':
      return 'That changed while you were working. Try again.';
    case 'unreachable':
      return machine ? `${machine} did not answer.` : 'Could not reach this machine.';
    case 'timeout':
      return machine ? `${machine} took too long to answer.` : 'That took too long to answer.';
    case 'cancelled':
      return 'That was stopped.';
    case 'busy':
      return machine ? `${machine} is busy right now.` : 'This machine is busy right now.';
    case 'failed':
    case 'internal':
    default:
      return 'Something went wrong on this machine.';
  }
}

/**
 * Formats an error (or explicit title/message/hint) for display according to Technical details mode.
 * Returns `{ heading, detail, machinePrefix, canRepair }`.
 */
export function formatErrorForDisplay({
  error = null,
  title = null,
  message = null,
  hint = undefined,
  technical = false,
  deviceNoun = 'device',
} = {}) {
  const reason = error?.details?.reason;
  const canRepair =
    error?.code === 'denied' &&
    (reason === 'device_removed_or_unknown' ||
      reason === 'auth_failed' ||
      reason === 'host_key_mismatch');

  const isRemovedPlain =
    !technical && !title && error?.code === 'denied' && reason === 'device_removed_or_unknown';

  if (isRemovedPlain) {
    return {
      heading: `${error.machine || 'This machine'} no longer lets this ${deviceNoun} in. It may have been removed there.`,
      detail: null,
      machinePrefix: '',
      canRepair,
    };
  }

  const rawHeading = title || (error ? error.message : message) || '';
  const rawDetail = hint !== undefined ? hint : (error ? error.hint : null);
  const rawMachine = error?.machine ? String(error.machine) : '';

  if (technical) {
    return {
      heading: rawHeading,
      detail: rawDetail || null,
      machinePrefix: rawMachine && !title ? `${rawMachine}: ` : '',
      canRepair,
    };
  }

  // Plain mode: never show raw tool names, backticks, config file paths, or CLI hints.
  const code = String(error?.code || '').toLowerCase();
  const forcePlainByCode =
    !title &&
    error &&
    (code === 'needs_newer_legion' || code === 'unknown_tool' || code === 'invalid_args');

  let heading = rawHeading;
  let machinePrefix = '';
  let suppressHint = false;

  if (forcePlainByCode) {
    heading = plainMessageForCode(code, { machine: rawMachine || null, deviceNoun });
    suppressHint = true;
  } else if (containsTechnicalText(rawHeading)) {
    heading = error?.code
      ? plainMessageForCode(code, { machine: rawMachine || null, deviceNoun })
      : 'Something went wrong on this machine.';
    suppressHint = true;
  } else if (rawMachine && !title && heading && !heading.toLowerCase().includes(rawMachine.toLowerCase())) {
    machinePrefix = `${rawMachine}: `;
  }

  let detail = suppressHint ? null : rawDetail || null;
  if (detail && (isTerminalHint(detail) || containsTechnicalText(detail))) {
    detail = null;
  }

  return {
    heading,
    detail,
    machinePrefix,
    canRepair,
  };
}

/**
 * Returns a single plain-language sentence for toast banners and inline error slots.
 */
export function plainErrorMessage(error, fallback = 'Something went wrong.', { technical = false, deviceNoun = 'device' } = {}) {
  if (!error) return fallback;
  if (typeof error === 'string') {
    if (!technical && containsTechnicalText(error)) return fallback;
    return error || fallback;
  }
  const formatted = formatErrorForDisplay({ error, technical, deviceNoun });
  if (!formatted.heading) return fallback;
  return `${formatted.machinePrefix}${formatted.heading}`;
}
