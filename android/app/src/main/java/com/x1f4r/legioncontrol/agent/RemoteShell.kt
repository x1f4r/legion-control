package com.x1f4r.legioncontrol.agent

/**
 * The grammar a command line is written in, which is the shell (or the dispatcher) on the far side
 * that will read it.
 *
 * ssh does not take an argv. It takes one string and gives it to the account's login shell, so every
 * argument this app sends has to survive that shell's own reading of it. Contract section 15.1 says
 * which grammar a system gets, and section 15.2 says exactly which bytes an argv becomes in each one.
 * Every client produces the same bytes for the same argv, which `contract/vectors/transport.json`
 * checks case by case; this file is a port of `contract/tools/transport.mjs` and not an idea of its
 * own about quoting.
 *
 * There is deliberately no "not set" member any more. The previous release had one, and wrote a
 * command unquoted while nothing in it needed quoting. That was the same line in every shell, right
 * up to a Windows path: a backslash written bare, which the restricted dispatcher refuses, so a
 * restricted Windows machine could not be driven from this app at all.
 */
enum class RemoteShell(val wire: String) {
    /** sh, dash, bash, zsh, ksh and fish. Not csh or tcsh. */
    POSIX("posix"),

    /**
     * The agent's own forced-command dispatcher behind a restricted key. It is not a shell: it
     * splits the line itself and never expands anything, on every platform.
     */
    DISPATCH("dispatch"),

    /** cmd.exe, which is what a stock Windows OpenSSH starts. */
    CMD("cmd"),

    /** PowerShell or pwsh, set as the account's DefaultShell. Never inferred. */
    POWERSHELL("powershell"),
    ;

    companion object {
        /**
         * The value of a system's `shell`, exactly as the contract spells it, or null.
         *
         * Strict on purpose: `POSIX`, `pwsh` and `auto` are not shells this app knows how to write
         * for, and reading them as the nearest thing it does know is how a command ends up quoted
         * for the wrong reader.
         */
        fun fromWire(value: String?): RemoteShell? = when (value) {
            "posix" -> POSIX
            "cmd" -> CMD
            "powershell" -> POWERSHELL
            else -> null
        }

        /**
         * A grammar as a launch tuple records it (contract section 9.9), the dispatcher included, or
         * null for `refused:<code>` and anything else section 15.2 does not write lines in.
         */
        fun fromGrammar(value: String?): RemoteShell? = entries.firstOrNull { it.wire == value }
    }
}

/** Which grammar a system's command line is written in, or why it has none (section 15.1). */
data class GrammarChoice(val grammar: RemoteShell?, val refusal: String?) {
    companion object {
        fun of(grammar: RemoteShell) = GrammarChoice(grammar, null)
        fun refused(code: String) = GrammarChoice(null, code)
    }
}

/**
 * The grammar for one system, from the document's raw values.
 *
 * A system restricted on the route always gets the dispatcher grammar, whatever its `shell` says,
 * because the forced command is not a shell. Otherwise the configured shell wins, and with none the
 * platform's stock OpenSSH shell decides: cmd on Windows, a POSIX shell everywhere else. PowerShell
 * is never inferred and a line is never retried in another grammar: a guess that is wrong fails
 * loudly, and a retry in a second grammar can run the command twice.
 *
 * [platform] and [shell] are the document's own strings, compared exactly. [restricted] counts only
 * when it is true.
 */
fun grammarFor(
    platform: String?,
    shell: String?,
    restricted: Boolean?,
    restrictedSession: Boolean,
    agentLength: Int,
): GrammarChoice {
    if (restricted == true || restrictedSession) {
        return if (agentLength == 2) {
            GrammarChoice.of(RemoteShell.DISPATCH)
        } else {
            GrammarChoice.refused(RESTRICTED_AGENT_SHAPE)
        }
    }
    if (shell == null) {
        return GrammarChoice.of(if (platform == "windows") RemoteShell.CMD else RemoteShell.POSIX)
    }
    return RemoteShell.fromWire(shell)?.let(GrammarChoice::of) ?: GrammarChoice.refused(UNKNOWN_SHELL)
}

/** A dispatcher system's `agent` is interpreter and entry, and nothing else. */
const val RESTRICTED_AGENT_SHAPE = "restricted-agent-shape"

/** A `shell` this app does not write for. Only a document kept under applied intake has one. */
const val UNKNOWN_SHELL = "unknown-shell"

/**
 * An argv that cannot be written in its grammar, and the element that failed.
 *
 * Nothing is ever sent with an element dropped or "fixed". The fault is in the configuration rather
 * than at the far side, and it is the same on every route, so it is reported rather than retried.
 */
class CommandLineRefusal(
    /** The contract's code, e.g. `percent` or `posix-backslash`. */
    val code: String,
    /** The element that failed, from 0, or null when the whole argv or line was refused. */
    val index: Int?,
    val argument: String?,
) : Exception(describeRefusal(code, argument))

/**
 * One command line for [grammar], out of an argv that was never one (section 15.2).
 *
 * @throws CommandLineRefusal when an element cannot be written in that grammar.
 */
fun buildRemoteCommand(grammar: RemoteShell, argv: List<String>): String {
    if (argv.isEmpty()) throw CommandLineRefusal("no-command", null, null)
    if (grammar == RemoteShell.DISPATCH && argv.size > DISPATCH_MAX_ARGUMENTS) {
        throw CommandLineRefusal("too-many-arguments", null, null)
    }
    val tokens = argv.mapIndexed { index, value ->
        fun refuse(code: String): Nothing = throw CommandLineRefusal(code, index, value)
        if (value.isEmpty()) refuse("empty-argument")
        if (value.any { it.isControlCharacter() }) refuse("control-character")
        when (grammar) {
            RemoteShell.POSIX, RemoteShell.DISPATCH -> {
                if (grammar == RemoteShell.POSIX && POSIX_BACKSLASH.containsMatchIn(value)) {
                    refuse("posix-backslash")
                }
                if (grammar == RemoteShell.DISPATCH && value.utf8Size() > DISPATCH_MAX_ARGUMENT_BYTES) {
                    refuse("argument-too-long")
                }
                quotePosix(value, first = index == 0)
            }

            RemoteShell.CMD -> {
                if ('"' in value) refuse("double-quote")
                if ('%' in value) refuse("percent")
                if ('!' in value) refuse("exclamation")
                // The C runtime reads a run of backslashes before a quote as escapes, so the trailing
                // run is doubled and the closing quote stays a quote. Backslashes anywhere else stand
                // for themselves.
                "\"" + value + value.takeLastWhile { it == '\\' } + "\""
            }

            RemoteShell.POWERSHELL -> {
                if ('"' in value) refuse("double-quote")
                if (value.endsWith('\\')) refuse("trailing-backslash")
                if ("  " in value) refuse("double-space")
                // PowerShell reads --% as its stop-parsing token even quoted.
                if (value == "--%") refuse("stop-parsing")
                "'" + POWERSHELL_QUOTE.replace(value) { it.value + it.value } + "'"
            }
        }
    }
    val body = tokens.joinToString(" ")
    if (grammar == RemoteShell.DISPATCH && body.utf8Size() > DISPATCH_MAX_LINE_BYTES) {
        throw CommandLineRefusal("line-too-long", null, null)
    }
    return when (grammar) {
        // The `@` is what makes the line survive `cmd /c` whether or not sshd wrapped it in quotes:
        // cmd strips the outer pair only when the line starts with one.
        RemoteShell.CMD -> "@$body"
        // A line that begins with a quoted string is a string literal to PowerShell, which prints
        // the path rather than running it. The call operator makes it a command.
        RemoteShell.POWERSHELL -> "& $body"
        RemoteShell.POSIX, RemoteShell.DISPATCH -> body
    }
}

/**
 * Single quotes, with the one escape sh has: close the quote, write an escaped quote, open it again.
 *
 * An element made only of the bare set is written as it stands. `~` is not in that set: argv
 * elements are literal, and a bare tilde would be expanded by every shell but the dispatcher, so the
 * same document would run a different file depending on which of the two received it. `=` may not
 * start a word, which zsh expands, and the first word may not hold one at all, which sh reads as a
 * variable assignment.
 */
internal fun quotePosix(argument: String, first: Boolean = false): String {
    if (POSIX_BARE.matches(argument) && !(first && '=' in argument)) return argument
    return "'" + argument.replace("'", "'\\''") + "'"
}

/** A sentence for a refusal code, for the person who has to fix the configuration. */
internal fun describeRefusal(code: String, argument: String?): String {
    val what = argument?.let { "\"$it\"" } ?: "The command"
    return when (code) {
        "no-command" -> "There is no command to run: the system's agent command is empty."
        "empty-argument" -> "The command has an empty argument, which this shell cannot be sent."
        "control-character" -> "An argument contains a control character, such as a line break."
        "posix-backslash" ->
            "$what has a backslash before another backslash, a quote or its end, which fish reads " +
                "as an escape even inside quotes."
        "double-quote" -> "$what contains a double quote, which this shell cannot be sent."
        "percent" -> "$what contains '%', which cmd.exe expands even inside quotes."
        "exclamation" -> "$what contains '!', which cmd.exe can expand even inside quotes."
        "trailing-backslash" ->
            "$what ends in a backslash, which PowerShell passes on wrongly to the program it runs."
        "double-space" -> "$what has two spaces in a row, which PowerShell passes on as one."
        "stop-parsing" -> "$what is --%, which PowerShell reads as an instruction to stop quoting."
        "too-many-arguments" -> "The command has more arguments than the restricted dispatcher takes."
        "argument-too-long" -> "$what is longer than the restricted dispatcher takes."
        "line-too-long" -> "The command line is longer than the restricted dispatcher takes."
        UNKNOWN_SHELL -> "The system's shell is not one of posix, cmd or powershell."
        RESTRICTED_AGENT_SHAPE ->
            "A restricted key runs the agent's dispatcher, which needs the agent command to be " +
                "exactly an interpreter and an entry point."
        else -> "The command cannot be written for this system's shell ($code)."
    }
}

/** What a POSIX shell and the dispatcher both read as one literal word. */
private val POSIX_BARE = Regex("^[A-Za-z0-9._/:@+,-][A-Za-z0-9._/:@+,=-]*$")

/**
 * The backslash sequences fish reads as escapes inside single quotes.
 *
 * The end is `\z`, not `$`: this is a search rather than a whole match, and java.util.regex lets `$`
 * match before a final U+0085, U+2028 or U+2029 too, none of which is a control character here. A
 * backslash before one of those is written by the reference and has to be written here as well.
 */
private val POSIX_BACKSLASH = Regex("\\\\(?:\\\\|'|\\z)")

/**
 * PowerShell's tokenizer reads U+2018 to U+201B as single quotes too. Doubling only U+0027 left a
 * typographic quote free to end the string early.
 */
private val POWERSHELL_QUOTE = Regex("['\u2018\u2019\u201a\u201b]")

private const val DISPATCH_MAX_ARGUMENTS = 64
private const val DISPATCH_MAX_ARGUMENT_BYTES = 4096
private const val DISPATCH_MAX_LINE_BYTES = 16384

/** U+0000 to U+001F and U+007F, and nothing that Kotlin's wider idea of a control character adds. */
private fun Char.isControlCharacter(): Boolean = this <= '\u001f' || this == '\u007f'

private fun String.utf8Size(): Int = toByteArray(Charsets.UTF_8).size
