package com.x1f4r.legioncontrol.ui

import androidx.compose.animation.Crossfade
import androidx.compose.animation.core.tween
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.x1f4r.legioncontrol.ui.theme.LocalStatusColors
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.time.format.FormatStyle

/**
 * Every page is built from the same few pieces: a heading, a label with a value, a mark with a
 * sentence, a version, and a word you can press. No boxes, no elevation, no rounded containers
 * grouping anything, no capsules. What separates one part of a page from the next is a hairline and
 * some air.
 */

/** The left edge every value on a page lines up against. */
val LabelWidth = 118.dp

private val LabelGap = 12.dp

/** Long enough to read as movement, short enough that it never delays an answer. */
private const val FadeMillis = 190

@Composable
fun SectionHeading(
    title: String,
    note: String? = null,
    /** The first heading on a page sets this to false: a rule with nothing above it is noise. */
    showsRule: Boolean = true,
) {
    Column(Modifier.fillMaxWidth()) {
        if (showsRule) {
            Spacer(Modifier.height(12.dp))
            HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
        }
        Row(
            modifier = Modifier.padding(top = if (showsRule) 10.dp else 0.dp, bottom = 4.dp),
            verticalAlignment = Alignment.Bottom,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Text(title, style = MaterialTheme.typography.titleSmall)
            if (note != null) {
                Text(
                    text = note,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(bottom = 1.dp),
                )
            }
        }
    }
}

/** Compact read-only label/value rows; interactive controls retain their own touch target. */
@Composable
fun DetailRow(
    label: String,
    alignment: Alignment.Vertical = Alignment.CenterVertically,
    content: @Composable () -> Unit,
) {
    BoxWithConstraints(Modifier.fillMaxWidth()) {
    val labelSize = if (maxWidth < 340.dp) 92.dp else LabelWidth
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .defaultMinSize(minHeight = 36.dp)
            .padding(vertical = 4.dp),
        verticalAlignment = alignment,
    ) {
        Text(
            text = label,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.width(labelSize),
        )
        Spacer(Modifier.width(LabelGap))
        Column(Modifier.weight(1f)) { content() }
    }
    }
}

/** Lines a continuation up under the value column rather than under the label. */
@Composable
fun ValueIndent(content: @Composable () -> Unit) {
    Row(Modifier.fillMaxWidth()) {
        Spacer(Modifier.width(LabelWidth + LabelGap))
        Column(Modifier.weight(1f)) { content() }
    }
}

/**
 * A mark and a sentence, faded across when the sentence changes.
 *
 * The fade is the point. These lines are rewritten by a poll the user did not ask for, and a verdict
 * that snaps from one word to another while being read looks like a glitch rather than like news.
 */
@Composable
fun StatusLine(
    mark: Mark,
    text: String,
    style: TextStyle = MaterialTheme.typography.bodyMedium,
) {
    Crossfade(
        targetState = mark to text,
        animationSpec = tween(FadeMillis),
        label = "status",
    ) { (currentMark, currentText) ->
        Row(verticalAlignment = Alignment.Top) {
            // Nudged down so the mark sits on the text's optical middle, not level with its cap height.
            Box(Modifier.padding(top = 6.dp)) { StatusMark(currentMark) }
            Spacer(Modifier.width(8.dp))
            Text(currentText, style = style)
        }
    }
}

/** Versions poll every fifteen seconds, so they get a fixed width face and stop jittering. */
@Composable
fun VersionText(value: String?, placeholder: String) {
    Crossfade(
        targetState = value,
        animationSpec = tween(FadeMillis),
        label = "version",
    ) { current ->
        Text(
            text = current ?: placeholder,
            style = MaterialTheme.typography.bodyMedium.copy(fontFamily = FontFamily.Monospace),
            color = if (current == null) {
                MaterialTheme.colorScheme.onSurfaceVariant
            } else {
                MaterialTheme.colorScheme.onSurface
            },
        )
    }
}

/** Plain prose value, faded for the same reason a version is. */
@Composable
fun ValueText(value: String?, placeholder: String) {
    Crossfade(
        targetState = value,
        animationSpec = tween(FadeMillis),
        label = "value",
    ) { current ->
        Text(
            text = current ?: placeholder,
            style = MaterialTheme.typography.bodyMedium,
            color = if (current == null) {
                MaterialTheme.colorScheme.onSurfaceVariant
            } else {
                MaterialTheme.colorScheme.onSurface
            },
        )
    }
}

@Composable
fun QuietText(text: String, modifier: Modifier = Modifier) {
    Text(
        text = text,
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = modifier,
    )
}

/** Body prose, for the one or two sentences a section needs to explain itself. */
@Composable
fun ExplanationText(text: String, modifier: Modifier = Modifier) {
    Text(
        text = text,
        style = MaterialTheme.typography.bodyMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = modifier,
    )
}

/**
 * An action is a word you can press, tinted, at a size a thumb can hit. Not an outlined box: a page
 * with no containers on it should not grow four of them at the bottom of every section.
 *
 * [working] is what makes an ssh round trip bearable. Every one of these takes seconds, and the
 * spinner appears in the frame the press lands rather than after the far side has answered, so the
 * button never looks like it missed the touch.
 *
 * [emphasis] is for the one action on a page that there is a reason to press right now. It is not
 * decoration: nothing is emphasised unless something was actually found to do.
 */
@Composable
fun PlainAction(
    label: String,
    enabled: Boolean,
    destructive: Boolean = false,
    emphasis: Boolean = false,
    working: Boolean = false,
    onClick: () -> Unit,
) {
    val haptics = rememberHaptics()
    val tint = if (destructive) LocalStatusColors.current.bad else MaterialTheme.colorScheme.primary
    TextButton(
        onClick = {
            haptics.tick()
            onClick()
        },
        enabled = enabled && !working,
        colors = ButtonDefaults.textButtonColors(contentColor = tint),
        contentPadding = PaddingValues(horizontal = 12.dp, vertical = 8.dp),
        modifier = Modifier.defaultMinSize(minHeight = 48.dp),
    ) {
        if (working) {
            CircularProgressIndicator(
                modifier = Modifier.size(13.dp),
                strokeWidth = 1.5.dp,
                color = tint,
            )
            Spacer(Modifier.width(9.dp))
        }
        Text(
            text = label,
            style = MaterialTheme.typography.bodyMedium,
            fontWeight = if (emphasis) FontWeight.SemiBold else null,
        )
    }
}

/**
 * Actions sit on one line where they fit and wrap where they do not.
 *
 * Shifted left by exactly the button's own content padding, so the first action word hangs on the
 * same edge as every label above it instead of sitting a dozen pixels adrift of the column.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
fun Actions(content: @Composable () -> Unit) {
    FlowRow(
        modifier = Modifier
            .fillMaxWidth()
            .offset(x = (-12).dp),
        horizontalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        content()
    }
}

/**
 * A field for a word rather than for a document or a secret: an address, a port, a user name.
 *
 * The same hairline as the two below it, and the same reason for it. What is different is only what
 * goes in: one line, a fixed width face because these are addresses and names that are compared
 * character by character, and no autocorrect, which on a phone would otherwise turn a hostname into
 * a word.
 */
@Composable
fun PlainField(
    value: String,
    onValueChange: (String) -> Unit,
    placeholder: String,
    modifier: Modifier = Modifier,
    keyboardType: KeyboardType = KeyboardType.Ascii,
) {
    val cursor = MaterialTheme.colorScheme.primary
    Column(modifier.fillMaxWidth()) {
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .defaultMinSize(minHeight = 40.dp)
                .padding(vertical = 6.dp),
            contentAlignment = Alignment.CenterStart,
        ) {
            if (value.isEmpty()) {
                Text(
                    text = placeholder,
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            BasicTextField(
                value = value,
                onValueChange = onValueChange,
                singleLine = true,
                textStyle = MaterialTheme.typography.bodyMedium.copy(
                    fontFamily = FontFamily.Monospace,
                    color = MaterialTheme.colorScheme.onSurface,
                ),
                keyboardOptions = KeyboardOptions(
                    keyboardType = keyboardType,
                    autoCorrectEnabled = false,
                    capitalization = KeyboardCapitalization.None,
                    imeAction = ImeAction.Next,
                ),
                cursorBrush = SolidColor(cursor),
                modifier = Modifier.fillMaxWidth(),
            )
        }
        HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
    }
}

/**
 * A field for a document rather than for a word.
 *
 * The same hairline as [PlainField] and for the same reason, but it grows with what is in it,
 * because what goes in here is JSON: a single line box turns a document into something nobody can
 * read back.
 */
@Composable
fun DocumentField(
    value: String,
    onValueChange: (String) -> Unit,
    placeholder: String,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
) {
    val cursor = MaterialTheme.colorScheme.primary
    Column(modifier.fillMaxWidth()) {
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .defaultMinSize(minHeight = 120.dp)
                .padding(vertical = 8.dp),
        ) {
            if (value.isEmpty()) {
                Text(
                    text = placeholder,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            BasicTextField(
                enabled = enabled,
                value = value,
                onValueChange = onValueChange,
                textStyle = MaterialTheme.typography.bodySmall.copy(
                    fontFamily = FontFamily.Monospace,
                    color = MaterialTheme.colorScheme.onSurface,
                ),
                keyboardOptions = KeyboardOptions(
                    keyboardType = KeyboardType.Ascii,
                    autoCorrectEnabled = false,
                    capitalization = KeyboardCapitalization.None,
                ),
                cursorBrush = SolidColor(cursor),
                modifier = Modifier.fillMaxWidth(),
            )
        }
        HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
    }
}

// MARK: - Time

/** The agent stamps its state in UTC. Nobody reads that at a glance, so show a local clock time. */
internal fun localTime(iso: String?): String? {
    if (iso.isNullOrBlank()) return null
    return runCatching { Instant.parse(iso) }.getOrNull()?.let { localTime(it.toEpochMilli()) }
}

internal fun localTime(epochMillis: Long): String =
    DateTimeFormatter.ofLocalizedDateTime(FormatStyle.MEDIUM, FormatStyle.SHORT)
        .withZone(ZoneId.systemDefault())
        .format(Instant.ofEpochMilli(epochMillis))
