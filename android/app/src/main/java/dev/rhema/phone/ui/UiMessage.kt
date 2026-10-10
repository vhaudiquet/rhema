package dev.rhema.phone.ui

// Screen state holds the failure data, not a formatted string; user-visible
// text is resolved with stringResource at render time so configuration
// changes re-resolve it.
sealed interface UiMessage {
    data class Http(val code: Int) : UiMessage
    data class Network(val message: String) : UiMessage
}
