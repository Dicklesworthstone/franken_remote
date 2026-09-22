package com.frankenremote.app

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.Surface
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.lifecycle.lifecycleScope
import com.frankenremote.app.lifecycle.AppLifecycleManager
import com.frankenremote.app.models.SavedHost
import com.frankenremote.app.ui.MachinePickerScreen
import com.frankenremote.app.ui.SessionViewerScreen
import com.frankenremote.app.ui.SettingsScreen
import com.frankenremote.app.ui.theme.FrankenTheme
import com.frankenremote.client.FrankenClient
import kotlinx.coroutines.launch

class MainActivity : ComponentActivity() {

    private lateinit var lifecycleManager: AppLifecycleManager
    private var activeClient: FrankenClient? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        lifecycleManager = AppLifecycleManager(this)
        lifecycle.addObserver(lifecycleManager)

        setupLifecycleHandlers()

        setContent {
            FrankenTheme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    MainAppContent()
                }
            }
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        lifecycleManager.unregister()
        activeClient?.close()
        activeClient = null
    }

    private fun setupLifecycleHandlers() {
        lifecycleManager.onBackgroundEntered = {
            // Plan §16.2: Backgrounding client immediately releases control and drops queued input.
            activeClient?.disconnect()
        }

        lifecycleManager.onForegroundResumed = {
            // Plan §16.2: Resume obtains fresh lease and recovery frame.
            activeClient?.let { client ->
                lifecycleScope.launch {
                    client.connect()
                }
            }
        }

        lifecycleManager.onNetworkRouteChanged = {
            // Plan §16.2: Network transitions re-evaluate route without preserving stale leases.
            activeClient?.let { client ->
                client.disconnect()
                lifecycleScope.launch {
                    client.connect()
                }
            }
        }
    }

    @Composable
    private fun MainAppContent() {
        val isNetworkAvailable by lifecycleManager.isNetworkAvailable.collectAsState()
        var currentScreen by remember { mutableStateOf<Screen>(Screen.MachinePicker) }
        var savedHosts by remember {
            mutableStateOf(
                listOf(
                    SavedHost(name = "Localhost Dev", address = "127.0.0.1", port = 4710)
                )
            )
        }
        var currentClient by remember { mutableStateOf<FrankenClient?>(null) }

        when (val screen = currentScreen) {
            is Screen.MachinePicker -> {
                MachinePickerScreen(
                    savedHosts = savedHosts,
                    isNetworkAvailable = isNetworkAvailable,
                    onAddHost = { newHost ->
                        savedHosts = savedHosts + newHost
                    },
                    onConnect = { host, token ->
                        try {
                            val client = FrankenClient(host.displayEndpoint, token)
                            activeClient = client
                            currentClient = client
                            lifecycleScope.launch {
                                client.connect()
                            }
                            currentScreen = Screen.SessionViewer
                        } catch (_: Exception) {
                            // Handled via state
                        }
                    },
                    onOpenSettings = {
                        currentScreen = Screen.Settings
                    }
                )
            }

            is Screen.SessionViewer -> {
                currentClient?.let { client ->
                    SessionViewerScreen(
                        client = client,
                        onDisconnect = {
                            activeClient = null
                            currentClient = null
                            currentScreen = Screen.MachinePicker
                        }
                    )
                } ?: run {
                    currentScreen = Screen.MachinePicker
                }
            }

            is Screen.Settings -> {
                SettingsScreen(
                    onBack = {
                        currentScreen = Screen.MachinePicker
                    }
                )
            }
        }
    }

    sealed interface Screen {
        object MachinePicker : Screen
        object SessionViewer : Screen
        object Settings : Screen
    }
}
