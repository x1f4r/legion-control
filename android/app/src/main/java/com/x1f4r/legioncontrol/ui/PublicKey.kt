package com.x1f4r.legioncontrol.ui

import android.content.ClipData
import android.content.ClipboardManager
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import com.x1f4r.legioncontrol.agent.MachineSystem
import kotlinx.coroutines.delay

/** Public-key authorization details, opened from a device without blocking fleet navigation. */
@Composable
fun AuthorisationScreen(
    publicKey: String,
    systems: List<MachineSystem>,
    detail: String?,
    onRetry: () -> Unit,
) {
    var instructions by remember { mutableStateOf(false) }
    var connectionDetails by remember { mutableStateOf(false) }
    Column(Modifier.fillMaxWidth()) {
        QuietText("Add this device's public key to the target's authorized keys.")
        PublicKeyBlock(publicKey)
        PlainAction("Try again", enabled = true, onClick = onRetry)
        SettingsEntry("Instructions") { instructions = !instructions }
        if (instructions) {
            QuietText("On ${systemsSentence(systems)}, add the key as one complete line to the appropriate file:")
            AuthorizedKeysPaths()
        }
        if (!detail.isNullOrBlank()) {
            SettingsEntry("Connection details") { connectionDetails = !connectionDetails }
            if (connectionDetails) SelectionContainer {
                Text(detail, style = MaterialTheme.typography.bodySmall.copy(fontFamily = FontFamily.Monospace))
            }
        }
    }
}

/** The key itself, plus the one button that matters on this screen. */
@Composable
fun PublicKeyBlock(publicKey: String) {
    // The platform clipboard rather than the Compose one: this is a single line of text going into
    // a text editor on another machine, and the Android service does that without a suspend call
    // and without an API that has been moved twice in as many releases.
    val context = LocalContext.current
    var copied by remember { mutableStateOf(false) }
    var showKey by remember { mutableStateOf(false) }

    Column(Modifier.fillMaxWidth()) {
        if (publicKey.isBlank()) {
            Text(
                text = "The key has not been generated yet.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            return@Column
        }
        if (showKey) CommandText(publicKey)
        Row {
            PlainAction(if (copied) "Copied" else "Copy key", enabled = true) {
                context.getSystemService(ClipboardManager::class.java)
                    ?.setPrimaryClip(ClipData.newPlainText("Legion Control device key", publicKey))
                copied = true
            }
            PlainAction(if (showKey) "Hide key" else "Show key", enabled = true) { showKey = !showKey }
        }
        // The acknowledgement goes away again. Left latched it stops being news and starts reading
        // as the button's name, and then there is nothing left to press for a second copy.
        if (copied) {
            LaunchedEffect(Unit) {
                delay(2_000)
                copied = false
            }
        }
    }
}

/** A line meant to be typed on the far side. Scrolls sideways rather than wrapping mid command. */
@Composable
fun CommandText(command: String) {
    SelectionContainer {
        Text(
            text = command,
            style = MaterialTheme.typography.bodySmall.copy(fontFamily = FontFamily.Monospace),
            maxLines = 1,
            modifier = Modifier
                .fillMaxWidth()
                .horizontalScroll(rememberScrollState()),
        )
    }
}
