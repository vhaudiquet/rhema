package dev.rhema.phone.ui

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

// The Jadeite dark tokens, lifted from crates/rhema-code/src/palette.rs
// DARK: one design system across the TUI, the web UI, and this app.
// Dark is the design's primary; there is deliberately no light scheme
// and no dynamic color — the palette is the brand.
/** The design's dark ground; also the system-bar scrim. */
internal val JadeiteGround = Color(0xFF0F1413)
private val Surface = Color(0xFF171D1B)
private val Elevated = Color(0xFF1F2624)
private val Border = Color(0xFF28312E)
private val BorderStrong = Color(0xFF3D4844)
private val Text = Color(0xFFEEF3F1)
private val Text2 = Color(0xFFB9C6C1)
private val Accent = Color(0xFF35A882)
private val AccentText = Color(0xFF64C9A3)
private val OnAccent = Color(0xFF07110D)
private val Error = Color(0xFFE4687A)
private val Info = Color(0xFF8FA6B8)

private val JadeiteDark = darkColorScheme(
    primary = Accent,
    onPrimary = OnAccent,
    primaryContainer = Elevated,
    onPrimaryContainer = AccentText,
    secondary = AccentText,
    onSecondary = OnAccent,
    tertiary = Info,
    background = JadeiteGround,
    onBackground = Text,
    surface = Surface,
    onSurface = Text,
    surfaceVariant = Elevated,
    onSurfaceVariant = Text2,
    surfaceContainerLowest = JadeiteGround,
    surfaceContainerLow = Surface,
    surfaceContainer = Surface,
    surfaceContainerHigh = Elevated,
    surfaceContainerHighest = Elevated,
    outline = Border,
    outlineVariant = BorderStrong,
    error = Error,
)

/** The app's theme: Jadeite dark, always — matching the TUI and the web UI. */
@Composable
fun RhemaTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = JadeiteDark,
        content = content,
    )
}
