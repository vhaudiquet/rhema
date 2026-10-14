package rs.rhema.phone.ui

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

// The design language's shared pieces, mirroring the web components:
// one card voice, one label voice, one badge/dot/callout vocabulary.
// Screens compose these; nothing here knows about the claw API.

/** The uppercase letterspaced section label — the repeated typographic voice. */
@Composable
fun SectionLabel(text: String, modifier: Modifier = Modifier) {
    Text(
        text,
        modifier = modifier,
        style = MaterialTheme.typography.labelMedium,
        fontWeight = FontWeight.Bold,
        color = JadeiteText3,
        letterSpacing = MaterialTheme.typography.labelMedium.letterSpacing * 1.4f,
    )
}

enum class DotTone { Ok, Warn, Error, Idle }

/** One honest state dot: ok, warn, error, or idle gray. */
@Composable
fun StatusDot(tone: DotTone, modifier: Modifier = Modifier) {
    val color = when (tone) {
        DotTone.Ok -> JadeiteOk
        DotTone.Warn -> JadeiteWarn
        DotTone.Error -> JadeiteError
        DotTone.Idle -> JadeiteText3
    }
    Box(modifier = modifier.size(8.dp).clip(RoundedCornerShape(50)).background(color))
}

/** The pill: one small status or tag word, outlined in its tone. */
@Composable
fun AppBadge(text: String, modifier: Modifier = Modifier, tint: Color = JadeiteText3) {
    Surface(
        modifier = modifier,
        shape = RoundedCornerShape(50),
        color = Color.Transparent,
        border = BorderStroke(1.dp, tint),
    ) {
        Text(
            text,
            modifier = Modifier.padding(horizontal = 8.dp, vertical = 1.dp),
            style = MaterialTheme.typography.labelSmall,
            fontFamily = JadeiteMono,
            color = tint,
        )
    }
}

/**
 * The avatar square: the agent's picture when the claw has one, the
 * accent mark (or a neutral one) otherwise — the same precedence as the
 * web's `AgentAvatar`.
 */
@Composable
fun AppAvatar(
    mark: String,
    accent: Boolean,
    modifier: Modifier = Modifier,
    size: Dp = 32.dp,
    image: ImageBitmap? = null,
) {
    val shape = RoundedCornerShape(size / 4)
    Box(
        modifier = modifier
            .size(size)
            .clip(shape)
            .then(
                if (image == null) {
                    Modifier
                        .background(if (accent) JadeiteAccent else JadeiteElevated)
                        .border(
                            if (accent) 0.dp else 1.dp,
                            if (accent) Color.Transparent else JadeiteBorderStrong,
                            shape,
                        )
                } else {
                    Modifier
                },
            ),
        contentAlignment = Alignment.Center,
    ) {
        if (image != null) {
            Image(
                bitmap = image,
                contentDescription = null,
                contentScale = ContentScale.Crop,
                modifier = Modifier.matchParentSize(),
            )
        } else {
            Text(
                mark,
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.Bold,
                color = if (accent) JadeiteOnAccent else JadeiteText2,
            )
        }
    }
}

/** The page header: title, optional subtitle, trailing actions. */
@Composable
fun PageHeader(
    title: String,
    modifier: Modifier = Modifier,
    subtitle: String? = null,
    actions: (@Composable RowScope.() -> Unit)? = null,
) {
    Row(
        modifier = modifier
            .fillMaxWidth()
            .padding(horizontal = 16.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(modifier = Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.headlineSmall, fontWeight = FontWeight.Bold)
            subtitle?.let {
                Text(it, style = MaterialTheme.typography.bodySmall, color = JadeiteText3)
            }
        }
        actions?.invoke(this)
    }
}

/**
 * The one card: a titled container whose body rows separate by hairline.
 * Rows are the caller's job ([CardRow]); pass `first = true` to the
 * card's first row so it draws no divider.
 */
@Composable
fun RhemaCard(
    title: String,
    modifier: Modifier = Modifier,
    meta: String? = null,
    content: @Composable ColumnScope.() -> Unit,
) {
    Surface(
        modifier = modifier.fillMaxWidth(),
        shape = MaterialTheme.shapes.medium,
        color = JadeiteSurface,
        border = BorderStroke(1.dp, JadeiteBorder),
    ) {
        Column {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 16.dp, vertical = 12.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    title,
                    style = MaterialTheme.typography.titleMedium,
                    fontWeight = FontWeight.Bold,
                    modifier = Modifier.weight(1f, fill = false),
                )
                meta?.let {
                    Text(
                        it,
                        modifier = Modifier.padding(start = 12.dp),
                        style = MaterialTheme.typography.bodySmall,
                        color = JadeiteText3,
                    )
                }
            }
            HorizontalDivider(color = JadeiteBorder)
            Column(content = content)
        }
    }
}

/**
 * One row inside a [RhemaCard]: name and explanation on the left, control
 * (or value) on the right. Hairline separation is drawn here except on
 * the card's first row.
 */
@Composable
fun CardRow(
    title: String,
    modifier: Modifier = Modifier,
    description: String? = null,
    first: Boolean = false,
    control: (@Composable () -> Unit)? = null,
) {
    if (!first) HorizontalDivider(color = JadeiteBorder)
    Row(
        modifier = modifier
            .fillMaxWidth()
            .padding(horizontal = 16.dp, vertical = 12.dp),
        verticalAlignment = if (description == null) Alignment.CenterVertically else Alignment.Top,
        horizontalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Column(modifier = Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.titleSmall, fontWeight = FontWeight.SemiBold)
            description?.let {
                Text(it, style = MaterialTheme.typography.bodySmall, color = JadeiteText3)
            }
        }
        control?.invoke()
    }
}

/** Tones of [AppCallout]: the inline note vocabulary. */
enum class CalloutTone { Info, Ok, Warn, Error }

/** The toned inline callout — the "changes not applied" voice. */
@Composable
fun AppCallout(
    tone: CalloutTone,
    text: String,
    modifier: Modifier = Modifier,
    title: String? = null,
) {
    val tint = when (tone) {
        CalloutTone.Info -> JadeiteInfo
        CalloutTone.Ok -> JadeiteOk
        CalloutTone.Warn -> JadeiteWarn
        CalloutTone.Error -> JadeiteError
    }
    val mark = when (tone) {
        CalloutTone.Info -> "i"
        CalloutTone.Ok -> "✓"
        CalloutTone.Warn -> "↺"
        CalloutTone.Error -> "✗"
    }
    Surface(
        modifier = modifier.fillMaxWidth(),
        shape = RoundedCornerShape(8.dp),
        color = tint.copy(alpha = 0.08f),
        border = BorderStroke(1.dp, JadeiteBorder),
    ) {
        Row(
            modifier = Modifier.padding(12.dp),
            horizontalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Text(
                mark,
                style = MaterialTheme.typography.titleSmall,
                fontWeight = FontWeight.Bold,
                color = tint,
            )
            Column {
                title?.let {
                    Text(it, style = MaterialTheme.typography.titleSmall, fontWeight = FontWeight.Bold)
                }
                Text(text, style = MaterialTheme.typography.bodySmall, color = JadeiteText2)
            }
        }
    }
}

/**
 * The design's select: a sunken field that opens a menu. Options are
 * (value, label) pairs; an empty-string value is a legitimate option
 * ("(provider default)"), which is why selection is by value.
 */
@Composable
fun RhemaSelect(
    selected: String?,
    options: List<Pair<String, String>>,
    onSelect: (String) -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
) {
    var open by remember { mutableStateOf(false) }
    val label = options.firstOrNull { it.first == selected }?.second ?: selected ?: ""
    Box(modifier) {
        Surface(
            shape = RoundedCornerShape(8.dp),
            color = JadeiteSunken,
            border = BorderStroke(1.dp, JadeiteBorder),
            modifier = Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(8.dp))
                .clickable(enabled = enabled) { open = true },
        ) {
            Row(
                modifier = Modifier.padding(horizontal = 12.dp, vertical = 10.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    label,
                    modifier = Modifier.weight(1f),
                    style = MaterialTheme.typography.bodyMedium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    color = if (selected == null) JadeiteText3 else JadeiteText,
                )
                Text("▾", style = MaterialTheme.typography.bodySmall, color = JadeiteText3)
            }
        }
        DropdownMenu(
            expanded = open,
            onDismissRequest = { open = false },
            containerColor = JadeiteElevated,
        ) {
            options.forEach { (value, optionLabel) ->
                DropdownMenuItem(
                    text = {
                        Text(
                            optionLabel,
                            style = MaterialTheme.typography.bodyMedium,
                            fontFamily = if (value.isEmpty()) null else JadeiteMono,
                            color = if (value == selected) JadeiteAccentText else JadeiteText2,
                        )
                    },
                    onClick = {
                        open = false
                        onSelect(value)
                    },
                )
            }
        }
    }
}

/** The tool-card accent bar: the design's one-sided border. The parent row must size itself (IntrinsicSize.Min) for the bar to span it. */
@Composable
fun AccentBar(color: Color, modifier: Modifier = Modifier) {
    Box(modifier = modifier.width(3.dp).fillMaxHeight().background(color))
}

/** A quiet icon action: transparent, dim, brightening on press. */
@Composable
fun GhostIcon(
    icon: ImageVector,
    contentDescription: String,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    onClick: () -> Unit,
) {
    IconButton(onClick = onClick, enabled = enabled, modifier = modifier) {
        Icon(icon, contentDescription = contentDescription, tint = JadeiteText3)
    }
}
