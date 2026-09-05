package com.x1f4r.legioncontrol.agent

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

/**
 * Writing an argv as a command line for the shell on the far side.
 *
 * ssh takes one string and hands it to a login shell, so an argv is not what travels: whatever this
 * produces is re-split by sh, cmd.exe or PowerShell, each by its own rules. The review found the app
 * joining the argv with spaces, which works exactly until a path has a space in it and then quietly
 * runs the wrong command.
 */
class RemoteShellTest {

    @Test
    fun `stable launcher keeps the configured node executable on posix and windows`() {
        assertEquals(
            listOf("/usr/bin/node", "/srv/legion/bin/launcher.mjs"),
            stableLauncherArgv(listOf("/usr/bin/node", "/srv/legion/agent/src/index.mjs")),
        )
        assertEquals(
            listOf("node.exe", "C:\\Legion Control\\bin\\launcher.mjs"),
            stableLauncherArgv(listOf("node.exe", "C:\\Legion Control\\agent\\src\\index.mjs")),
        )
    }

    private val ordinary = listOf("/usr/bin/node", "/home/me/.legion-control/agent/src/index.mjs", "status")

    @Test
    fun `an argv that needs no quoting is written exactly as it always was`() {
        // The compatibility promise: every configuration that worked before produces the same bytes.
        val expected = "/usr/bin/node /home/me/.legion-control/agent/src/index.mjs status"
        RemoteShell.entries.forEach { shell ->
            assertEquals(shell.name, expected, buildRemoteCommand(shell, ordinary))
        }
    }

    @Test
    fun `a tilde stays unquoted so the shell still expands it`() {
        val argv = listOf("node", "~/.legion-control/agent/src/index.mjs", "status")
        val line = buildRemoteCommand(RemoteShell.POSIX, argv)
        assertEquals("node ~/.legion-control/agent/src/index.mjs status", line)
    }

    @Test
    fun `a path with a space is quoted for posix and survives as one argument`() {
        val argv = listOf("/opt/my node/bin/node", "/home/me/agent.mjs", "status")
        assertEquals(
            "'/opt/my node/bin/node' /home/me/agent.mjs status",
            buildRemoteCommand(RemoteShell.POSIX, argv),
        )
    }

    @Test
    fun `a single quote inside an argument cannot end the quoting`() {
        // The one escape sh has. Getting it wrong is how an apostrophe in a path becomes a second
        // command, so this is the case worth stating rather than trusting.
        val argv = listOf("/bin/sh", "/home/o'brien/agent.mjs")
        val line = buildRemoteCommand(RemoteShell.POSIX, argv)
        assertEquals("/bin/sh '/home/o'\\''brien/agent.mjs'", line)
    }

    @Test
    fun `nothing a document can contain becomes a second posix command`() {
        val hostile = listOf(
            "; rm -rf /",
            "\$(touch /tmp/pwned)",
            "`id`",
            "a && b",
            "a | b",
            "a\nb",
            "'; id; '",
        )
        hostile.forEach { argument ->
            val line = buildRemoteCommand(RemoteShell.POSIX, listOf("/bin/echo", argument))
            // Everything after the first token is one single-quoted string, and the only way out of
            // one is the escape above, which is exactly what is being checked here.
            assertTrue(
                "\"$argument\" should be one quoted argument, got: $line",
                line.startsWith("/bin/echo '") && line.endsWith("'"),
            )
            val body = line.removePrefix("/bin/echo ")
            assertEquals(argument, unquotePosix(body))
        }
    }

    @Test
    fun `windows argv quoting survives trailing backslashes and embedded quotes`() {
        // The classic: a path ending in a backslash would otherwise escape its own closing quote.
        assertEquals("\"C:\\Program Files\\\\\"", quoteWindowsArgv("C:\\Program Files\\"))
        assertEquals("\"a\\\"b\"", quoteWindowsArgv("a\"b"))
        assertEquals("\"a b\"", quoteWindowsArgv("a b"))
    }

    @Test
    fun `cmd refuses a character it would expand even inside quotes`() {
        // %PATH% is expanded by cmd after the argv splitting is done, so quoting cannot hide it.
        // Refusing is honest; a caret in the wrong place would silently change the path.
        try {
            buildRemoteCommand(RemoteShell.CMD, listOf("node", "C:\\Users\\%USERNAME%\\a b\\x.mjs"))
            fail("cmd should refuse an argument containing a percent sign")
        } catch (failure: UnquotableArgument) {
            assertTrue(failure.reason.contains("cmd.exe"))
            assertTrue(failure.reason.contains("powershell"))
        }
    }

    @Test
    fun `powershell gets the call operator, because a quoted first token is a string literal`() {
        val argv = listOf("C:\\Program Files\\nodejs\\node.exe", "C:\\agent\\index.mjs", "status")
        val line = buildRemoteCommand(RemoteShell.POWERSHELL, argv)
        assertTrue("PowerShell needs & or it just prints the path", line.startsWith("& '"))
        assertEquals(
            "& 'C:\\Program Files\\nodejs\\node.exe' 'C:\\agent\\index.mjs' 'status'",
            line,
        )
    }

    @Test
    fun `powershell doubles an embedded single quote`() {
        val line = buildRemoteCommand(RemoteShell.POWERSHELL, listOf("a b", "it's"))
        assertEquals("& 'a b' 'it''s'", line)
    }

    @Test
    fun `a shell that was never named is reported rather than guessed`() {
        // The whole point of AUTO. Guessing wrong here does not fail loudly, it runs the wrong
        // command, so a configuration that has not said is asked rather than assumed.
        try {
            buildRemoteCommand(RemoteShell.AUTO, listOf("/opt/my node/bin/node", "x.mjs"))
            fail("an argument needing quotes with no declared shell should be refused")
        } catch (failure: UnquotableArgument) {
            assertEquals("/opt/my node/bin/node", failure.argument)
            assertTrue(failure.reason.contains("posix"))
        }
    }

    @Test
    fun `a null byte is refused for every shell`() {
        RemoteShell.entries.forEach { shell ->
            try {
                buildRemoteCommand(shell, listOf("/bin/echo", "a\u0000b"))
                fail("$shell should refuse a null byte")
            } catch (_: UnquotableArgument) {
                // Expected.
            }
        }
    }

    /** Reads a single-quoted POSIX word back, so the round trip can be asserted rather than eyeballed. */
    private fun unquotePosix(quoted: String): String {
        require(quoted.startsWith("'") && quoted.endsWith("'"))
        return quoted.substring(1, quoted.length - 1).replace("'\\''", "'")
    }
}
