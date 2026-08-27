package com.x1f4r.legioncontrol.ui

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import com.x1f4r.legioncontrol.agent.Platform
import com.x1f4r.legioncontrol.ui.theme.LocalStatusColors

/**
 * Status is a mark and a sentence, never a badge and never a coloured capsule.
 *
 * The marks are drawn here rather than pulled from an icon set: six shapes at one size, in colours
 * that come from the theme, is less code than a dependency and it guarantees they all sit on the
 * same optical weight.
 */
enum class Mark {
    /** Working as it should. */
    Good,

    /** True, but you would want to know. */
    Attention,

    /** Not working. */
    Bad,

    /** Nothing wrong, nothing happening. */
    Idle,

    /** Something is running right now. */
    Busy,

    /** We do not know, and saying so is the honest answer. */
    Unknown,
}

@Composable
fun StatusMark(mark: Mark, size: Dp = 9.dp) {
    val palette = LocalStatusColors.current
    val colour = when (mark) {
        Mark.Good -> palette.good
        Mark.Attention -> palette.attention
        Mark.Bad -> palette.bad
        Mark.Busy -> palette.attention
        Mark.Idle, Mark.Unknown -> palette.quiet
    }
    Canvas(Modifier.size(size)) {
        val side = this.size.minDimension
        val centre = Offset(this.size.width / 2f, this.size.height / 2f)
        when (mark) {
            Mark.Good, Mark.Attention, Mark.Bad ->
                drawCircle(colour, radius = side / 2f, center = centre)

            Mark.Idle, Mark.Unknown -> {
                val width = side * 0.24f
                drawCircle(colour, radius = (side - width) / 2f, center = centre, style = Stroke(width))
            }

            Mark.Busy -> {
                val width = side * 0.26f
                drawArc(
                    color = colour,
                    startAngle = -60f,
                    sweepAngle = 280f,
                    useCenter = false,
                    topLeft = Offset(width / 2f, width / 2f),
                    size = Size(this.size.width - width, this.size.height - width),
                    style = Stroke(width, cap = StrokeCap.Round),
                )
            }
        }
    }
}

/**
 * A glyph for the running system. Drawn, for the same reason as the status marks, and because two
 * systems on one machine want to be told apart at a glance from across a desk.
 */
@Composable
fun PlatformGlyph(platform: Platform, size: Dp, colour: Color) {
    Canvas(Modifier.size(size)) {
        if (platform == Platform.WINDOWS) drawPanes(colour) else drawTerminal(colour)
    }
}

/** Four panes, the way that system has drawn itself for thirty years. */
private fun DrawScope.drawPanes(colour: Color) {
    val gap = size.minDimension * 0.13f
    val cell = (size.minDimension - gap) / 2f
    val corner = CornerRadius(cell * 0.16f, cell * 0.16f)
    for (row in 0..1) {
        for (column in 0..1) {
            drawRoundRect(
                color = colour,
                topLeft = Offset(column * (cell + gap), row * (cell + gap)),
                size = Size(cell, cell),
                cornerRadius = corner,
            )
        }
    }
}

/** A prompt in a frame: everything that is not four panes is a shell, here. */
private fun DrawScope.drawTerminal(colour: Color) {
    val side = size.minDimension
    val line = side * 0.1f
    drawRoundRect(
        color = colour,
        topLeft = Offset(line / 2f, side * 0.09f + line / 2f),
        size = Size(side - line, side * 0.82f - line),
        cornerRadius = CornerRadius(side * 0.16f, side * 0.16f),
        style = Stroke(line),
    )
    fun stroke(fromX: Float, fromY: Float, toX: Float, toY: Float) = drawLine(
        color = colour,
        start = Offset(side * fromX, side * fromY),
        end = Offset(side * toX, side * toY),
        strokeWidth = line,
        cap = StrokeCap.Round,
    )
    stroke(0.28f, 0.36f, 0.45f, 0.50f)
    stroke(0.45f, 0.50f, 0.28f, 0.64f)
    stroke(0.55f, 0.64f, 0.74f, 0.64f)
}
