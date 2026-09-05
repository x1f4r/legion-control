package com.x1f4r.legioncontrol.ui

import android.content.Context
import android.content.Intent
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.x1f4r.legioncontrol.BuildConfig
import com.x1f4r.legioncontrol.agent.MachineSystem
import com.x1f4r.legioncontrol.data.DeviceBindings
import com.x1f4r.legioncontrol.net.AppUpdates
import java.io.File

/**
 * The phone's own business: the app's version and how it updates itself, the ssh key that gets this
 * phone through the front door, what this device is called, and the shared setup it edits like every
 * other device does.
 *
 * The one thing this page is careful to say plainly is what a phone cannot do. It cannot run a
 * service and it cannot be a machine in the setup, and that is a fact about the platform rather than
 * a lesser standing: it edits, publishes and controls exactly like the Mac and the desktop.
 */
@Composable
fun ThisDeviceSection(app: AppModel, updates: AppUpdateModel, bindings: DeviceBindings, onBindings: BindingsActions) {
    var selected by remember { mutableStateOf<String?>(null) }
    Text("Settings", style = MaterialTheme.typography.titleLarge)
    SettingsEntry("App updates", if (updates.newerAppAvailable) "Available" else BuildConfig.VERSION_NAME) { selected = "App updates" }
    SettingsEntry("This device") { selected = "This device" }
    SettingsEntry("SSH key") { selected = "SSH key" }
    SettingsEntry("Notifications") { selected = "Notifications" }
    SettingsEntry("Shared setup") { selected = "Shared setup" }
    SettingsEntry("Revision history") { selected = "Revision history" }
    selected?.let { title -> DetailSheet(title, { selected = null }) {
        when (title) {
            "App updates" -> AppUpdateBlock(updates)
            "This device" -> ThisDeviceBlock(app, bindings, onBindings)
            "SSH key" -> DeviceKeyBlock(app)
            "Notifications" -> NotificationsBlock(app)
            "Shared setup" -> SetupBlock(app) { selected = null; app.openEditor() }
            "Revision history" -> HistoryBlock(app)
        }
    } }
}

/** The two things a device can be told about itself, neither of which is ever published. */
interface BindingsActions {
    fun setDeviceName(name: String)
    fun setCurrentSite(siteId: String?)
}

// MARK: - The app

@Composable
internal fun AppUpdateBlock(updates: AppUpdateModel) {
    val context = LocalContext.current
    val available = updates.check as? AppUpdates.Check.Available

    SectionHeading("This app", showsRule = false)
    DetailRow("Installed") { VersionText(BuildConfig.VERSION_NAME, "unknown") }
    DetailRow("Newest release") { VersionText(available?.release?.version, "not known") }

    if (available != null && available.release.notes.isNotBlank()) {
        Spacer(Modifier.height(4.dp))
        ExplanationText(available.release.notes)
    }

    val downloaded = updates.downloaded
    Actions {
        PlainAction("Check for updates", enabled = !updates.checking && !updates.downloading, working = updates.checking) { updates.recheck() }
        if (downloaded != null) {
            PlainAction(
                label = "Install ${updates.downloadedVersion ?: "the download"}",
                enabled = true,
                emphasis = true,
            ) { updates.install { file -> startInstall(context, file, updates::reportInstallProblem) } }

            PlainAction(label = "Discard", enabled = true) { updates.discardDownload() }
        } else {
            PlainAction(
                label = if (available != null) "Download ${available.release.version}" else "Update the app",
                enabled = available != null && !updates.checking && !updates.downloading,
                emphasis = available != null,
                working = updates.downloading,
            ) { updates.download() }
        }
    }

    appUpdateReason(updates)?.let { QuietText(it, Modifier.padding(top = 2.dp)) }

    if (updates.downloading) {
        Spacer(Modifier.height(8.dp))
        Column(Modifier.fillMaxWidth()) {
            QuietText(
                if (updates.progress >= 0) "Downloading, ${updates.progress} percent." else "Downloading.",
            )
            Spacer(Modifier.height(8.dp))
            if (updates.progress >= 0) {
                LinearProgressIndicator(
                    progress = { updates.progress / 100f },
                    modifier = Modifier.fillMaxWidth(),
                )
            } else {
                LinearProgressIndicator(modifier = Modifier.fillMaxWidth())
            }
        }
    }

    updates.note?.let { QuietText(it, Modifier.padding(top = 6.dp)) }
    updates.failure?.let { QuietText(it, Modifier.padding(top = 6.dp)) }
}

/** Why there is nothing to press, in one line. Null when there is. */
private fun appUpdateReason(updates: AppUpdateModel): String? = when (val check = updates.check) {
    null -> if (updates.checking) "Looking for a newer release." else "The release list has not been read yet."
    is AppUpdates.Check.Available ->
        if (updates.downloaded != null) {
            "Downloaded, and checked against the signed release manifest and this app's own " +
                "signing key. Ready to install."
        } else {
            "The release manifest was signed by the key this app trusts."
        }

    is AppUpdates.Check.UpToDate ->
        if (check.noReleaseYet) {
            "That repository has published no release yet, so there is nothing to install."
        } else {
            "This is the newest release."
        }

    is AppUpdates.Check.NotForThisBuild -> check.reason
    is AppUpdates.Check.Failed -> "The release could not be verified: ${check.reason}"
}

// MARK: - This device

/**
 * What this device is called and where it is standing.
 *
 * Both are private to this phone and neither is published, except the name, which travels inside a
 * revision so that a divergence screen on another device can say who made the edit.
 */
@Composable
private fun ThisDeviceBlock(app: AppModel, bindings: DeviceBindings, actions: BindingsActions) {
    SectionHeading("This device")
    ExplanationText(
        "A phone cannot run a service, so it is never one of the machines in the setup. It edits " +
            "the setup, publishes it and controls every machine exactly like the other devices do.",
    )
    Spacer(Modifier.height(6.dp))

    DetailRow("Called") {
        PlainField(
            value = bindings.deviceName,
            onValueChange = actions::setDeviceName,
            placeholder = "this phone",
        )
    }
    QuietText(
        "Goes with any setup change made here, so other devices can see who made it.",
        Modifier.padding(top = 2.dp),
    )

    val sites = app.sites
    if (sites.isNotEmpty()) {
        DetailRow("Standing at", alignment = Alignment.Top) {
            Column {
                QuietText(app.siteMatch.explain())
                Spacer(Modifier.height(4.dp))
                Actions {
                    PlainAction(
                        label = "Work it out",
                        enabled = bindings.currentSite != null,
                    ) { actions.setCurrentSite(null) }
                    sites.forEach { site ->
                        PlainAction(
                            label = site.displayName,
                            enabled = bindings.currentSite != site.id,
                            emphasis = bindings.currentSite == site.id,
                        ) { actions.setCurrentSite(site.id) }
                    }
                }
            }
        }
        QuietText(
            "Two networks can use the same private addresses, so an address is a hint and never a " +
                "proof. Saying which site this is only changes where a wake packet is sent from; " +
                "what proves a machine is the machine is its pinned host key.",
            Modifier.padding(top = 2.dp),
        )
    }
}

// MARK: - Notifications

@Composable
private fun NotificationsBlock(app: AppModel) {
    val context = LocalContext.current
    var enabled by remember { mutableStateOf(app.notificationsEnabled) }
    val request = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted ->
        enabled = granted
        app.notificationsEnabled = granted
    }

    SectionHeading("Telling you when it is done")
    ExplanationText(
        "An update, a restart or an action you asked for can take minutes. With this on, the app " +
            "says how it ended even if you have put the phone away. Nothing else is ever announced.",
    )
    Row(
        Modifier.fillMaxWidth().defaultMinSize(minHeight = 60.dp).padding(vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text("Finished changes", style = MaterialTheme.typography.bodyLarge)
            QuietText(
                if (enabled) "On." else "Off. Turning it on asks Android for permission once.",
                Modifier.padding(top = 2.dp),
            )
        }
        Spacer(Modifier.width(12.dp))
        Switch(
            checked = enabled,
            onCheckedChange = { wanted ->
                if (!wanted) {
                    enabled = false
                    app.notificationsEnabled = false
                } else {
                    request.launch(android.Manifest.permission.POST_NOTIFICATIONS)
                }
            },
        )
    }
}

// MARK: - The key

@Composable
private fun DeviceKeyBlock(app: AppModel) {
    var showKey by rememberSaveable { mutableStateOf(false) }
    val systems = app.machines.flatMap { it.machine.systems }

    SectionHeading("This phone's key")

    if (showKey) {
        ExplanationText(
            "This phone has a key of its own. It has to be in authorized_keys on " +
                "${systemsSentence(systems)}, and taking that one line back out is all it takes to " +
                "cut the phone off without touching anything else.",
        )
        Spacer(Modifier.height(10.dp))
        AuthorizedKeysPaths()
        Spacer(Modifier.height(14.dp))
        PublicKeyBlock(app.publicKey)
        app.keyFingerprint.takeIf { it.isNotBlank() }?.let {
            Spacer(Modifier.height(6.dp))
            CommandText(it)
        }
        Actions {
            PlainAction("Hide the key", enabled = true) { showKey = false }
        }
    } else {
        val answering = app.machines.mapNotNull { model ->
            model.currentSystem?.let { "${it.name} on ${model.machine.name}" }
        }
        ExplanationText(
            if (answering.isEmpty()) {
                "Generated on this phone. The private half never leaves it."
            } else {
                "Accepted by ${answering.joinToString(", ")}, which " +
                    (if (answering.size == 1) "is the system" else "are the systems") +
                    " answering now."
            },
        )
        Actions {
            PlainAction("Show this phone's key", enabled = true) { showKey = true }
        }
    }
}

// MARK: - The setup

/**
 * The document that decides what this app knows, and that every device shares.
 *
 * There is no authority any more. This device edits it, publishes it to every machine that will take
 * it, and takes a newer copy when somebody else made one. What keeps that from losing an edit is
 * ancestry rather than a rule about who may write, and the two questions it cannot answer on its own
 * are right here rather than buried.
 */
@Composable
private fun SetupBlock(app: AppModel, onEdit: () -> Unit = app::openEditor) {
    SectionHeading(
        "Setup",
        note = if (app.hasMachines) {
            "${app.machines.size} machine${if (app.machines.size == 1) "" else "s"}"
        } else {
            "nothing configured"
        },
    )

    val provenance = app.setupProvenance
    if (provenance != null) {
        DetailRow("Revision") {
            ValueText(provenance.revision?.let { "revision $it" }, "no revision")
        }
        DetailRow("Setup") { ValueText(provenance.authority, "unnamed") }
        DetailRow("Last written") {
            ValueText(
                listOfNotNull(
                    provenance.describeAuthor().takeIf { it != "somebody" },
                    localTime(provenance.updatedAt),
                ).joinToString(", ").takeIf { it.isNotBlank() },
                "not known",
            )
        }
    }

    ExplanationText(
        "Every device carries the same document and every machine keeps a copy. A change made here " +
            "is sent to each machine on its next check, and a change made elsewhere arrives the " +
            "same way. Neither can quietly undo the other.",
    )

    app.advisories.forEach { QuietText(it, Modifier.padding(top = 4.dp)) }

    Actions {
        PlainAction(
            label = "Edit the setup",
            enabled = true,
            emphasis = app.divergence == null && app.identityClash == null,
        ) { onEdit() }
    }

    Spacer(Modifier.height(14.dp))
    FetchSetupBlock(app)

    Spacer(Modifier.height(20.dp))
    SectionHeading("Or paste one", showsRule = false)
    ExplanationText(
        "A document pasted here becomes this device's setup and is offered to every machine. One " +
            "that names no setup gets a new identity, so it does not quietly take over an existing " +
            "one.",
    )
    Spacer(Modifier.height(10.dp))
    DocumentField(
        value = app.configDraft,
        onValueChange = app::updateDraft,
        placeholder = "Paste a setup here",
    )
    Actions {
        PlainAction(
            label = "Apply",
            enabled = app.configDraft.isNotBlank(),
            emphasis = app.configDraft.isNotBlank(),
        ) { app.applyConfig() }

        PlainAction("Insert example", enabled = true) { app.insertExample() }
    }
    app.configError?.let { QuietText(it, Modifier.padding(top = 2.dp)) }
    app.configNote?.takeIf { app.configError == null }?.let {
        QuietText(it, Modifier.padding(top = 2.dp))
    }
}

/** Every change this app has asked for, and a way to hand the lot to somebody who can read it. */
@Composable
private fun HistoryBlock(app: AppModel) {
    val context = LocalContext.current
    val records = app.operations.records.collectAsStateSafely()
    SectionHeading("Changes made from this phone", note = "${records.size}")
    if (records.isEmpty()) {
        ExplanationText("Nothing has been asked for from this device yet.")
        return
    }
    records.take(8).forEach { record ->
        val mark = when (record.succeeded) {
            true -> Mark.Good
            false -> Mark.Bad
            null -> Mark.Unknown
        }
        Column(Modifier.padding(vertical = 2.dp)) {
            StatusLine(mark, record.summary, style = MaterialTheme.typography.bodySmall)
            QuietText(
                "${record.machineName} · ${localTime(record.startedAt)}",
                Modifier.padding(start = 17.dp, top = 2.dp),
            )
        }
    }
    Actions {
        PlainAction("Export", enabled = true) { shareText(context, app.operations.exportText()) }
        PlainAction("Clear finished", enabled = true) { app.operations.clearHistory() }
    }
}

/**
 * Three fields and a button, which is the whole of setting this app up from nothing.
 *
 * Every machine carries the document, so one address is enough to get all of it. The key is this
 * phone's own and has to be authorised on that system already, exactly as for every other command.
 */
@Composable
fun FetchSetupBlock(app: AppModel) {
    SectionHeading("Fetch from a machine", showsRule = false)
    DetailRow("Address") {
        PlainField(
            value = app.fetchHost,
            onValueChange = { app.fetchHost = it },
            placeholder = "a tailnet or LAN address",
            keyboardType = KeyboardType.Uri,
        )
    }
    DetailRow("Port") {
        PlainField(
            value = app.fetchPort,
            onValueChange = { app.fetchPort = it },
            placeholder = "22",
            keyboardType = KeyboardType.Number,
        )
    }
    DetailRow("User") {
        PlainField(
            value = app.fetchUser,
            onValueChange = { app.fetchUser = it },
            placeholder = "the account to log in as",
        )
    }
    Actions {
        PlainAction(
            label = "Fetch",
            enabled = app.canFetch,
            emphasis = app.canFetch,
            working = app.fetching,
        ) { app.fetchSetup() }
    }
    app.fetchOutcome?.let { QuietText(it, Modifier.padding(top = 2.dp)) }
}

/** The systems by name, or a general word for them when there is no configuration yet. */
internal fun systemsSentence(systems: List<MachineSystem>): String {
    val names = systems.map { it.name }.distinct()
    return when (names.size) {
        0 -> "every system you want to control"
        1 -> names.single()
        else -> names.dropLast(1).joinToString(", ") + " and " + names.last()
    }
}

/**
 * Where that file lives, which depends on the system and, on Windows, on the account.
 *
 * Written out rather than guessed at from the configuration: the app knows a system's name and its
 * platform, and neither of those says whether the account it logs in as is an administrator, which
 * is the whole of the difference on Windows.
 */
@Composable
fun AuthorizedKeysPaths() {
    CommandText("Linux and macOS:  ~/.ssh/authorized_keys")
    CommandText("Windows, administrator:  C:\\ProgramData\\ssh\\administrators_authorized_keys")
    CommandText("Windows, otherwise:  C:\\Users\\<user>\\.ssh\\authorized_keys")
}

/**
 * Android needs one permission granted by hand before any app may hand it an apk, and it is granted
 * per app in settings. Sending the user straight to that screen is the only useful thing to do when
 * it is missing, because the install would otherwise fail silently.
 */
private fun startInstall(context: Context, apk: File, onFailure: (String) -> Unit) {
    if (!AppUpdates.canInstall(context)) {
        onFailure("Allow this app to install apps, then press install again.")
        context.startActivity(AppUpdates.installPermissionIntent(context))
        return
    }
    runCatching { AppUpdates.install(context, apk) }
        .onFailure { onFailure(it.message ?: "the installer could not be opened") }
}

/** Hands text to whatever the user wants to put it in. Nothing leaves the phone on its own. */
internal fun shareText(context: Context, text: String) {
    val intent = Intent(Intent.ACTION_SEND).apply {
        type = "application/json"
        putExtra(Intent.EXTRA_TEXT, text)
        putExtra(Intent.EXTRA_SUBJECT, "Legion Control diagnostics")
    }
    runCatching {
        context.startActivity(
            Intent.createChooser(intent, "Share diagnostics").addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
        )
    }
}
