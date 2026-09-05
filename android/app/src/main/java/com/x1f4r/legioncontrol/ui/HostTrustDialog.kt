package com.x1f4r.legioncontrol.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import com.x1f4r.legioncontrol.data.*

/** Local enrollment choices remain separate from the shared setup's suggested OS labels. */
@Composable
fun HostTrustDialog(dialog: MachineModel.Dialog.ConfirmTrustHostKey, onDismiss: () -> Unit, onConfirm: (HostTrustApproval) -> Unit) {
    val trust = dialog.trust
    val suggested = (trust.systems + dialog.suggestedSystems).distinctBy { it.id }
    var systemsText by remember(dialog) { mutableStateOf(suggested.joinToString(", ") { it.id }) }
    var selected by remember(dialog) { mutableStateOf("") }
    val assignments = remember(dialog) { mutableStateMapOf<String, String>() }
    val systems = if (trust.needsSetup) systemsText.split(',').map { it.trim() }.filter { it.isNotEmpty() }
        .map { id -> HostIdentitySystem(id, suggested.firstOrNull { it.id == id }?.name ?: id) }
    else trust.systems.map { it.copy(keys = emptyList()) }
    val eligible = systems.filter { system ->
        trust.systems.firstOrNull { it.id == system.id }?.keys.isNullOrEmpty() && assignments.values.none { it == system.id }
    }
    val valid = systems.isNotEmpty() && systems.map { it.id }.distinct().size == systems.size &&
        systems.all { it.id.matches(Regex("[A-Za-z0-9][A-Za-z0-9._-]{0,63}")) } &&
        trust.unassigned.all { assignments[it] in systems.map { s -> s.id } } && selected in eligible.map { it.id }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Verify host identity") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                Text(dialog.address)
                Text("Offered: ${dialog.fingerprint}")
                Text("Compare this fingerprint on the machine through a trusted connection before approving.")
                if (trust.needsSetup) {
                    Text("Local trust configuration: confirm every operating system using this address. These choices stay on this phone.")
                    OutlinedTextField(systemsText, { systemsText = it; selected = "" }, label = { Text("Operating system IDs, comma separated") })
                }
                trust.systems.filter { it.keys.isNotEmpty() }.forEach { system ->
                    Text("${system.name}: ${system.keys.joinToString { HostKeyStore.fingerprintOf(it.blob) }}")
                }
                trust.unassigned.forEach { blob ->
                    Text("Assign existing key ${HostKeyStore.fingerprintOf(blob)}")
                    systems.forEach { system ->
                        TextButton(onClick = { assignments[blob] = system.id; selected = "" }) {
                            Text((if (assignments[blob] == system.id) "Selected: " else "") + system.name)
                        }
                    }
                }
                Text("The offered key belongs to:")
                eligible.forEach { system ->
                    TextButton(onClick = { selected = system.id }) { Text((if (selected == system.id) "Selected: " else "") + system.name) }
                }
                if (eligible.isEmpty()) Text("Every local identity already has keys. No existing key can be replaced here.")
            }
        },
        confirmButton = { TextButton(enabled = valid, onClick = { onConfirm(HostTrustApproval(trust.revision, systems, selected, assignments.toMap())) }) { Text("Approve selected identity") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
fun HostIdentitySettingsDialog(address: String, snapshot: HostTrustSnapshot, suggestions: List<HostIdentitySystem>, onDismiss: () -> Unit, onSave: (List<HostIdentitySystem>) -> Unit) {
    var text by remember(address, snapshot) { mutableStateOf((snapshot.systems.ifEmpty { suggestions }).joinToString(", ") { it.id }) }
    val ids = text.split(',').map { it.trim() }.filter { it.isNotEmpty() }
    val valid = ids.isNotEmpty() && ids.distinct().size == ids.size && ids.all { it.matches(Regex("[A-Za-z0-9][A-Za-z0-9._-]{0,63}")) } && snapshot.systems.all { it.id in ids }
    AlertDialog(onDismissRequest = onDismiss, title = { Text("Local host identities") }, text = {
        Column(Modifier.verticalScroll(rememberScrollState())) {
            Text(address)
            Text("Reserve only operating systems you independently know use this address. This saves no offered key. Existing identities and fingerprints stay unchanged.")
            snapshot.systems.forEach { system -> Text("${system.name}: ${system.keys.joinToString { HostKeyStore.fingerprintOf(it.blob) }.ifEmpty { "awaiting fingerprint approval" }}") }
            snapshot.unassigned.forEach { Text("Unassigned existing key: ${HostKeyStore.fingerprintOf(it)}") }
            OutlinedTextField(text, { text = it }, label = { Text("Operating system IDs, comma separated") })
        }
    }, confirmButton = { TextButton(enabled = valid, onClick = { onSave(ids.map { HostIdentitySystem(it) }) }) { Text("Save local reservations") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } })
}
