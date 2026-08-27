package com.x1f4r.legioncontrol.ui

import android.content.Context
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.x1f4r.legioncontrol.BuildConfig
import com.x1f4r.legioncontrol.agent.MachineSystem
import com.x1f4r.legioncontrol.net.AppUpdates
import java.io.File

/**
 * The phone's own business: the app's version and how it updates itself, the ssh key that gets this
 * phone through the front door, and the configuration that decides which doors there are.
 *
 * Nothing on this page talks to a machine except the one thing that has to: fetching the setup off
 * one, which is how the configuration gets here in the first place. It used to be the tail of one
 * long scroll, under everything anyone actually opens the app to see.
 */
@Composable
fun ThisDeviceSection(app: AppModel, updates: AppUpdateModel) {
    Spacer(Modifier.height(6.dp))
    AppUpdateBlock(updates)
    DeviceKeyBlock(app)
    ConfigurationBlock(app)
}

// MARK: - The app

@Composable
private fun AppUpdateBlock(updates: AppUpdateModel) {
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
        if (downloaded != null) {
            PlainAction(
                label = "Install ${available?.release?.version ?: "the download"}",
                enabled = true,
                emphasis = true,
            ) { startInstall(context, downloaded, updates::reportInstallProblem) }

            PlainAction(label = "Discard", enabled = true) { updates.discardDownload() }
        } else {
            // Dead unless a newer release was actually found. Not merely "the check has not failed":
            // an unreadable release list and an up to date app are both reasons there is nothing to
            // press, and neither of them should look like there is.
            PlainAction(
                label = if (available != null) "Download ${available.release.version}" else "Update the app",
                enabled = available != null,
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
    is AppUpdates.Check.Available -> if (updates.downloaded != null) "Downloaded and ready to install." else null
    is AppUpdates.Check.UpToDate ->
        if (check.noReleaseYet) {
            "That repository has published no release yet, so there is nothing to install."
        } else {
            "This is the newest release."
        }

    is AppUpdates.Check.Failed -> "The release list could not be read: ${check.reason}"
}

// MARK: - The key

@Composable
private fun DeviceKeyBlock(app: AppModel) {
    // Once the phone is authorised this has done its job, so it collapses to one line. It stays
    // reachable rather than disappearing, because the key is needed again whenever a system is
    // rebuilt or the phone has to be cut off and let back in. Leaving the whole block open forever
    // is just a wall of base64 under everything you actually came here to look at.
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
        Actions {
            PlainAction("Hide the key", enabled = true) { showKey = false }
        }
    } else {
        // What this page may claim is only what it has seen. A system that is answering has accepted
        // the key, and one that is not running has said nothing either way: it is checked the first
        // time this phone reaches it, and until then a line saying every system is done would be a
        // guess the user only finds out is wrong after a boot switch.
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

// MARK: - The configuration

/**
 * The document that decides what this app knows.
 *
 * A text field and not a form. The same JSON is read by the Mac from a file, and one document that
 * can be moved between the two, pasted whole, is worth more than a screen of pickers that can only
 * ever describe part of it. What this side owes the user is the check: Apply says in one sentence
 * why nothing changed, rather than storing something that quietly does not work.
 */
@Composable
private fun ConfigurationBlock(app: AppModel) {
    SectionHeading(
        "Configuration",
        note = if (app.hasMachines) {
            "${app.machines.size} machine${if (app.machines.size == 1) "" else "s"}"
        } else {
            "nothing configured"
        },
    )
    ExplanationText(
        "The same document the Mac keeps in ~/.config/legion-control/config.json: which machines " +
            "exist, how to reach them, how to wake them, and which systems each one can boot into. " +
            "Every machine carries a copy of it, so one address is enough to get all of it.",
    )
    Spacer(Modifier.height(14.dp))
    FetchSetupBlock(app)
    Spacer(Modifier.height(20.dp))
    SectionHeading("Or paste it", showsRule = false)
    ExplanationText(
        "A document typed in here is never quietly replaced by an older one: what a machine offers " +
            "is only taken when it is a different document, and only when it holds up.",
    )
    Spacer(Modifier.height(10.dp))
    DocumentField(
        value = app.configDraft,
        onValueChange = { app.configDraft = it },
        placeholder = "Paste the configuration here",
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
    app.configNote?.takeIf { app.configError == null }?.let { QuietText(it, Modifier.padding(top = 2.dp)) }
}

/**
 * Three fields and a button, which is the whole of setting this app up.
 *
 * It is the first thing on the Machines page when there is nothing configured, and it sits above
 * the document field here, because it is the way in: the Mac writes the setup once and pushes it to
 * the machines, and the phone only has to know where one machine is. The key is the phone's own and
 * has to be authorised on that system already, exactly as for every other command this app runs.
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
