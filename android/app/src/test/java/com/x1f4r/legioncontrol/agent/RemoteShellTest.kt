package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.ControllerConfig
import com.x1f4r.legioncontrol.data.EndpointConfig
import com.x1f4r.legioncontrol.data.MachineConfig
import com.x1f4r.legioncontrol.data.SystemConfig
import com.x1f4r.legioncontrol.ui.withWindowsShell
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

/**
 * Writing an argv as a command line for the reader on the far side.
 *
 * ssh takes one string and hands it to a login shell, or to the agent's dispatcher behind a
 * restricted key, so an argv is not what travels: whatever this produces is re-split by sh, cmd.exe,
 * PowerShell or the dispatcher, each by its own rules. The shared vectors pin every byte; these tests
 * say why the rules are what they are, and read each line back the way its reader would.
 */
class RemoteShellTest {

    @Test
    fun `a restricted system is written for the dispatcher whatever its shell says`() {
        // The C4 case: a Windows system behind a restricted key. The previous release wrote its path
        // bare, the dispatcher refuses an unquoted backslash, and the machine could not be driven.
        val machine = ControllerConfig(
            machines = listOf(
                MachineConfig(
                    id = "tower",
                    endpoints = listOf(EndpointConfig(id = "lan", kind = "lan", host = "192.0.2.10", user = "me")),
                    systems = listOf(
                        SystemConfig(
                            id = "windows",
                            platform = "windows",
                            shell = "cmd",
                            restricted = true,
                            agent = listOf(
                                "C:\\Program Files\\nodejs\\node.exe",
                                "C:\\Users\\me\\.legion-control\\bin\\launcher.mjs",
                            ),
                        ),
                    ),
                ),
            ),
        ).toMachines().single()
        val system = machine.systems.single()
        assertEquals(RemoteShell.DISPATCH, system.grammar)

        val line = buildRemoteCommand(system.grammar!!, system.agent + "status")
        assertEquals(
            "'C:\\Program Files\\nodejs\\node.exe' 'C:\\Users\\me\\.legion-control\\bin\\launcher.mjs' status",
            line,
        )
        assertEquals(system.agent + "status", readDispatch(line))
    }

    @Test
    fun `a Windows system with no shell is written for cmd, and PowerShell is never assumed`() {
        assertEquals(GrammarChoice.of(RemoteShell.CMD), grammarFor("windows", null, null, false, 2))
        assertEquals(GrammarChoice.of(RemoteShell.POSIX), grammarFor("linux", null, null, false, 2))
        assertEquals(GrammarChoice.of(RemoteShell.POSIX), grammarFor(null, null, null, false, 2))
        // The platform is compared as written. A capital letter is a different platform, and a
        // forgiving reading here would quote differently from every other client.
        assertEquals(GrammarChoice.of(RemoteShell.POSIX), grammarFor("Windows", null, null, false, 2))
        assertEquals(GrammarChoice.of(RemoteShell.POWERSHELL), grammarFor("windows", "powershell", false, false, 2))
    }

    @Test
    fun `the editor writes cmd for a Windows system that does not say, and nothing else`() {
        fun system(vararg pairs: Pair<String, String>) = JsonObject(pairs.associate { (k, v) -> k to JsonPrimitive(v) })
        assertEquals(
            system("id" to "windows", "platform" to "windows", "shell" to "cmd"),
            system("id" to "windows", "platform" to "windows").withWindowsShell(),
        )
        val powershell = system("id" to "windows", "platform" to "windows", "shell" to "powershell")
        assertEquals(powershell, powershell.withWindowsShell())
        val linux = system("id" to "linux", "platform" to "linux")
        assertEquals(linux, linux.withWindowsShell())
    }

    @Test
    fun `a shell this app does not write for is a refusal rather than the nearest guess`() {
        listOf("pwsh", "POSIX", "auto", "fish", "").forEach { shell ->
            assertEquals(shell, GrammarChoice.refused(UNKNOWN_SHELL), grammarFor("linux", shell, null, false, 2))
            assertNull(shell, RemoteShell.fromWire(shell))
        }
        assertNull("dispatch is a grammar, not a shell a document may name", RemoteShell.fromWire("dispatch"))
    }

    @Test
    fun `a restricted session only ever moves a system to the dispatcher`() {
        assertEquals(GrammarChoice.of(RemoteShell.DISPATCH), grammarFor("windows", "powershell", false, true, 2))
        assertEquals(GrammarChoice.refused(RESTRICTED_AGENT_SHAPE), grammarFor("windows", "cmd", null, true, 3))
        assertEquals(GrammarChoice.refused(RESTRICTED_AGENT_SHAPE), grammarFor("linux", null, true, false, 1))
    }

    @Test
    fun `a tilde is quoted, because nothing expands an argv element`() {
        assertEquals(
            "node '~/.legion-control/agent/src/index.mjs' status",
            buildRemoteCommand(RemoteShell.POSIX, listOf("node", "~/.legion-control/agent/src/index.mjs", "status")),
        )
    }

    @Test
    fun `nothing a document can contain becomes a second posix command`() {
        val hostile = listOf(
            "; rm -rf /",
            "\$(touch /tmp/pwned)",
            "`id`",
            "a && b",
            "a | b",
            "'; id; '",
            "~root",
            "=cmd",
            "\u2018quoted\u2019",
        )
        for (grammar in listOf(RemoteShell.POSIX, RemoteShell.DISPATCH)) {
            hostile.forEach { argument ->
                val argv = listOf("/bin/echo", argument)
                val line = buildRemoteCommand(grammar, argv)
                assertEquals("$grammar $argument", argv, readDispatch(line))
            }
        }
    }

    @Test
    fun `cmd lines start with an at sign and keep a trailing backslash`() {
        // cmd /c strips the outer pair of quotes from a line that starts with one, whether or not
        // sshd added them. A line that starts with @ never starts with a quote, so it reads the same
        // either way (A12).
        val argv = listOf("C:\\Program Files (x86)\\nodejs\\node.exe", "C:\\stage dir\\", "a&b|c<d>e^f", "status")
        val line = buildRemoteCommand(RemoteShell.CMD, argv)
        assertTrue(line, line.startsWith("@\""))
        assertEquals(argv, readCmd(line))
        assertFalse("carets inside double quotes are literal, so none are added", line.contains("^&"))
    }

    @Test
    fun `cmd refuses what it would expand even inside quotes`() {
        assertRefused("percent", 1) { buildRemoteCommand(RemoteShell.CMD, listOf("node", "C:\\Users\\%USERNAME%\\x.mjs")) }
        assertRefused("exclamation", 1) { buildRemoteCommand(RemoteShell.CMD, listOf("node", "wow!")) }
        assertRefused("double-quote", 1) { buildRemoteCommand(RemoteShell.CMD, listOf("node", "say \"hi\"")) }
    }

    @Test
    fun `powershell doubles every character its tokenizer reads as a single quote`() {
        // T11 and A12: U+2018 to U+201B end a single-quoted string in PowerShell exactly as ' does.
        // Doubling only the ASCII one left a typographic quote free to end the string early.
        val argv = listOf("node", "it's", "\u2018a\u2019\u201ab\u201b", "\$env:USERPROFILE", "--%")
        val line = buildRemoteCommand(RemoteShell.POWERSHELL, argv)
        assertEquals("& 'node' 'it''s' '\u2018\u2018a\u2019\u2019\u201a\u201ab\u201b\u201b' '\$env:USERPROFILE' '--%'", line)
        assertEquals(argv, readPowerShell(line))
    }

    @Test
    fun `every refusal names the element that failed`() {
        assertRefused("empty-argument", 1) { buildRemoteCommand(RemoteShell.POSIX, listOf("node", "")) }
        assertRefused("control-character", 1) { buildRemoteCommand(RemoteShell.CMD, listOf("node", "a\nb")) }
        assertRefused("posix-backslash", 1) { buildRemoteCommand(RemoteShell.POSIX, listOf("node", "a\\")) }
        assertRefused("trailing-backslash", 1) { buildRemoteCommand(RemoteShell.POWERSHELL, listOf("node", "C:\\dir\\")) }
        assertRefused("no-command", null) { buildRemoteCommand(RemoteShell.POSIX, emptyList()) }
        assertRefused("too-many-arguments", null) {
            buildRemoteCommand(RemoteShell.DISPATCH, List(65) { "a$it" })
        }
        // The dispatcher has no fish to worry about, so a trailing backslash is fine there.
        assertEquals("node 'a\\'", buildRemoteCommand(RemoteShell.DISPATCH, listOf("node", "a\\")))
    }

    @Test
    fun `a backslash before a final line separator is written, as the reference writes it`() {
        // java.util.regex lets `$` match before a final U+0085, U+2028 or U+2029, which JavaScript
        // does not. None of the three is a control character, so the backslash is not before the
        // end of the element and the reference quotes it rather than refusing it.
        listOf('\u0085', '\u2028', '\u2029').forEach { separator ->
            val argument = "a\\$separator"
            assertEquals("node '$argument'", buildRemoteCommand(RemoteShell.POSIX, listOf("node", argument)))
        }
        assertRefused("posix-backslash", 1) { buildRemoteCommand(RemoteShell.POSIX, listOf("node", "a\u2028\\")) }
    }

    private fun assertRefused(code: String, index: Int?, block: () -> Unit) {
        try {
            block()
            fail("expected a $code refusal")
        } catch (refusal: CommandLineRefusal) {
            assertEquals(code, refusal.code)
            assertEquals(index, refusal.index)
            assertTrue(refusal.message!!.isNotBlank())
        }
    }

    /**
     * Reads a POSIX or dispatch line back the way the dispatcher does: runs of U+0020 separate
     * tokens, a single-quoted run is literal, `\'` outside quotes is one quote, and nothing else
     * outside quotes may be special.
     */
    private fun readDispatch(line: String): List<String> {
        val words = mutableListOf<String>()
        var index = 0
        while (index < line.length) {
            val word = StringBuilder()
            while (index < line.length && line[index] != ' ') {
                val character = line[index]
                when {
                    character == '\'' -> {
                        val close = line.indexOf('\'', index + 1)
                        check(close > index) { "a quoted run is never closed in $line" }
                        word.append(line, index + 1, close)
                        index = close + 1
                    }

                    character == '\\' && line.getOrNull(index + 1) == '\'' -> {
                        word.append('\'')
                        index += 2
                    }

                    else -> {
                        check(character !in "\\\$`;&|<>(){}*?!#\"~") { "unquoted $character in $line" }
                        word.append(character)
                        index += 1
                    }
                }
            }
            words += word.toString()
            index += 1
        }
        return words
    }

    /** Reads an `@` cmd line back by the C runtime rule, after cmd has dropped the `@`. */
    private fun readCmd(line: String): List<String> {
        check(line.startsWith("@"))
        val body = line.substring(1)
        val words = mutableListOf<String>()
        var index = 0
        while (index < body.length) {
            check(body[index] == '"') { "every element of a cmd line is double-quoted: $line" }
            index += 1
            val word = StringBuilder()
            while (true) {
                var slashes = 0
                while (body[index] == '\\') {
                    slashes += 1
                    index += 1
                }
                if (body[index] == '"') {
                    check(slashes % 2 == 0) { "an escaped quote in $line" }
                    word.append("\\".repeat(slashes / 2))
                    index += 1
                    break
                }
                word.append("\\".repeat(slashes)).append(body[index])
                index += 1
            }
            words += word.toString()
            if (index < body.length) {
                check(body[index] == ' ')
                index += 1
            }
        }
        return words
    }

    /** Reads a PowerShell line back, with all five single-quote characters treated as quotes. */
    private fun readPowerShell(line: String): List<String> {
        val quotes = "'\u2018\u2019\u201a\u201b"
        check(line.startsWith("& "))
        val body = line.substring(2)
        val words = mutableListOf<String>()
        var index = 0
        while (index < body.length) {
            check(body[index] in quotes)
            index += 1
            val word = StringBuilder()
            while (true) {
                val character = body[index]
                if (character in quotes) {
                    if (body.getOrNull(index + 1)?.let { it in quotes } == true) {
                        word.append(body[index + 1])
                        index += 2
                        continue
                    }
                    index += 1
                    break
                }
                word.append(character)
                index += 1
            }
            words += word.toString()
            if (index < body.length) {
                check(body[index] == ' ')
                index += 1
            }
        }
        return words
    }
}
