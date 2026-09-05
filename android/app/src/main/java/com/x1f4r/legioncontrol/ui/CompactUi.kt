package com.x1f4r.legioncontrol.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties

/** Secondary controls use the same bounded, scrollable surface on a phone or a large screen. */
@Composable
internal fun DetailSheet(title: String, onClose: () -> Unit, content: @Composable ColumnScope.() -> Unit) {
    Dialog(onDismissRequest = onClose, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        BoxWithConstraints(Modifier.imePadding().padding(12.dp).fillMaxWidth()) {
        val maximumHeight = maxHeight * .92f
        Column(Modifier.widthIn(max = 800.dp).fillMaxWidth().heightIn(max = maximumHeight)
            .background(MaterialTheme.colorScheme.surface).align(Alignment.Center)) {
            Row(Modifier.fillMaxWidth().padding(start = 16.dp, end = 4.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(title, Modifier.weight(1f), style = MaterialTheme.typography.titleSmall)
                PlainAction("Close", enabled = true, onClick = onClose)
            }
            HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
            Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState()).padding(16.dp), content = content)
        }
        }
    }
}

@Composable
internal fun SettingsEntry(title: String, value: String? = null, onClick: () -> Unit) {
    Row(Modifier.fillMaxWidth().defaultMinSize(minHeight = 52.dp).clickable(onClick = onClick)
        .padding(vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
        Text(title, Modifier.weight(1f), style = MaterialTheme.typography.bodyMedium)
        value?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant) }
        Text("›", Modifier.padding(start = 12.dp), color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
    HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
}

@Composable
internal fun MoreMenu(label: String, content: @Composable ColumnScope.(close: () -> Unit) -> Unit) {
    var open by remember { mutableStateOf(false) }
    Box {
        TextButton(onClick = { open = true }, modifier = Modifier.defaultMinSize(minWidth = 48.dp, minHeight = 48.dp)
            .semantics { contentDescription = label }) { Text("More") }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) { content { open = false } }
    }
}

@Composable
internal fun MenuAction(label: String, enabled: Boolean = true, close: () -> Unit, onClick: () -> Unit) {
    DropdownMenuItem(text = { Text(label) }, enabled = enabled, onClick = { close(); onClick() })
}

/** Unknown activity is kept distinct from observed work even when the safety gate reports busy. */
internal fun compactActivityState(busy: com.x1f4r.legioncontrol.agent.BusyStatus?): String? = when {
    busy?.isUnknown == true -> "Activity unknown"
    busy?.monitored == false || busy?.evidence == "unmonitored" -> "Not monitored"
    busy?.isBusy == true -> "Busy"
    else -> null
}
