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

/**
 * The key screen.
 *
 * Nothing in this app works until the phone's own key is in authorized_keys on every system it
 * talks to, so when ssh comes back with a rejection this replaces the page rather than showing up
 * as an error line under an otherwise empty layout. The key is generated on the device and never
 * leaves it: the public half is here to be copied, the private half stays in app storage. A phone
 * is the most losable thing in the house, and one line removed from authorized_keys revokes it
 * without touching anything else.
 */
@Composable
fun AuthorisationScreen(
    publicKey: String,
    systems: List<MachineSystem>,
    detail: String?,
    onRetry: () -> Unit,
) {
    Column(
        modifier = Modifier
            .fillMaxSize()
            .systemBarsPadding()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 20.dp)
            .padding(top = 32.dp, bottom = 32.dp),
    ) {
        Text(
            text = "This phone is not authorised yet",
            style = MaterialTheme.typography.titleLarge,
        )
        Spacer(Modifier.height(14.dp))
        Text(
            text = "The connection was refused. Add the key below to authorized_keys on " +
                "${systemsSentence(systems)}, from a terminal on the machine itself, then try " +
                "again. Where that file lives depends on the system, and on Windows on whether " +
                "the account is an administrator.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )

        Spacer(Modifier.height(24.dp))
        PublicKeyBlock(publicKey)

        Spacer(Modifier.height(20.dp))
        AuthorizedKeysPaths()
        Spacer(Modifier.height(14.dp))
        Text(
            text = "The key goes on a line of its own, whichever of those files it is.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )

        if (!detail.isNullOrBlank()) {
            Spacer(Modifier.height(20.dp))
            Text(
                text = detail,
                style = MaterialTheme.typography.bodySmall.copy(fontFamily = FontFamily.Monospace),
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 5,
            )
        }

        Spacer(Modifier.height(16.dp))
        Row {
            PlainAction("Try again", enabled = true, onClick = onRetry)
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

    Column(Modifier.fillMaxWidth()) {
        if (publicKey.isBlank()) {
            Text(
                text = "The key has not been generated yet.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            return@Column
        }
        // The fixed width face is what marks this out as something to be pasted verbatim. No filled
        // panel behind it: nothing else on the page sits in a box, and one grey rectangle here would
        // be the only container in the app.
        SelectionContainer {
            Text(
                text = publicKey,
                style = MaterialTheme.typography.bodySmall.copy(fontFamily = FontFamily.Monospace),
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(vertical = 4.dp),
            )
        }
        Row {
            PlainAction(if (copied) "Copied" else "Copy the key", enabled = true) {
                context.getSystemService(ClipboardManager::class.java)
                    ?.setPrimaryClip(ClipData.newPlainText("Legion Control device key", publicKey))
                copied = true
            }
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
