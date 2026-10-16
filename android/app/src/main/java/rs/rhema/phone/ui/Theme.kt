package rs.rhema.phone.ui

import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Shapes
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp

// The Jadeite dark tokens, lifted from crates/rhema-code/src/palette.rs
// DARK: one design system across the TUI, the web UI, and this app.
// Dark is the design's primary; there is deliberately no light scheme
// and no dynamic color — the palette is the brand.
/** The design's dark ground; also the system-bar scrim. */
internal val JadeiteGround = Color(0xFF0F1413)
internal val JadeiteSurface = Color(0xFF171D1B)
internal val JadeiteElevated = Color(0xFF1F2624)
internal val JadeiteSunken = Color(0xFF0A0F0E)
internal val JadeiteBorder = Color(0xFF28312E)
internal val JadeiteBorderStrong = Color(0xFF3D4844)
internal val JadeiteText = Color(0xFFEEF3F1)
internal val JadeiteText2 = Color(0xFFB9C6C1)
internal val JadeiteText3 = Color(0xFF87948F)
internal val JadeiteAccent = Color(0xFF35A882)
internal val JadeiteAccentText = Color(0xFF64C9A3)
internal val JadeiteOnAccent = Color(0xFF07110D)
internal val JadeiteOk = Color(0xFF4FBF94)
internal val JadeiteWarn = Color(0xFFD9A643)
internal val JadeiteError = Color(0xFFE4687A)
internal val JadeiteInfo = Color(0xFF8FA6B8)

/** Machine spellings — keys, model ids, outcomes — render mono. */
internal val JadeiteMono: FontFamily = FontFamily.Monospace

private val JadeiteDark = darkColorScheme(
    primary = JadeiteAccent,
    onPrimary = JadeiteOnAccent,
    primaryContainer = JadeiteElevated,
    onPrimaryContainer = JadeiteAccentText,
    secondary = JadeiteAccentText,
    onSecondary = JadeiteOnAccent,
    tertiary = JadeiteInfo,
    background = JadeiteGround,
    onBackground = JadeiteText,
    surface = JadeiteSurface,
    onSurface = JadeiteText,
    surfaceVariant = JadeiteElevated,
    onSurfaceVariant = JadeiteText2,
    surfaceContainerLowest = JadeiteGround,
    surfaceContainerLow = JadeiteSurface,
    surfaceContainer = JadeiteSurface,
    surfaceContainerHigh = JadeiteElevated,
    surfaceContainerHighest = JadeiteElevated,
    outline = JadeiteBorder,
    outlineVariant = JadeiteBorderStrong,
    error = JadeiteError,
)

// The design's shape language: soft cards on a hairline grid, controls a
// step softer. Default Material radii read as someone else's app.
private val JadeiteShapes = Shapes(
    extraSmall = RoundedCornerShape(6.dp),
    small = RoundedCornerShape(8.dp),
    medium = RoundedCornerShape(10.dp),
    large = RoundedCornerShape(14.dp),
)

/** The app's theme: Jadeite dark, always — matching the TUI and the web UI. */
@Composable
fun RhemaTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = JadeiteDark,
        shapes = JadeiteShapes,
        content = content,
    )
}
