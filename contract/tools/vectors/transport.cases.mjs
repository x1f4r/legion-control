// Command lines and retry safety, v3.md section 15, as cases.
//
// Each group pins one function of tools/transport.mjs. The command lines are
// the argvs that broke a client somewhere: a Windows path the dispatcher
// refused because Android left its backslashes bare (C4), an interpreter under
// "Program Files (x86)" that cmd split at the parenthesis once the desktop had
// put carets inside its quotes (D10), a typographic quote that ended a
// PowerShell string early (T11, A12). The classifications are what ssh, a
// shell and the agent really print, including the exit 255 nobody can explain,
// which the Mac read as "never sent" and answered by restarting a service a
// second time over another route (M15). The walks put those readings in order
// across routes and systems, and the budgets are the arithmetic that let a
// status survive two dead routes (C9). The hosts, users and ports are the dial
// grammar, with the strings that try to be ssh options or known_hosts patterns
// tagged injection (T5).

import { classify } from '../transport.mjs';

const EVERYONE = ['agent', 'mac', 'desktop', 'android'];
const CLIENTS = ['mac', 'desktop', 'android'];
const DESKTOPS = ['mac', 'desktop'];

// --- grammars ----------------------------------------------------------------

const grammar = (name, input, want, purpose, tags = []) => ({
  name, purpose, tags,
  input: { platform: null, shell: null, restricted: null, restrictedSession: false, agentLength: 2, ...input },
  want,
});

const grammars = [
  grammar('linux-posix', { platform: 'linux', shell: 'posix', restricted: false }, { grammar: 'posix', refusal: null },
    'A Linux system with its shell written out: that value is the grammar.'),
  grammar('linux-shell-absent', { platform: 'linux' }, { grammar: 'posix', refusal: null },
    'No shell on a Linux system: a POSIX shell is what its OpenSSH runs.'),
  grammar('platform-and-shell-absent', {}, { grammar: 'posix', refusal: null },
    'Neither platform nor shell: anything that is not Windows defaults to POSIX.'),
  grammar('mac-shell-absent', { platform: 'mac' }, { grammar: 'posix', refusal: null },
    'A Mac with no shell is a POSIX login like Linux.'),
  grammar('windows-shell-absent', { platform: 'windows' }, { grammar: 'cmd', refusal: null },
    'No shell on Windows: cmd, because that is what a stock Windows OpenSSH runs. The Mac guessed PowerShell here and the desktop cmd, so one setup was quoted two ways (C4).', ['finding:c4']),
  grammar('windows-restricted-false', { platform: 'windows', restricted: false }, { grammar: 'cmd', refusal: null },
    'restricted false changes nothing: a Windows system without a shell is still cmd.'),
  grammar('windows-cmd', { platform: 'windows', shell: 'cmd' }, { grammar: 'cmd', refusal: null },
    'cmd written out on Windows.'),
  grammar('windows-powershell', { platform: 'windows', shell: 'powershell' }, { grammar: 'powershell', refusal: null },
    'An account whose OpenSSH runs PowerShell says so; it is never inferred.'),
  grammar('linux-cmd-written', { platform: 'linux', shell: 'cmd' }, { grammar: 'cmd', refusal: null },
    'A shell that is written out wins over the platform, even an odd one: the document says how the line is read.'),
  grammar('restricted-in-document', { platform: 'windows', shell: 'cmd', restricted: true }, { grammar: 'dispatch', refusal: null },
    'A system restricted in the document uses the dispatcher grammar whatever its shell: the forced command is not a shell, on any platform (C4).', ['finding:c4']),
  grammar('restricted-session-reported', { platform: 'windows', shell: 'cmd', restrictedSession: true }, { grammar: 'dispatch', refusal: null },
    'The last authenticated status over this route reported a restricted session: dispatch, whatever the document says.'),
  grammar('restricted-session-three-elements', { platform: 'windows', shell: 'cmd', restrictedSession: true, agentLength: 3 },
    { grammar: null, refusal: 'restricted-agent-shape' },
    'The dispatcher runs exactly an interpreter and an entry. A third element would be an agent argument smuggled in front of every command, so the system is refused.'),
  grammar('restricted-one-element', { platform: 'linux', restricted: true, agentLength: 1 }, { grammar: null, refusal: 'restricted-agent-shape' },
    'One element is not interpreter and entry either: refused like three.'),
  grammar('restricted-wins-over-unknown-shell', { platform: 'linux', shell: 'pwsh', restricted: true }, { grammar: 'dispatch', refusal: null },
    'Restriction is decided before the shell is read, so a shell value nobody knows does not matter on a restricted system.'),
  grammar('pwsh-unknown', { platform: 'windows', shell: 'pwsh' }, { grammar: null, refusal: 'unknown-shell' },
    'pwsh is not a grammar name. Aliases are refused, never mapped to the nearest grammar.'),
  grammar('shell-case-sensitive', { platform: 'linux', shell: 'POSIX' }, { grammar: null, refusal: 'unknown-shell' },
    'Grammar names are compared exactly; POSIX in capitals is not posix.'),
  grammar('restricted-null', { platform: 'linux', shell: 'posix', restricted: null }, { grammar: 'posix', refusal: null },
    'Only the JSON value true restricts a system; null does not.'),
];

// --- commandLines --------------------------------------------------------------

/**
 * A command-line case. The agent runs the dispatch cases that produce a line,
 * through its own parser, to prove the dispatcher accepts every line a client
 * may send.
 */
const line = (name, grammarName, argv, want, purpose, tags = []) => ({
  name, purpose, tags,
  consumers: grammarName === 'dispatch' && typeof want.line === 'string' ? EVERYONE : CLIENTS,
  input: { grammar: grammarName, argv },
  want,
});
const refused = (refusal, index) => ({ line: null, refusal, index });
const ok = (text) => ({ line: text, refusal: null, index: null });

const LAUNCHER = '/home/me/.legion-control/bin/launcher.mjs';
const WIN_NODE = 'C:\\Program Files\\nodejs\\node.exe';
const WIN_NODE_X86 = 'C:\\Program Files (x86)\\nodejs\\node.exe';
const WIN_LAUNCHER = 'C:\\Users\\me\\.legion-control\\bin\\launcher.mjs';
const ENCODED = 'ZQB4AGkAdAAgADAAIAA/AD4AIAB+AA==';

// The dispatcher's limits, at and one past each edge.
const many = (count) => ['node', LAUNCHER, ...Array.from({ length: count - 2 }, (_, i) => `a${i}`)];
const fill = (bytes) => 'a'.repeat(bytes);

const commandLines = [
  line('posix-launcher-status', 'posix', ['/usr/bin/node', LAUNCHER, 'status', '--budget-ms', '20000'],
    ok('/usr/bin/node /home/me/.legion-control/bin/launcher.mjs status --budget-ms 20000'),
    'The everyday line: every element is in the bare set, so nothing is quoted.'),
  line('posix-space-in-path', 'posix', ['/usr/bin/node', '/home/me/legion control/agent/src/index.mjs', 'update', '--service', 't3'],
    ok("/usr/bin/node '/home/me/legion control/agent/src/index.mjs' update --service t3"),
    'A path with a space is single-quoted as one element; the bare elements around it stay bare.'),
  line('posix-sh-c-exec', 'posix', ['/bin/sh', '-c', 'exec "$HOME/.legion-control/bin/legionctl" "$@"', 'legionctl', 'config'],
    ok("/bin/sh -c 'exec \"$HOME/.legion-control/bin/legionctl\" \"$@\"' legionctl config"),
    'The enrolment fetch command: the script for sh -c is one element, quoted so the remote shell passes it to sh untouched and sh expands $HOME itself.'),
  line('posix-single-quote', 'posix', ['node', "/home/me/it's/index.mjs"],
    ok("node '/home/me/it'\\''s/index.mjs'"),
    "A single quote inside an element closes the quoted run, adds an escaped quote and reopens it: '\\''."),
  line('posix-tilde-never-bare', 'posix', ['node', '~/.legion-control/agent/src/index.mjs'],
    ok("node '~/.legion-control/agent/src/index.mjs'"),
    'A tilde is never bare. argv elements are literal, and a shell would expand ~ where the dispatcher does not, so the same document would name two different files.'),
  line('posix-assignment-first', 'posix', ['FOO=bar', 'status'],
    ok("'FOO=bar' status"),
    'An = in the first word makes sh read it as a variable assignment and run the next word instead, so the first element is quoted.'),
  line('posix-leading-equals', 'posix', ['node', '=x'],
    ok("node '=x'"),
    'A word starting with = is expanded by zsh to the path of a command, so it is quoted.'),
  line('posix-option-with-value', 'posix', ['node', '--from=/tmp/x'],
    ok('node --from=/tmp/x'),
    'An = after the first character of a later element is plain text to every supported shell and stays bare.'),
  line('posix-backslash-letter', 'posix', ['node', 'a\\b'],
    ok("node 'a\\b'"),
    'A backslash before a letter is literal inside single quotes in every supported shell, fish included.'),
  line('posix-double-backslash', 'posix', ['node', 'a\\\\b'],
    refused('posix-backslash', 1),
    'Two backslashes are one to fish inside single quotes, so the element cannot be written for every POSIX shell the same way.'),
  line('posix-trailing-backslash', 'posix', ['node', 'a\\'],
    refused('posix-backslash', 1),
    'A trailing backslash would escape the closing quote in fish.'),
  line('posix-backslash-quote', 'posix', ['node', "a\\'b"],
    refused('posix-backslash', 1),
    "A backslash before a quote is an escaped quote to fish inside single quotes."),
  line('posix-empty-element', 'posix', ['node', ''],
    refused('empty-argument', 1),
    'An empty element vanishes or becomes an empty argument depending on the shell; it is refused in every grammar.'),
  line('posix-line-feed', 'posix', ['node', 'a\nb'],
    refused('control-character', 1),
    'A line feed ends the command in a shell and splits the dispatcher line; no grammar carries a control character.'),
  line('posix-delete-character', 'posix', ['node', 'a\u007fb'],
    refused('control-character', 1),
    'U+007F is a control character too.'),
  line('posix-non-ascii', 'posix', ['node', 'naïve'],
    ok("node 'naïve'"),
    'Anything outside the bare set is quoted, non-ASCII letters included; the bytes inside the quotes are passed as they are.'),
  line('posix-metacharacters', 'posix', ['node', '$(id);echo `id`|cat&>x<y*?{a,b}#!%^'],
    ok("node '$(id);echo `id`|cat&>x<y*?{a,b}#!%^'"),
    'Every character a POSIX shell treats as syntax is literal inside single quotes.'),
  line('posix-no-command', 'posix', [],
    refused('no-command', null),
    'An empty argv has nothing to run. A system that is not driven never reaches the serialiser; this is the second guard.'),
  line('dispatch-windows-paths', 'dispatch', [WIN_NODE, WIN_LAUNCHER, 'status'],
    ok("'C:\\Program Files\\nodejs\\node.exe' 'C:\\Users\\me\\.legion-control\\bin\\launcher.mjs' status"),
    'A restricted Windows system: its backslashes are quoted, because the dispatcher refuses a bare backslash. Android left them bare and could not drive one (C4).', ['finding:c4']),
  line('dispatch-restricted-run', 'dispatch', ['node', LAUNCHER, 'run', 'wake-tower'],
    ok('node /home/me/.legion-control/bin/launcher.mjs run wake-tower'),
    'A helper running a wake action through a restricted key: interpreter, launcher, command, all bare.'),
  line('dispatch-unc-path', 'dispatch', ['node', '\\\\server\\share\\x.mjs'],
    ok("node '\\\\server\\share\\x.mjs'"),
    'A UNC path is quoted and accepted: the dispatcher takes everything inside quotes literally, so the fish backslash rule does not apply.'),
  line('dispatch-trailing-backslash', 'dispatch', ['node', 'a\\'],
    ok("node 'a\\'"),
    'A trailing backslash is fine for the dispatcher, which has no escapes inside quotes.'),
  line('dispatch-single-quote', 'dispatch', ['node', "it's"],
    ok("node 'it'\\''s'"),
    "The dispatcher's one escape: \\' outside quotes is a literal quote, so the POSIX spelling works unchanged."),
  line('dispatch-empty-element', 'dispatch', ['node', LAUNCHER, ''],
    refused('empty-argument', 2),
    'An empty element is refused in the dispatcher grammar as well.'),
  line('dispatch-64-elements', 'dispatch', many(64),
    ok(many(64).join(' ')),
    'Exactly 64 elements: the most the dispatcher accepts.'),
  line('dispatch-65-elements', 'dispatch', many(65),
    refused('too-many-arguments', null),
    'One element over the dispatcher limit: refused before anything is written.'),
  line('dispatch-4096-byte-element', 'dispatch', ['node', fill(4096)],
    ok(`node ${fill(4096)}`),
    'An element of exactly 4096 UTF-8 bytes: the longest the dispatcher takes.'),
  line('dispatch-4097-byte-element', 'dispatch', ['node', fill(4097)],
    refused('argument-too-long', 1),
    'One byte over: refused, with the index of the element.'),
  line('dispatch-16384-byte-line', 'dispatch', ['node', fill(4096), fill(4096), fill(4096), fill(4088)],
    ok(['node', fill(4096), fill(4096), fill(4096), fill(4088)].join(' ')),
    'A line of exactly 16384 UTF-8 bytes: the longest the dispatcher reads.'),
  line('dispatch-16385-byte-line', 'dispatch', ['node', fill(4096), fill(4096), fill(4096), fill(4089)],
    refused('line-too-long', null),
    'One byte longer: refused as a whole line.'),
  line('cmd-program-files-x86', 'cmd', [WIN_NODE_X86, WIN_LAUNCHER, 'status', '--budget-ms', '20000'],
    ok('@"C:\\Program Files (x86)\\nodejs\\node.exe" "C:\\Users\\me\\.legion-control\\bin\\launcher.mjs" "status" "--budget-ms" "20000"'),
    'Every cmd element is double-quoted and the line starts with @. Parentheses are literal inside quotes; carets inside quotes are literal too, which is why the desktop broke this path by adding them (D10). The @ survives cmd /c whether or not sshd wrapped the line in quotes (A12).', ['finding:d10', 'finding:a12']),
  line('cmd-trailing-backslash', 'cmd', ['node', 'C:\\stage dir\\'],
    ok('@"node" "C:\\stage dir\\\\"'),
    'A trailing backslash is doubled, so the C runtime reads one backslash and a closing quote rather than an escaped quote.'),
  line('cmd-trailing-backslash-run', 'cmd', ['node', 'C:\\a\\\\'],
    ok('@"node" "C:\\a\\\\\\\\"'),
    'The whole trailing run is doubled: two backslashes become four. Backslashes anywhere else stand for themselves.'),
  line('cmd-operators-inside-quotes', 'cmd', ['node', 'a&b|c<d>e^f'],
    ok('@"node" "a&b|c<d>e^f"'),
    '& | < > and ^ are literal inside double quotes; no caret is added.'),
  line('cmd-percent', 'cmd', ['node', '%PATH%'],
    refused('percent', 1),
    'cmd expands %VAR% even inside quotes, so a percent sign cannot be carried.'),
  line('cmd-exclamation', 'cmd', ['node', 'wow!'],
    refused('exclamation', 1),
    'With delayed expansion on, cmd expands !VAR! inside quotes; an exclamation mark is refused.'),
  line('cmd-double-quote', 'cmd', ['node', 'say "hi"'],
    refused('double-quote', 1),
    'A double quote cannot be written inside a cmd line that both cmd and the C runtime read the same way.'),
  line('cmd-empty-element', 'cmd', ['node', ''],
    refused('empty-argument', 1),
    'An empty element is refused in cmd as everywhere.'),
  line('cmd-line-feed', 'cmd', ['node', 'a\r\nb'],
    refused('control-character', 1),
    'A carriage return or line feed would end the cmd line.'),
  line('cmd-encoded-powershell', 'cmd', ['powershell.exe', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', ENCODED],
    ok(`@"powershell.exe" "-NoProfile" "-NonInteractive" "-ExecutionPolicy" "Bypass" "-EncodedCommand" "${ENCODED}"`),
    'The enrolment fetch on Windows: an encoded PowerShell command run from cmd. + / and = in the base64 are plain characters in quotes.'),
  line('powershell-launcher-status', 'powershell', [WIN_NODE, WIN_LAUNCHER, 'status'],
    ok("& 'C:\\Program Files\\nodejs\\node.exe' 'C:\\Users\\me\\.legion-control\\bin\\launcher.mjs' 'status'"),
    'A PowerShell login: every element single-quoted, and the call operator in front so a quoted path is run, not printed.'),
  line('powershell-single-quote', 'powershell', ['node', "it's"],
    ok("& 'node' 'it''s'"),
    'A single quote inside a PowerShell single-quoted string is written twice.'),
  line('powershell-typographic-quotes', 'powershell', ['node', '\u2018a\u2019\u201ab\u201b'],
    ok("& 'node' '\u2018\u2018a\u2019\u2019\u201a\u201ab\u201b\u201b'"),
    'U+2018 to U+201B are single quotes to the PowerShell tokenizer, so each is doubled like U+0027. Doubling only the ASCII quote let one of these end the string early (T11, A12).', ['finding:t11', 'finding:a12']),
  line('powershell-variable-literal', 'powershell', ['node', '$env:USERPROFILE'],
    ok("& 'node' '$env:USERPROFILE'"),
    'Nothing expands inside single quotes: $env:USERPROFILE reaches the agent as those sixteen characters.'),
  line('powershell-stop-parsing', 'powershell', ['node', '--%'],
    ok("& 'node' '--%'"),
    'The stop-parsing token only works bare; quoted it is an ordinary argument.'),
  line('powershell-trailing-backslash', 'powershell', ['node', 'C:\\dir\\'],
    refused('trailing-backslash', 1),
    'Windows PowerShell 5.1 passes a quoted native argument with a trailing backslash without doubling it, so the argument swallows the next one.'),
  line('powershell-double-space', 'powershell', ['node', 'a  b'],
    refused('double-space', 1),
    'Two spaces in a row are refused: PowerShell legacy native-argument passing does not keep them reliably.'),
  line('powershell-double-quote', 'powershell', ['node', 'say "hi"'],
    refused('double-quote', 1),
    'A double quote inside a native argument is mangled by legacy argument passing.'),
  line('powershell-empty-element', 'powershell', ['node', ''],
    refused('empty-argument', 1),
    'Windows PowerShell drops an empty native argument altogether; it is refused instead.'),
];

// --- classifications -------------------------------------------------------------

const EVIDENCE = { launchFailed: false, timedOut: false, exitCode: 255, stdout: '', stderr: '', sshLog: '' };

/**
 * One attempt's evidence and what it says. Android has no ssh log and no
 * separate launch step (it uses its in-process client and maps its own events),
 * so it runs only the cases without either.
 */
const reading = (name, evidence, want, purpose, tags = []) => {
  const input = { ...EVIDENCE, ...evidence };
  return {
    name, purpose, tags,
    consumers: input.sshLog === '' && !input.launchFailed ? CLIENTS : DESKTOPS,
    input,
    want,
  };
};
const never = (kind) => ({ outcome: 'failure', kind, dispatch: 'never' });
const unknown = (kind) => ({ outcome: 'failure', kind, dispatch: 'unknown' });
const reply = { outcome: 'reply', kind: null, dispatch: 'acknowledged', routeLevel: false };
const log = (...lines) => `${lines.join('\n')}\n`;
const JSON_REPLY = '{"ok":true,"contract":3,"agentVersion":"3.1.0","system":{"id":"linux","name":"Linux"}}\n';
const CONNECT = 'ssh: connect to host desk-tower.local port 22';

const classifications = [
  reading('log-connection-refused', { sshLog: log(`${CONNECT}: Connection refused`) }, never('connectionRefused'),
    'ssh logged that the port refused the connection: nothing was sent, another route may be tried.'),
  reading('log-connect-operation-timed-out', { sshLog: log(`${CONNECT}: Operation timed out`) }, never('hostUnreachable'),
    'macOS words a connect timeout this way. The host was never reached.'),
  reading('log-connect-connection-timed-out', { sshLog: log(`${CONNECT}: Connection timed out`) }, never('hostUnreachable'),
    'Linux words a connect timeout this way.'),
  reading('log-connect-no-route', { sshLog: log(`${CONNECT}: No route to host`) }, never('hostUnreachable'),
    'No route to the host: unreachable, never dispatched.'),
  reading('log-connect-network-unreachable', { sshLog: log(`${CONNECT}: Network is unreachable`) }, never('hostUnreachable'),
    'The network itself is unreachable from here.'),
  reading('log-connect-host-down', { sshLog: log(`${CONNECT}: Host is down`) }, never('hostUnreachable'),
    'The host is down: unreachable, never dispatched.'),
  reading('log-resolve-macos', { sshLog: log('ssh: Could not resolve hostname desk-tower.local: nodename nor servname provided, or not known') }, never('hostUnreachable'),
    'A name that does not resolve, in the macOS wording.'),
  reading('log-resolve-linux', { sshLog: log('ssh: Could not resolve hostname desk-tower.local: Name or service not known') }, never('hostUnreachable'),
    'The same failure in the Linux wording.'),
  reading('log-resolve-windows-crlf', { sshLog: 'ssh.exe: Could not resolve hostname desk-tower.local: No such host is known. \r\n' }, never('hostUnreachable'),
    'Windows OpenSSH names itself ssh.exe and ends its lines with CRLF. One trailing CR is removed from each line before the patterns are tried.'),
  reading('log-connect-refused-windows', { sshLog: 'ssh.exe: connect to host desk-tower.local port 22: Connection refused\r\n' }, never('connectionRefused'),
    'A refusal logged by ssh.exe with a CRLF.'),
  reading('log-host-key-banner', {
    sshLog: log(
      '@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@',
      '@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @',
      '@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@',
      'IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!',
    ),
  }, never('hostKeyChanged'),
  'The changed-key banner. The pinned key refused the host before anything was sent.'),
  reading('log-host-key-changed', { sshLog: log('Host key for desk-tower.local has changed and you have requested strict checking.') }, never('hostKeyChanged'),
    'The one-line form of a changed host key.'),
  reading('log-host-key-verification-failed', { sshLog: log('Host key verification failed.') }, never('hostKeyChanged'),
    'Host key verification failed with no other line: a key mismatch.'),
  reading('log-host-key-unknown', {
    sshLog: log('No ED25519 host key is known for desk-tower.local and you have requested strict checking.', 'Host key verification failed.'),
  }, never('hostKeyUnknown'),
  'An unknown key also prints "Host key verification failed.". The first row of the table with a matching line wins, and hostKeyUnknown comes first, so the device offers to pin rather than warning about a changed key.'),
  reading('log-permission-denied', { sshLog: log('me@desk-tower.local: Permission denied (publickey).') }, never('authenticationFailed'),
    'The key was refused: authentication failed before the channel opened.'),
  reading('log-permission-denied-several-methods', { sshLog: log('me@desk-tower.local: Permission denied (publickey,password,keyboard-interactive).') }, never('authenticationFailed'),
    'The same with several methods offered.'),
  reading('log-too-many-authentication-failures', {
    sshLog: log('Received disconnect from 192.0.2.10 port 22:2: Too many authentication failures', 'Disconnected from 192.0.2.10 port 22'),
  }, never('authenticationFailed'),
  'An agent offering too many keys: the server disconnected before any command.'),
  reading('log-no-more-authentication-methods', { sshLog: log('No more authentication methods to try.') }, never('authenticationFailed'),
    'Every method was tried and none was accepted.'),
  reading('log-kex-connection-closed', { sshLog: log('kex_exchange_identification: Connection closed by remote host') }, never('connectionRefused'),
    'The server closed the connection during key exchange: a refusal before any session.'),
  reading('log-kex-closed-with-proxy-line', {
    stderr: 'ProxyCommand: connecting through a jump host\n',
    sshLog: log('kex_exchange_identification: Connection closed by remote host'),
  }, never('connectionRefused'),
  'The same with a ProxyCommand line on stderr. What stderr says plays no part.'),
  reading('log-kex-connection-reset', { sshLog: log('kex_exchange_identification: read: Connection reset by peer') }, never('connectionRefused'),
    'Reset during key exchange: refused before any session.'),
  reading('log-ssh-exchange-identification', { sshLog: log('ssh_exchange_identification: Connection closed by remote host') }, never('connectionRefused'),
    'Older OpenSSH names the same step ssh_exchange_identification.'),
  reading('log-banner-exchange', { sshLog: log('banner exchange: Connection to 192.0.2.10 port 22: invalid format') }, never('connectionRefused'),
    'Something that is not an ssh server answered on the port.'),
  reading('log-banner-timeout', { sshLog: log('Connection timed out during banner exchange') }, never('hostUnreachable'),
    'Nothing answered the banner in time: unreachable.'),
  reading('log-unable-to-negotiate', { sshLog: log('Unable to negotiate with 192.0.2.10 port 22: no matching host key type found. Their offer: ssh-rsa') }, never('connectionRefused'),
    'No common algorithm: the connection is refused before authentication.'),
  reading('log-channel-open-failed', { sshLog: log('channel 0: open failed: administratively prohibited: open failed') }, never('connectionRefused'),
    'The server refused the session channel, so no command was sent.'),
  reading('log-exec-request-failed', { sshLog: log('exec request failed on channel 0') }, never('connectionRefused'),
    'The server refused the exec request itself.'),
  reading('log-bad-permissions', { sshLog: log('Bad owner or permissions on /Users/me/.ssh/config') }, never('launchFailed'),
    'ssh refused its own configuration file and connected to nothing.'),
  reading('log-bad-configuration-option', { sshLog: log('/Users/me/.ssh/config: line 4: Bad configuration option: usekeychain', '/Users/me/.ssh/config: terminating, 1 bad configuration options') }, never('launchFailed'),
    'A configuration option this ssh does not know stops it before it connects.'),
  reading('log-command-line-option', { sshLog: log('command-line line 0: Unsupported option "updatehostkeys"') }, never('launchFailed'),
    'An -o option this ssh does not know, from the client itself.'),
  reading('log-user-config-missing', { sshLog: log("Can't open user config file /Users/me/.ssh/missing: No such file or directory") }, never('launchFailed'),
    'An -F file that does not exist: ssh stops before it connects.'),
  reading('log-command-line-and-remote-command', { sshLog: log('Cannot execute command-line and remote command.') }, never('launchFailed'),
    'A RemoteCommand in ssh_config conflicts with the command line; the client passes RemoteCommand=none, and this is what is left if it did not.'),
  reading('unexplained-255-empty', {}, unknown('linkLost'),
    'Exit 255 with nothing at all to explain it. The command may have run before the link dropped, so a mutation is never sent again (M15).', ['finding:m15']),
  reading('unexplained-255-broken-pipe', { sshLog: log('client_loop: send disconnect: Broken pipe') }, unknown('linkLost'),
    'The link broke during the session: the command may already be running.', ['finding:m15']),
  reading('unexplained-255-closed-by-remote', { sshLog: log('Connection to desk-tower.local closed by remote host.') }, unknown('linkLost'),
    'The remote side closed an open session, which happens after a command started as easily as before.', ['finding:m15']),
  reading('unexplained-255-read-timed-out', { sshLog: log('Read from remote host desk-tower.local: Operation timed out') }, unknown('linkLost'),
    'A read timed out inside the session, after dispatch.', ['finding:m15']),
  reading('unexplained-255-read-reset', { sshLog: log('Read from remote host desk-tower.local: Connection reset by peer') }, unknown('linkLost'),
    'A reset inside the session, after dispatch.', ['finding:m15']),
  reading('unexplained-255-server-not-responding', { sshLog: log('Timeout, server desk-tower.local not responding.') }, unknown('linkLost'),
    'Keepalives went unanswered during the session.', ['finding:m15']),
  reading('unexplained-255-closed-alone', { sshLog: log('Connection closed by 192.0.2.10 port 22') }, unknown('linkLost'),
    'A bare "Connection closed" says nothing about when: it is not in the pre-dispatch table.', ['finding:m15']),
  reading('refusal-in-stderr-only', { stderr: `${CONNECT}: Connection refused\n` }, unknown('linkLost'),
    'The refused line on stderr and an empty ssh log. stderr is the remote program\'s as much as ssh\'s, and the agent writes raw text to it, so this is no evidence that nothing ran (M15).', ['finding:m15']),
  reading('refusal-logged-with-output', { stdout: 'starting\n', sshLog: log(`${CONNECT}: Connection refused`) }, unknown('linkLost'),
    'A pre-dispatch line in the log but output on stdout: something ran, so the log line cannot be the whole story.', ['finding:m15']),
  reading('exit-127-sh-not-found', { exitCode: 127, stderr: 'sh: 1: /usr/bin/node: not found\n' }, never('interpreterMissing'),
    'A shell that could not find the interpreter exits 127 with nothing on stdout: the agent never started, so another system may be tried (A4).', ['finding:a4']),
  reading('exit-127-fish-unknown-command', { exitCode: 127, stderr: "fish: Unknown command: node\n" }, never('interpreterMissing'),
    'fish says it differently and exits the same way.', ['finding:a4']),
  reading('exit-126-permission-denied', { exitCode: 126, stderr: 'bash: /usr/bin/node: Permission denied\n' }, never('interpreterMissing'),
    'An interpreter that is not executable: exit 126, nothing ran.', ['finding:a4']),
  reading('exit-9009-cmd-not-recognized', { exitCode: 9009, stderr: "'node' is not recognized as an internal or external command,\r\noperable program or batch file.\r\n" }, never('interpreterMissing'),
    'cmd exits 9009 when the command does not exist.', ['finding:a4']),
  reading('exit-127-blank-crlf-stdout', { exitCode: 127, stdout: '\r\n' }, never('interpreterMissing'),
    'Blank means tab, LF, CR and space only, so a stray CRLF on stdout is still blank.', ['finding:a4']),
  reading('exit-127-with-output', { exitCode: 127, stdout: 'partial output\n' }, unknown('unreadableOutput'),
    'Exit 127 with something on stdout: something ran, so it is not the shell failing to start it.'),
  reading('powershell-term-not-recognized', {
    exitCode: 1,
    stderr: "node : The term 'node' is not recognized as the name of a cmdlet, function, script file, or operable program.\r\n",
  }, unknown('interpreterMissing'),
  'PowerShell exits 1 for a missing command, and 1 is also what the agent exits with, so the text names the failure without making it never.'),
  reading('cmd-path-not-found', { exitCode: 3, stderr: 'The system cannot find the path specified.\r\n' }, unknown('interpreterMissing'),
    'cmd could not find a path: named for a person, still unknown.'),
  reading('cannot-find-module', { exitCode: 1, stderr: "Error: Cannot find module '/home/me/.legion-control/agent/src/index.mjs'\n" }, unknown('agentMissing'),
    'Node ran and the entry is missing. A mutation is not tried on another system on the strength of stderr text (A4).', ['finding:a4']),
  reading('err-module-not-found', { exitCode: 1, stderr: 'node:internal/errors:496\n    ErrorCaptureStackTrace(err);\nError [ERR_MODULE_NOT_FOUND]: Cannot find package\n' }, unknown('agentMissing'),
    'The ESM spelling of a missing module.', ['finding:a4']),
  reading('no-such-file', { exitCode: 1, stderr: 'bash: /home/me/missing: No such file or directory\n' }, unknown('unreadableOutput'),
    'exit 1 with "No such file or directory": nothing in the table, so it is unreadable output.'),
  reading('exit-0-empty', { exitCode: 0 }, unknown('unreadableOutput'),
    'Exit 0 with no output at all: the agent always answers with JSON, so this is not a reply.'),
  reading('not-json-braces', { exitCode: 0, stdout: '{oops}\n' }, unknown('unreadableOutput'),
    'Braces that do not hold JSON are not a reply.'),
  reading('reply-exit-0', { exitCode: 0, stdout: JSON_REPLY }, reply,
    'A JSON object on stdout is a reply, whatever else is true.'),
  reading('reply-exit-255', { exitCode: 255, stdout: '{"ok":true,"contract":3,"agentVersion":"3.1.0","system":{"id":"linux","name":"Linux"},"action":"rebooting"}\n' }, reply,
    'A reboot drops the link after the agent answered; the reply on stdout wins over the 255.'),
  reading('reply-exit-1', { exitCode: 1, stdout: '{"ok":false,"contract":3,"agentVersion":"3.1.0","system":{"id":"linux","name":"Linux"},"reasonCode":"busy","message":"busy"}\n' }, reply,
    'ok false is still a reply: the agent answered.'),
  reading('reply-after-clixml', { exitCode: 0, stdout: `#< CLIXML\r\n${JSON_REPLY}` }, reply,
    'Windows PowerShell can print a #< CLIXML banner first; the object from the first { to the last } is the reply.'),
  reading('reply-after-timeout', { timedOut: true, exitCode: null, stdout: JSON_REPLY }, reply,
    'The watchdog fired after the agent had already answered: the answer counts.'),
  reading('no-exit-status-blank', { exitCode: null }, unknown('linkLost'),
    'The channel closed without an exit status: the link was lost, and nothing says when.'),
  reading('no-exit-status-reply', { exitCode: null, stdout: JSON_REPLY }, reply,
    'No exit status, but a reply arrived before the link went: it is a reply.'),
  reading('launch-failed', { launchFailed: true, exitCode: null }, never('launchFailed'),
    'ssh could not be started at all: nothing can have been sent.'),
  reading('timed-out', { timedOut: true, exitCode: null }, unknown('timedOut'),
    'The client stopped the process: the command may be running.'),
];

// --- walks ---------------------------------------------------------------------

/** What each attempt would end in, as classify() reads it. Evaluated when the vectors are built. */
const ENDINGS = {
  reply: { exitCode: 0, stdout: JSON_REPLY },
  unexplained255: {},
  refused: { sshLog: log(`${CONNECT}: Connection refused`) },
  unreachable: { sshLog: log(`${CONNECT}: Connection timed out`) },
  hostKeyChanged: { sshLog: log('Host key for desk-tower.local has changed and you have requested strict checking.') },
  authenticationFailed: { sshLog: log('me@desk-tower.local: Permission denied (publickey).') },
  exit127: { exitCode: 127, stderr: 'sh: 1: /usr/bin/node: not found\n' },
  cannotFindModule: { exitCode: 1, stderr: "Error: Cannot find module '/home/me/.legion-control/agent/src/index.mjs'\n" },
  powershellNotRecognized: { exitCode: 1, stderr: "node : The term 'node' is not recognized as the name of a cmdlet, function, script file, or operable program.\r\n" },
  unreadable: { exitCode: 0, stdout: '{oops}\n' },
  watchdog: { timedOut: true, exitCode: null },
};
// A line refused by section 15.2 is never sent, so it has no evidence to read.
const BAD_REQUEST = { outcome: 'failure', kind: 'badRequest', dispatch: 'never', routeLevel: false };

const walkCase = (name, safety, steps, want, purpose, tags = []) => ({
  name, purpose, tags,
  input: () => ({
    safety,
    attempts: steps.map(([route, system, ending]) => ({
      route, system,
      result: ending === 'badRequest' ? BAD_REQUEST : classify({ ...EVIDENCE, ...ENDINGS[ending] }),
    })),
  }),
  want,
});

const walks = [
  walkCase('mutation-stops-after-unexplained-255', 'mutation',
    [['lan', 'linux', 'unexplained255'], ['lan', 'windows', 'reply'], ['tailnet', 'linux', 'reply']],
    { sent: [0], ending: 'stopped' },
    'An unexplained 255 may have run the restart, so the mutation goes nowhere else: exactly one attempt across two routes and two systems (M15).', ['finding:m15']),
  walkCase('mutation-fails-over-after-refusal', 'mutation',
    [['lan', 'linux', 'refused'], ['lan', 'windows', 'reply'], ['tailnet', 'linux', 'reply']],
    { sent: [0, 2], ending: 'reply' },
    'A refusal in the ssh log proves nothing was sent, so the mutation moves on; the refusal is route-level, so the rest of that route is skipped.'),
  walkCase('mutation-next-system-after-127', 'mutation',
    [['lan', 'linux', 'exit127'], ['lan', 'windows', 'reply']],
    { sent: [0, 1], ending: 'reply' },
    'Exit 127 with blank stdout means the interpreter never started: the same route\'s other system is tried (A4).', ['finding:a4']),
  walkCase('mutation-stops-after-cannot-find-module', 'mutation',
    [['lan', 'linux', 'cannotFindModule'], ['lan', 'windows', 'reply']],
    { sent: [0], ending: 'stopped' },
    'Node ran and printed "Cannot find module". stderr text never makes a mutation safe to resend (A4).', ['finding:a4']),
  walkCase('mutation-stops-after-powershell-not-recognized', 'mutation',
    [['lan', 'windows', 'powershellNotRecognized'], ['lan', 'linux', 'reply']],
    { sent: [0], ending: 'stopped' },
    'PowerShell\'s "not recognized" comes with exit 1, which the agent also uses: stopped.', ['finding:a4']),
  walkCase('mutation-stops-after-unreadable-output', 'mutation',
    [['lan', 'linux', 'unreadable'], ['lan', 'windows', 'reply']],
    { sent: [0], ending: 'stopped' },
    'Output nobody can read was still output: the command ran, and is reconciled with op, never resent.'),
  walkCase('mutation-stops-after-watchdog', 'mutation',
    [['lan', 'linux', 'watchdog'], ['tailnet', 'linux', 'reply']],
    { sent: [0], ending: 'stopped' },
    'The client\'s own watchdog fired: the work may be running.'),
  walkCase('mutation-continues-after-bad-request', 'mutation',
    [['lan', 'windows', 'badRequest'], ['lan', 'linux', 'reply']],
    { sent: [0, 1], ending: 'reply' },
    'A line refused before sending was never sent, so the next system is tried.'),
  walkCase('mutation-exhausted-when-nothing-was-sent', 'mutation',
    [['lan', 'linux', 'refused'], ['tailnet', 'linux', 'unreachable']],
    { sent: [0, 1], ending: 'exhausted' },
    'Every attempt failed before dispatch: the walk runs out with nothing sent anywhere.'),
  walkCase('read-continues-after-unexplained-255', 'read',
    [['lan', 'linux', 'unexplained255'], ['lan', 'windows', 'reply'], ['tailnet', 'linux', 'reply']],
    { sent: [0, 2], ending: 'reply' },
    'A read is safe to repeat, so it goes on; a lost link is route-level, so it moves to the next route.', ['finding:m15']),
  walkCase('read-continues-after-cannot-find-module', 'read',
    [['lan', 'linux', 'cannotFindModule'], ['lan', 'windows', 'reply']],
    { sent: [0, 1], ending: 'reply' },
    'A missing module is not route-level: a read tries the same route\'s next system.'),
  walkCase('read-exhausted-after-route-failures', 'read',
    [['lan', 'linux', 'hostKeyChanged'], ['lan', 'windows', 'reply'], ['tailnet', 'linux', 'authenticationFailed']],
    { sent: [0, 2], ending: 'exhausted' },
    'A changed host key kills the lan route, its other system is skipped, and authentication fails on the next route.'),
];

// --- statusBudgets ---------------------------------------------------------------

const budget = (name, remainingMs, attemptsMade, want, purpose) => ({
  name, purpose, tags: ['finding:c9'], input: { remainingMs, attemptsMade }, want,
});

const statusBudgets = [
  budget('first-attempt-full-deadline', 30000, 0, { attempt: true, budgetMs: 20000 },
    'A fresh 30 s status: the agent gets its full 20 s.'),
  budget('cap-holds-with-time-to-spare', 120000, 0, { attempt: true, budgetMs: 20000 },
    'Never more than 20 s, however much time is left.'),
  budget('second-attempt-exactly-enough', 25000, 1, { attempt: true, budgetMs: 20000 },
    '25 s left: 20 s for the agent and 5 s in hand.'),
  budget('second-attempt-one-short', 24999, 1, { attempt: true, budgetMs: 19999 },
    'One millisecond less: the budget shrinks with the time left.'),
  budget('third-attempt-after-two-dead-routes', 14000, 2, { attempt: true, budgetMs: 9000 },
    'Two dead routes left 14 s. Sending 20000 here made the agent overrun the client\'s own deadline (C9).'),
  budget('floor-for-a-later-attempt', 6000, 1, { attempt: true, budgetMs: 1000 },
    'Exactly 1 s left after the margin: still worth an attempt.'),
  budget('no-later-attempt-without-time', 5999, 1, { attempt: false, budgetMs: null },
    'Less than 1 s left after the margin, and an attempt was already made: stop.'),
  budget('first-attempt-always-goes', 3000, 0, { attempt: true, budgetMs: 1000 },
    'The first attempt goes out whatever is left, with the 1 s floor.'),
];

// --- hosts, users, ports -------------------------------------------------------

const host = (name, value, want, purpose, tags = []) => ({ name: `host-${name}`, purpose, tags, input: { value }, want });
const dns = { valid: true, form: 'dns' };
const ipv4 = { valid: true, form: 'ipv4' };
const ipv6 = { valid: true, form: 'ipv6' };
const no = { valid: false, form: null };
const injection = (name, value, purpose) => host(name, value, no, purpose, ['injection', 'finding:t5']);

const LONGEST = [fill(63), 'b'.repeat(63), 'c'.repeat(63), 'd'.repeat(61)].join('.');

const hosts = [
  host('single-label', 'desk-tower', dns, 'A bare name, resolved by the device\'s own search domains.'),
  host('local-name', 'desk-tower.local', dns, 'An mDNS name in the .local domain.'),
  host('deep-name', 'laptop.tailnet.example.net', dns, 'A tailnet name with several labels.'),
  host('underscore-alias', 'my_alias', dns, 'An underscore is allowed: it is what ssh_config Host aliases often contain.'),
  host('longest-name', LONGEST, dns, 'Exactly 253 characters of 63-character labels: the longest name.'),
  host('ipv4-documentation', '192.0.2.10', ipv4, 'A dotted quad of four decimal octets.'),
  host('ipv4-cgnat', '100.64.0.10', ipv4, 'A tailnet address in the shared range.'),
  host('ipv6-compressed', '2001:db8::10', ipv6, 'An IPv6 address with a compressed run of zeros.'),
  host('ipv6-loopback', '::1', ipv6, 'The IPv6 loopback, all compressed.'),
  host('ipv6-mapped-ipv4', '::ffff:192.0.2.1', ipv6, 'An IPv4-mapped IPv6 address: an embedded dotted quad as the last group.'),
  injection('empty', '', 'Nothing at all is not a host.'),
  injection('leading-dash-option', '-oProxyCommand=x', 'A host that starts with a hyphen is an ssh option, and -oProxyCommand runs a command on this device.'),
  injection('known-hosts-pattern', 'victim.example,*', 'A comma and a star make a known_hosts pattern that pins one key for every host on OpenSSH before 9.6.'),
  injection('wildcard', '*.example', 'A wildcard is a pattern, never a host.'),
  injection('space', 'host name', 'A space splits the host into two arguments.'),
  injection('semicolon', 'host;id', 'A semicolon is shell syntax.'),
  injection('bracketed-ipv6', '[::1]', 'Brackets are known_hosts syntax for a port; a pin pattern is built from the checked bare address instead.'),
  injection('scheme', 'ssh://h', 'A URL is not a host.'),
  injection('user-at-host', 'me@h', 'A user belongs in user; an @ in the host would change which account ssh logs in to.'),
  host('empty-label', 'a..b', no, 'Two dots in a row leave an empty label.'),
  host('leading-dot', '.a', no, 'A name cannot start with a dot.'),
  host('trailing-dot', 'a.', no, 'A trailing dot passes the document guard and is not dialled: names are written without it.'),
  host('label-ends-with-hyphen', 'a-.b', no, 'A label cannot end with a hyphen.'),
  host('octet-over-255', '256.1.1.1', no, 'All digits in the last label make it an address, and 256 is no octet.'),
  host('three-octets', '1.2.3', no, 'Three octets would be completed by a resolver into an address nobody wrote.'),
  host('all-digits', '1234', no, 'A single all-digit label is an integer address to a resolver.'),
  host('leading-zero-octet', '01.2.3.4', no, 'A leading zero reads as octal to some resolvers.'),
  host('ipv6-zone', 'fe80::1%en0', no, 'A zone names an interface of the device that wrote it; it means nothing elsewhere.'),
  host('ipv6-two-compressions', '1::2::3', no, 'Two :: are ambiguous.'),
  host('ipv4-then-compression', '1.2.3.4::', no, 'An embedded IPv4 address must be the last group.'),
  host('host-with-port', 'desk-tower.example:22', no, 'A port belongs in port.'),
  host('label-too-long', `${fill(64)}.example`, no, 'A 64-character label is one over the limit.'),
  host('name-too-long', `${LONGEST}d`, no, '254 characters is one over the limit.'),
  host('non-ascii-name', 'héllo.local', no, 'Names are ASCII; an internationalised name is written in its punycode form.'),
];

const user = (name, value, valid, purpose, tags = []) => ({ name: `user-${name}`, purpose, tags, input: { value }, want: { valid } });

const users = [
  user('plain', 'me', true, 'A local account name in lower case.'),
  user('dotted', 'me.example', true, 'Dots are allowed inside a name.'),
  user('leading-underscore', '_svc', true, 'A service account starting with an underscore.'),
  user('email-style', 'me@example.net', true, 'A Microsoft account login on Windows.'),
  user('domain-backslash', 'AzureAD\\me', true, 'A domain account, DOMAIN\\name.'),
  user('single-space', 'me 2', true, 'A Windows display-style account name with one space.'),
  user('one-character', 'a', true, 'One character is a name.'),
  user('empty', '', false, 'Nothing at all is not a login name.'),
  user('option', '-o', false, 'A leading hyphen would be read as an ssh option.', ['injection', 'finding:t5']),
  user('semicolon', 'me;id', false, 'A semicolon is shell syntax.', ['injection', 'finding:t5']),
  user('command-substitution', 'me$(x)', false, 'A command substitution.', ['injection', 'finding:t5']),
  user('single-quote', "me'x", false, 'A quote would end a quoted run on the remote side.', ['injection', 'finding:t5']),
  user('double-space', 'me  2', false, 'Two spaces in a row are refused.'),
  user('space-then-hyphen', 'me -x', false, 'A space followed by a hyphen looks like an option to whatever splits on spaces.'),
  user('leading-space', ' me', false, 'A name starts with a letter, digit or underscore.'),
  user('trailing-space', 'me ', false, 'A name ends with a letter, digit, dot, underscore or hyphen.'),
  user('trailing-backslash', 'me\\', false, 'A trailing backslash would escape whatever follows it.'),
  user('trailing-at', 'me@', false, 'An @ at the end leaves an empty host.'),
  user('too-long', fill(65), false, '65 characters is one over the limit.'),
  user('non-ascii', 'mé', false, 'Login names are ASCII here.'),
];

const port = (name, value, valid, purpose) => ({ name: `port-${name}`, purpose, tags: [], input: { value }, want: { valid } });

const ports = [
  port('lowest', 1, true, 'Port 1 is the lowest port there is.'),
  port('ssh', 22, true, 'The ssh port, the one nearly every endpoint uses.'),
  port('highest', 65535, true, 'The highest port there is.'),
  port('zero', 0, false, 'Port 0 is not dialled.'),
  port('over-highest', 65536, false, 'One over the highest port.'),
  port('negative', -1, false, 'A negative number is no port.'),
  port('string', '22', false, 'A port is a JSON integer, never a string that looks like one.'),
  port('fraction', 22.5, false, 'A fraction is no port.'),
];

export default {
  family: 'transport',
  section: '15',
  reference: 'tools/transport.mjs',
  notes: [
    'grammars pins resolveGrammar (section 15.1); commandLines pins serialize (15.2); classifications pins classify (15.4); walks pins walk and statusBudgets pins statusBudget (15.5); hosts, users and ports pin hostForm, isUser and isPort (15.6).',
    'A command line is compared byte for byte. A refusal is null line, the code, and the index of the refused element, or null when the whole argv or line is refused.',
    'classifications inputs are the evidence of one attempt. sshLog is the text ssh wrote with -E; one trailing CR is removed from each line before the patterns are tried. Android, which has no ssh log, runs the cases whose sshLog is empty and launchFailed is false.',
    'A walk result is what classify returns for that attempt, or a badRequest failure for a line section 15.2 refused. sent lists the indices of the attempts made.',
    'Cases tagged injection are strings that try to be ssh options, shell syntax or known_hosts patterns; the document guard in common.schema.json refuses them as well.',
  ],
  consumers: {
    grammars: EVERYONE,
    commandLines: CLIENTS,
    classifications: DESKTOPS,
    walks: CLIENTS,
    statusBudgets: CLIENTS,
    hosts: EVERYONE,
    users: EVERYONE,
    ports: EVERYONE,
  },
  groups: { grammars, commandLines, classifications, walks, statusBudgets, hosts, users, ports },
};
