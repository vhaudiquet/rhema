package dev.rhema.phone

import android.Manifest
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import dev.rhema.phone.di.AppContainer
import dev.rhema.phone.ui.ChatScreen
import dev.rhema.phone.ui.RhemaTheme
import dev.rhema.phone.ui.SessionsScreen
import dev.rhema.phone.ui.SettingsScreen

class MainActivity : ComponentActivity() {

    private val notificationPermission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // POST_NOTIFICATIONS is a runtime permission on API 33+. Asking every
        // cold start is deliberate; the OS throttles repeated denials itself.
        if (Build.VERSION.SDK_INT >= 33) {
            notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
        val container = (application as RhemaApp).container
        setContent {
            RhemaTheme {
                AppRoot(container)
            }
        }
    }
}

@Composable
private fun AppRoot(container: AppContainer) {
    // null means "still loading stored settings", not "unconfigured".
    val configured by container.settings.configured.collectAsState(initial = null)
    var chatSession by rememberSaveable { mutableStateOf<String?>(null) }
    var showSettings by rememberSaveable { mutableStateOf(false) }

    BackHandler(enabled = chatSession != null || showSettings) {
        chatSession = null
        showSettings = false
    }

    when {
        configured == null -> Box(
            modifier = Modifier.fillMaxSize(),
            contentAlignment = Alignment.Center,
        ) { CircularProgressIndicator() }

        // Unconfigured: first launch. SettingsScreen calls onConnected (a no-op
        // here); flipping `configured` in DataStore recomposes this root.
        configured == false -> SettingsScreen(container)
        showSettings -> SettingsScreen(container) { showSettings = false }
        chatSession != null -> {
            val key = chatSession
            if (key != null) ChatScreen(container, key) { chatSession = null }
        }
        else -> SessionsScreen(
            container,
            onOpenSession = { chatSession = it },
            onOpenSettings = { showSettings = true },
        )
    }
}
