package com.x1f4r.legioncontrol.ui

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.x1f4r.legioncontrol.ui.theme.LocalStatusColors

/**
 * The machine itself: which system is up, what it is called, and the things that can be done to it.
 * This is the landing page because it answers the question the app is opened to answer, and nothing
 * on it competes with that answer.
 */
@Composable
fun MachineSection(model: MachineModel) {
    Spacer(Modifier.height(6.dp))
    RunningNow(model)

    Spacer(Modifier.height(14.dp))
    DetailRow("Machine") { ValueText(model.status?.hostname, "not known") }
    DetailRow("Control agent") { VersionText(model.status?.agentVersion, "not reachable") }
    DetailRow("Reached") { ValueText(model.route?.label, "no address has answered") }

    // Only after this machine has actually handed the setup over, and only while that is still
    // news. The document arrives on a poll nobody asked for, so the one thing owed here is a line
    // saying where it came from.
    model.setupNote?.let { QuietText(it, Modifier.padding(top = 2.dp)) }

    SectionHeading("Power")
    Actions {
        // Greyed out only while the user's own refresh is running. A background poll no longer
        // disables it, because a button that goes dead for half a second every fifteen seconds is a
        // button you learn not to trust, and pressing during a poll now joins that poll anyway.
        PlainAction(
            label = "Refresh",
            enabled = !model.isWorking,
            working = model.isRefreshingVisibly,
        ) { model.refreshNow() }

        // Only drawn for a machine the configuration says can be woken. A button that could never
        // work is worse than no button, and the configuration is the only thing that knows.
        if (model.canWake) {
            PlainAction(
                label = "Wake",
                enabled = !model.isWorking && !model.isAwake && model.wakeReadiness.possible,
                working = model.isBusyWith(MachineModel.Task.WAKE),
            ) { model.wake() }
        }

        // Not drawn in the destructive colour, unlike the ones below it. Sleep is the one action
        // here that Wake undoes; losing the running system to a reboot is not. It still asks first,
        // because it takes the machine away from whatever it was doing.
        //
        // Keyed off the running system rather than off "awake", because a machine that answered ssh
        // without having the control agent on it cannot be told to do this.
        PlainAction(
            label = "Sleep",
            enabled = !model.isWorking && model.currentSystem != null,
            working = model.isBusyWith(MachineModel.Task.SLEEP),
        ) { model.requestSleep() }

        model.bootChoices.forEach { target ->
            PlainAction(
                label = "Boot into ${target.name}",
                enabled = !model.isWorking && model.currentSystem != null,
                destructive = true,
                working = model.isBusyWith(MachineModel.Task.BOOT, target.id),
            ) { model.requestBoot(target) }
        }

        if (model.hostKeyChanged != null) {
            PlainAction(
                label = "Trust the new host key",
                enabled = !model.isWorking,
                destructive = true,
                working = model.isBusyWith(MachineModel.Task.TRUST_KEY),
            ) { model.requestTrustHostKey() }
        }
    }

    // A disabled Wake needs a reason, and only in the case where you would have reached for it.
    // When the machine is already awake, a greyed out Wake explains itself. The sentence comes from
    // the network layer rather than being written here, because it knows what the phone is on.
    val wake = model.wakeReadiness
    if (model.canWake && !wake.possible && !model.isAwake && wake.explanation.isNotBlank()) {
        QuietText(wake.explanation, Modifier.padding(top = 2.dp))
    } else if (!model.isAwake) {
        QuietText(
            "Sleep and the boot actions need ${model.machine.name} awake.",
            Modifier.padding(top = 2.dp),
        )
    }

    // Whatever else the agent's own configuration decided to offer. No versions, no health, no busy
    // state of their own: a name and a button, and the agent's own sentence before it runs.
    val actions = model.actions
    if (actions.isNotEmpty()) {
        SectionHeading("Actions", note = model.currentSystem?.let { "on ${it.name}" })
        Actions {
            actions.forEach { action ->
                PlainAction(
                    label = action.displayName,
                    enabled = !model.isWorking && model.currentSystem != null,
                    destructive = action.confirm?.isNotBlank() == true,
                    working = model.isBusyWith(MachineModel.Task.RUN, action.id),
                ) { model.requestRun(action) }
            }
        }
    }
}

/**
 * The one thing you open this app to find out, so it carries weight the other rows do not, and it
 * changes with a fade rather than a jump.
 */
@Composable
private fun RunningNow(model: MachineModel) {
    val palette = LocalStatusColors.current
    val reboot = model.rebootInProgress
    val link = model.link

    AnimatedContent(
        targetState = if (reboot != null && !model.isAwake) null else link,
        transitionSpec = { fadeIn(tween(200)) togetherWith fadeOut(tween(140)) },
        label = "running",
    ) { state ->
        Column {
            when {
                state == null ->
                    StatusLine(Mark.Busy, "Restarting into ${reboot?.name ?: "another system"}")

                state is LinkState.Online -> Row(verticalAlignment = Alignment.CenterVertically) {
                    PlatformGlyph(state.system.platform, 22.dp, palette.good)
                    Spacer(Modifier.width(11.dp))
                    Text(state.system.name, style = MaterialTheme.typography.titleLarge)
                }

                state is LinkState.AgentMissing ->
                    StatusLine(
                        Mark.Attention,
                        "${state.system.name}, but the control agent is not installed",
                    )

                state is LinkState.HostKeyChanged ->
                    StatusLine(Mark.Bad, "The host key for ${state.address} is not a trusted one")

                state is LinkState.Offline -> StatusLine(Mark.Attention, "Asleep or unreachable")

                else -> StatusLine(Mark.Unknown, "Checking")
            }
        }
    }
}
