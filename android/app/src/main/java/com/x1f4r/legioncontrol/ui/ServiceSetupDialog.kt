package com.x1f4r.legioncontrol.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.contentOrNull

/** The full document and an explicit validation preview share one focused editing surface. */
@Composable
fun ServiceSetupDialog(editor: ServiceSetupModel) {
    if (!editor.isOpen) return
    Dialog(onDismissRequest = editor::close, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        Column(Modifier.fillMaxSize().background(MaterialTheme.colorScheme.background).imePadding().padding(20.dp)) {
            Text("Service setup", style = MaterialTheme.typography.titleLarge)
            Actions {
                PlainAction("Close", enabled = !editor.working) { editor.close() }
                PlainAction("Validate", enabled = !editor.working && editor.text.isNotBlank()) { editor.validate() }
                PlainAction("Save", enabled = editor.canSave, emphasis = editor.canSave) { editor.save() }
            }
            Column(Modifier.weight(1f).verticalScroll(rememberScrollState())) {
                if (editor.working) LinearProgressIndicator(Modifier.fillMaxWidth())
                editor.error?.let { StatusLine(Mark.Bad, it) }
                editor.preview?.let { ExplanationText(it) }
                if (editor.profiles.isNotEmpty()) {
                    SectionHeading("AI tools")
                    QuietText("Managed tools start with automatic updates off.")
                    editor.profiles.forEach { profile ->
                        val name = profile["name"]?.jsonPrimitive?.contentOrNull ?: profile["id"]?.jsonPrimitive?.contentOrNull ?: "AI tool"
                        val available = canAddAiProfile(profile)
                        val monitor = canMonitorAiProfile(profile)
                        if (available || monitor) {
                            PlainAction("${if (monitor) "Monitor" else "Add"} $name", enabled = !editor.working) { editor.useProfile(profile) }
                        } else Text(name, style = MaterialTheme.typography.bodyMedium)
                        profile["message"]?.jsonPrimitive?.contentOrNull?.takeIf { it.isNotBlank() }?.let { QuietText(it) }
                        if (available) profile["updateMethod"]?.jsonPrimitive?.contentOrNull?.takeIf { it.isNotBlank() }?.let { QuietText("Update method: $it") }
                    }
                }
                if (editor.templates.isNotEmpty()) {
                    SectionHeading("Templates")
                    Actions {
                        editor.templates.forEach { template ->
                            val name = template["name"]?.jsonPrimitive?.contentOrNull ?: template["id"]?.jsonPrimitive?.contentOrNull ?: "Template"
                            PlainAction(name, enabled = !editor.working) { editor.useTemplate(template) }
                        }
                    }
                }
                if (editor.text.isNotEmpty()) {
                    QuietText("Edit the full agent configuration. Validate shows the changes before Save applies them.")
                    DocumentField(editor.text, editor::edit, "Agent configuration JSON", enabled = !editor.working)
                }
            }
        }
    }
}
