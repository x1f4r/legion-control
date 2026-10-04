package com.x1f4r.legioncontrol.agent

/**
 * Putting a signed 3.x agent on a machine whose 2.x agent has no way to install one itself.
 *
 * A port of the Mac's `AgentBootstrap.swift`, and gated the same way (SPEC S1.11): only over an
 * authenticated status from an actual 1.x or 2.x agent, only with a key that is not restricted, and
 * only when the configured command shows where the agent is installed. The base is never guessed:
 * a command this file does not recognise means nothing is offered, because the bootstrap runs code
 * the far side has only just received, and it has to run it in the right place.
 *
 * Three steps, each with the agent's own interpreter: [PREPARE] checks the base holds an installed
 * agent and makes `incoming/`; the archive goes over SFTP to [archivePath]; [INSTALL] checks the
 * upload's size and SHA-256 against the ones this phone sends, and only then unpacks it and runs the
 * new tree's own `self-update --install`. The two scripts are byte for byte the Mac's, and both are
 * compared with `contract/bootstrap/` in tests.
 */
object AgentBootstrap {

    /**
     * Whether [status] is an authenticated answer from an agent that predates contract 3.
     *
     * Nothing short of that permits the bootstrap. A status that was not read, that said `ok:false`,
     * or that does not name a 1.x or 2.x version is not evidence that the machine needs one, and a
     * missing agent is never a reason to put one there from a phone.
     */
    fun isAuthenticatedLegacy(status: AgentStatus?): Boolean {
        if (status?.ok != true) return false
        if ((status.contract ?: status.agent?.contract ?: LEGACY_CONTRACT) >= Contract.REQUIRED) return false
        val major = status.version?.split('.')?.firstOrNull()?.toIntOrNull() ?: return false
        return major == 1 || major == 2
    }

    /** Where an agent is installed, as its configured command shows it. */
    data class Layout(
        /** The installation base: the directory that holds `agent/` and `bin/`. */
        val base: String,
        /** The configured argv without its script: the interpreter, and anything given to it. */
        val prefix: List<String>,
    ) {
        private val separator: String get() = if ('\\' in base) "\\" else "/"

        /** [suffix], written with `/`, under the base in the base's own separator. */
        fun path(suffix: String): String = base + separator + suffix.replace("/", separator)
    }

    /**
     * The layout [argv] was installed with, or null when it cannot be told from the command.
     *
     * Only the two shapes an installer writes are recognised: `<base>/agent/src/index.mjs` and the
     * stable launcher `<base>/bin/launcher.mjs`, after the interpreter. The base has to be absolute.
     */
    fun layout(argv: List<String>): Layout? {
        if (argv.size < 2) return null
        val script = argv.last()
        val normalized = script.replace('\\', '/')
        val suffix = SCRIPT_SUFFIXES.firstOrNull { normalized.endsWith(it) } ?: return null
        val base = script.dropLast(suffix.length)
        if (!isAbsolute(base)) return null
        return Layout(base, argv.dropLast(1))
    }

    /** An absolute path with no `..` in it and no trailing separator: `/…` or `X:/…`, either slash. */
    fun isAbsolute(path: String): Boolean {
        if (path.isEmpty() || path.any { it == '\u0000' || it == '\n' || it == '\r' }) return false
        val normalized = path.replace('\\', '/')
        if (".." in normalized.split('/') || normalized.endsWith("/")) return false
        if (normalized.startsWith("/")) return true
        return normalized.length >= 3 && normalized[0].isAsciiLetter() && normalized[1] == ':' && normalized[2] == '/'
    }

    /** Where the archive is uploaded: inside the base, named for the operation it belongs to. */
    fun archivePath(layout: Layout, operationId: String): String =
        layout.path("incoming/legion-bootstrap-$operationId.tgz").replace('\\', '/')

    /** `node -e PREPARE -- <base>`. */
    fun prepareArgv(layout: Layout): List<String> =
        layout.prefix + listOf("-e", onCommandLine(PREPARE), "--", layout.base)

    /** `node -e INSTALL -- <base> <archive> <sha256> <size> <op>`. */
    fun installArgv(
        layout: Layout,
        archive: String,
        sha256: String,
        size: Long,
        operationId: String,
    ): List<String> = layout.prefix +
        listOf("-e", onCommandLine(INSTALL), "--", layout.base, archive, sha256, size.toString(), operationId)

    /**
     * A script as it travels on a command line: its lines joined with a space.
     *
     * Section 15.2 refuses a control character in every grammar, a line break included, and that
     * rule has no exception for this. Joining changes nothing node runs: every line of both scripts
     * is a complete statement ending in `;`, with no comment and no template literal, which the
     * tests check against the shared files themselves.
     */
    fun onCommandLine(script: String): String = script.replace('\n', ' ')

    /** Refuses an old node and a base with no agent in it, then makes `incoming/`. Prints `{"ok":true}`. */
    val PREPARE: String = """
        const fs=require('node:fs'),path=require('node:path');
        if(Number(process.versions.node.split('.')[0])<24) throw Error('Node 24 or newer is required before this upgrade');
        const base=process.argv[1], incoming=path.join(base,'incoming');
        if(!fs.existsSync(path.join(base,'agent','src','index.mjs'))) throw Error('No installed agent exists in this base; bootstrap refused');
        fs.mkdirSync(incoming,{recursive:true});
        if(fs.existsSync(path.join(incoming,'agent'))) throw Error('An incoming agent tree already exists. Inspect and recover it before trying another installation');
        console.log(JSON.stringify({ok:true}));
    """.trimIndent()

    /**
     * Checks the upload's size and SHA-256 before tar reads a byte of it, unpacks it into `incoming/`,
     * and runs the new tree's own install with the base as its home. Its reply is the new agent's.
     */
    val INSTALL: String = """
        const fs=require('node:fs'),path=require('node:path'),crypto=require('node:crypto'),cp=require('node:child_process');
        const [base,archive,hash,size,op]=process.argv.slice(1), incoming=path.join(base,'incoming');
        const stat=fs.statSync(archive); if(stat.size!==Number(size)) throw Error('Uploaded archive size does not match');
        const bytes=fs.readFileSync(archive); if(crypto.createHash('sha256').update(bytes).digest('hex')!==hash) throw Error('Uploaded archive hash does not match');
        if(fs.existsSync(path.join(incoming,'agent'))) throw Error('An incoming agent tree already exists; nothing was replaced');
        const unpack=cp.spawnSync('tar',['-xzf',archive,'-C',incoming],{encoding:'utf8',timeout:30000});
        if(unpack.status!==0) throw Error(unpack.stderr||'Archive extraction failed');
        const installed=cp.spawnSync(process.execPath,[path.join(incoming,'agent','src','index.mjs'),'self-update','--install','--op',op],{env:{...process.env,LEGIONCTL_HOME:base},stdio:'inherit',timeout:240000});
        process.exit(installed.status===null?1:installed.status);
    """.trimIndent()

    /** What a status without a contract number is: the Mac reads it the same way. */
    private const val LEGACY_CONTRACT = 2

    private val SCRIPT_SUFFIXES = listOf("/agent/src/index.mjs", "/bin/launcher.mjs")

    private fun Char.isAsciiLetter(): Boolean = this in 'A'..'Z' || this in 'a'..'z'
}
