package com.x1f4r.legioncontrol.ui.theme

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp

/**
 * A fixed, quiet palette rather than the wallpaper-derived one.
 *
 * Dynamic colour would tint a page whose whole job is to say green, amber or red about a machine,
 * and a status the same hue as the background is a status nobody reads. The surfaces here are neutral
 * white in the light and near-black ink in the dark, with one restrained accent for the things you
 * can press.
 */
private val InkLight = Color(0xFF202124)
private val InkDark = Color(0xFFEDEAE4)

private val LightScheme = lightColorScheme(
    primary = Color(0xFF2166D1),
    onPrimary = Color(0xFFFFFFFF),
    secondary = Color(0xFF5A6472),
    onSecondary = Color(0xFFFFFFFF),
    background = Color(0xFFFFFFFF),
    onBackground = InkLight,
    surface = Color(0xFFFFFFFF),
    onSurface = InkLight,
    surfaceVariant = Color(0xFFF3F4F6),
    onSurfaceVariant = Color(0xFF61656B),
    outline = Color(0xFF8D887F),
    outlineVariant = Color(0xFFE0DCD5),
    error = Color(0xFF9B2C21),
    onError = Color(0xFFFFFFFF),
)

private val DarkScheme = darkColorScheme(
    primary = Color(0xFF9CC2EC),
    onPrimary = Color(0xFF10233A),
    secondary = Color(0xFFB3BDCB),
    onSecondary = Color(0xFF1D242D),
    background = Color(0xFF131211),
    onBackground = InkDark,
    surface = Color(0xFF131211),
    onSurface = InkDark,
    surfaceVariant = Color(0xFF201F1D),
    onSurfaceVariant = Color(0xFF9C968D),
    outline = Color(0xFF6E6961),
    outlineVariant = Color(0xFF2F2D2A),
    error = Color(0xFFF0A198),
    onError = Color(0xFF3A0F0A),
)

/**
 * Material has no role for "this is healthy" or "this wants attention", and status is most of what
 * this screen says, so the two live here beside the scheme instead of being borrowed from tertiary.
 */
@Immutable
data class StatusColors(
    val good: Color,
    val attention: Color,
    val bad: Color,
    val quiet: Color,
)

val LocalStatusColors = staticCompositionLocalOf {
    StatusColors(Color.Unspecified, Color.Unspecified, Color.Unspecified, Color.Unspecified)
}

private val LightStatus = StatusColors(
    good = Color(0xFF2C6E45),
    attention = Color(0xFF8A5A08),
    bad = Color(0xFF9B2C21),
    quiet = Color(0xFF8D887F),
)

private val DarkStatus = StatusColors(
    good = Color(0xFF74BE8F),
    attention = Color(0xFFD8A251),
    bad = Color(0xFFE58C82),
    quiet = Color(0xFF7C776F),
)

/**
 * Slightly tighter and slightly more tracked than the Material defaults, which are tuned for cards
 * and buttons rather than for a page of label and value rows.
 */
private val LegionTypography = Typography().let { base ->
    base.copy(
        titleLarge = base.titleLarge.copy(fontSize = 22.sp, fontWeight = FontWeight.SemiBold),
        titleSmall = base.titleSmall.copy(
            fontSize = 15.sp,
            lineHeight = 20.sp,
            fontWeight = FontWeight.SemiBold,
            letterSpacing = 0.2.sp,
        ),
        bodyMedium = base.bodyMedium.copy(fontSize = 14.sp, lineHeight = 20.sp),
        bodySmall = base.bodySmall.copy(fontSize = 12.sp, lineHeight = 17.sp),
    )
}

@Composable
fun LegionTheme(
    darkTheme: Boolean = isSystemInDarkTheme(),
    content: @Composable () -> Unit,
) {
    androidx.compose.runtime.CompositionLocalProvider(
        LocalStatusColors provides if (darkTheme) DarkStatus else LightStatus,
    ) {
        MaterialTheme(
            colorScheme = if (darkTheme) DarkScheme else LightScheme,
            typography = LegionTypography,
            content = content,
        )
    }
}
