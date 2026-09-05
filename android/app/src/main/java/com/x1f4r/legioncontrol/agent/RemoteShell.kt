package com.x1f4r.legioncontrol.agent

/**
 * Which shell the far side hands an ssh command to, and how to write an argument for it.
 *
 * ssh does not take an argv. It takes one string and gives it to the account's login shell, so every
 * argument this app sends has to survive that shell's own reading of it. Which shell that is belongs
 * to the machine and cannot be guessed from the platform: OpenSSH on Windows runs cmd.exe unless the
 * DefaultShell registry value says otherwise, and plenty of installs set that to PowerShell.
 *
 * [AUTO] is what a configuration that has not said gets, and it is deliberately not a guess. While
 * every argument is one that means the same thing in all three shells, the command is written exactly
 * as it always was and nothing changes. The moment one of them would need quoting, the answer depends
 * on a shell nobody has named, and that is reported rather than resolved by picking one.
 */
enum class RemoteShell(val wire: String) {
    /** sh, bash, zsh, and anything else that reads a single-quoted string literally. */
    POSIX("posix"),

    /** cmd.exe, which is what OpenSSH on Windows starts with. */
    CMD("cmd"),

    /** PowerShell or pwsh, set as the account's DefaultShell. */
    POWERSHELL("powershell"),

    /** The configuration did not say. Safe only while nothing needs quoting. */
    AUTO("auto"),
    ;

    companion object {
        fun fromWire(value: String?): RemoteShell =
            entries.firstOrNull { it.wire.equals(value?.trim(), ignoreCase = true) } ?: AUTO
    }
}

/** An argument that cannot be written for the shell the far side is using, and why. */
class UnquotableArgument(val argument: String, val reason: String) :
    IllegalArgumentException(reason)

/**
 * The characters the contract's argv grammar allows: no spaces, no quotes, no dollar, no semicolon.
 *
 * An argument made only of these means the same thing to sh, cmd.exe and PowerShell, which is to say
 * nothing at all, so it is written exactly as it stands. That is also byte for byte what the previous
 * release sent, so every configuration that worked before produces the same command line now.
 *
 * The character set only, without the contract's "starts with a letter or a digit" rule. That rule
 * is about values this app *sends*, and it is enforced separately by [Contract.requireToken]; the
 * arguments checked here include absolute paths out of the configuration, which begin with a slash,
 * and `~/.legion-control/agent/src/index.mjs`, which begins with a tilde.
 *
 * `~` is in the set on purpose. A tilde inside quotes is a directory literally called `~` rather
 * than the home directory, so quoting it would break every install that uses the short form.
 */
private val BARE_SAFE = Regex("^[A-Za-z0-9._:@/\\\\~=+-]+$")

/**
 * cmd.exe reads these after CommandLineToArgvW has already finished, so no amount of double quoting
 * hides them: `%` still expands a variable and `!` still expands one under delayed expansion. They
 * can be escaped with a caret outside quotes, but not inside them, and every argument this app sends
 * that is long enough to need quoting is a path. Refusing is honest; a caret in the wrong place
 * silently changes the path.
 */
private val CMD_UNSAFE = charArrayOf('%', '!', '^', '&', '|', '<', '>', '\n', '\r')

/**
 * One command line for [shell], out of an argv that was never one.
 *
 * @throws UnquotableArgument when an argument cannot be written for that shell, which is a fault in
 *   the configuration rather than a fault at the far side, and is reported as such.
 */
fun buildRemoteCommand(shell: RemoteShell, arguments: List<String>): String {
    require(arguments.isNotEmpty()) { "a remote command needs at least one argument" }
    for (argument in arguments) {
        if (argument.isEmpty() && shell == RemoteShell.AUTO) {
            throw UnquotableArgument(
                argument,
                "An empty argument has to be quoted, and this system's configuration does not say " +
                    "which shell it logs in to. Add \"shell\": \"posix\", \"cmd\" or \"powershell\" " +
                    "to it.",
            )
        }
        if (argument.contains('\u0000')) {
            throw UnquotableArgument(argument, "An argument contains a null byte.")
        }
    }

    // Nothing needs quoting, so nothing is quoted. This is the case every existing configuration is
    // already in, and it produces byte for byte what this app has always sent.
    if (arguments.all { it.isNotEmpty() && BARE_SAFE.matches(it) }) {
        return arguments.joinToString(" ")
    }

    return when (shell) {
        RemoteShell.POSIX -> arguments.joinToString(" ") { quotePosix(it) }
        RemoteShell.CMD -> quoteCmdLine(arguments)
        RemoteShell.POWERSHELL -> quotePowerShellLine(arguments)
        RemoteShell.AUTO -> {
            val offender = arguments.first { !BARE_SAFE.matches(it) }
            throw UnquotableArgument(
                offender,
                "\"$offender\" has to be quoted, and this system's configuration does not say which " +
                    "shell it logs in to. Add \"shell\": \"posix\", \"cmd\" or \"powershell\" to the " +
                    "system so the argument can be written correctly.",
            )
        }
    }
}

/**
 * Single quotes, with the one escape sh has: close the quote, write an escaped quote, open it again.
 * Everything else inside single quotes is literal, including backslashes, dollars and newlines, so
 * this is total rather than a list of characters somebody has to keep up to date.
 */
internal fun quotePosix(argument: String): String {
    if (argument.isNotEmpty() && BARE_SAFE.matches(argument)) return argument
    return "'" + argument.replace("'", "'\\''") + "'"
}

/**
 * The Windows argv encoding, which is the reverse of what CommandLineToArgvW does.
 *
 * Backslashes are only special in front of a quote: a run of them before one is doubled and the
 * quote is escaped, a run of them at the end of a quoted argument is doubled so the closing quote is
 * not eaten, and everywhere else they stand for themselves. Getting this wrong is how
 * `C:\Program Files\` turns into a quote that never closes.
 */
internal fun quoteWindowsArgv(argument: String): String {
    if (argument.isNotEmpty() && BARE_SAFE.matches(argument)) return argument
    val out = StringBuilder("\"")
    var backslashes = 0
    for (character in argument) {
        when (character) {
            '\\' -> backslashes += 1
            '"' -> {
                out.append("\\".repeat(backslashes * 2 + 1))
                out.append('"')
                backslashes = 0
            }

            else -> {
                out.append("\\".repeat(backslashes))
                out.append(character)
                backslashes = 0
            }
        }
    }
    out.append("\\".repeat(backslashes * 2))
    out.append('"')
    return out.toString()
}

private fun quoteCmdLine(arguments: List<String>): String = arguments.joinToString(" ") { argument ->
    val offender = argument.firstOrNull { it in CMD_UNSAFE }
    if (offender != null) {
        throw UnquotableArgument(
            argument,
            "\"$argument\" contains '$offender', which cmd.exe expands even inside quotes. Move the " +
                "agent somewhere without it, or set \"shell\": \"powershell\" on this system if that " +
                "is what it actually logs in to.",
        )
    }
    quoteWindowsArgv(argument)
}

/**
 * PowerShell, where a single-quoted string is literal and an embedded quote is written twice.
 *
 * The call operator is what makes this a command rather than an expression. A line that begins with
 * a quoted string is a string literal in PowerShell and it prints the path instead of running it,
 * which is the one difference from cmd.exe that actually matters here.
 */
private fun quotePowerShellLine(arguments: List<String>): String {
    val quoted = arguments.map { "'" + it.replace("'", "''") + "'" }
    return "& " + quoted.joinToString(" ")
}
