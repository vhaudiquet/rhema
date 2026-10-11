package rs.rhema.phone.ui

import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.mikepenz.markdown.m3.Markdown
import com.mikepenz.markdown.m3.markdownColor
import com.mikepenz.markdown.m3.markdownTypography
import com.mikepenz.markdown.model.markdownDimens

// Agent replies are markdown; this renders them the way the web UI's
// AppMarkdown does. The parser runs the GFM flavour: raw HTML in the
// source stays literal, so model output cannot mount markup here.
// Code and links pin Jadeite tokens; everything else follows the theme.
// Headings step down from the Material display sizes: a chat reply is
// not a document (the web styles ride the same scale).
@Composable
internal fun AgentMarkdown(text: String, modifier: Modifier = Modifier, streaming: Boolean = false) {
    // The cursor rides the source itself: the parser treats it as text at
    // the stream edge, so it lands where the reply currently ends.
    val base = MaterialTheme.typography.bodyLarge
    Markdown(
        if (streaming) text + "▊" else text,
        colors = markdownColor(
            codeBackground = JadeiteSunken,
            inlineCodeBackground = JadeiteSunken,
            dividerColor = JadeiteBorder,
            tableBackground = JadeiteSurface,
        ),
        typography = markdownTypography(
            h1 = base.copy(fontWeight = FontWeight.Bold),
            h2 = base.copy(fontSize = 15.sp, fontWeight = FontWeight.Bold),
            h3 = base.copy(fontSize = 14.sp, fontWeight = FontWeight.Bold),
            h4 = base.copy(fontSize = 13.sp, fontWeight = FontWeight.Bold),
            h5 = base.copy(fontSize = 13.sp, fontWeight = FontWeight.Bold),
            h6 = base.copy(fontSize = 13.sp, fontWeight = FontWeight.Bold),
            code = MaterialTheme.typography.bodySmall.copy(
                fontFamily = JadeiteMono,
                color = JadeiteText,
            ),
            inlineCode = base.copy(fontFamily = JadeiteMono, color = JadeiteText),
            textLink = TextLinkStyles(
                style = SpanStyle(
                    color = JadeiteAccentText,
                    textDecoration = TextDecoration.Underline,
                ),
            ),
        ),
        // The default 160.dp per cell makes a three-column table wider
        // than the phone screen; at 100.dp it fits and only wider tables
        // turn into their horizontal-scroll variant.
        dimens = markdownDimens(tableCellWidth = 100.dp, tableCellPadding = 8.dp),
        modifier = modifier.fillMaxWidth(),
    )
}
