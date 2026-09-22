package com.frankenremote.app.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Computer
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.frankenremote.app.models.SavedHost

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MachinePickerScreen(
    savedHosts: List<SavedHost>,
    isNetworkAvailable: Boolean,
    onAddHost: (SavedHost) -> Unit,
    onConnect: (SavedHost, String) -> Unit,
    onOpenSettings: () -> Unit
) {
    var showAddDialog by remember { mutableStateOf(false) }
    var hostToConnect by remember { mutableStateOf<SavedHost?>(null) }
    var authTokenInput by remember { mutableStateOf("") }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("FrankenRemote") },
                actions = {
                    IconButton(onClick = onOpenSettings) {
                        Icon(Icons.Default.Settings, contentDescription = "Settings")
                    }
                }
            )
        },
        floatingActionButton = {
            FloatingActionButton(onClick = { showAddDialog = true }) {
                Icon(Icons.Default.Add, contentDescription = "Add Machine")
            }
        }
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
        ) {
            if (!isNetworkAvailable) {
                Surface(
                    color = MaterialTheme.colorScheme.errorContainer,
                    modifier = Modifier.fillMaxWidth()
                ) {
                    Text(
                        "Network Offline. Please verify your Tailscale connection.",
                        color = MaterialTheme.colorScheme.onErrorContainer,
                        modifier = Modifier.padding(12.dp)
                    )
                }
            }

            if (savedHosts.isEmpty()) {
                Box(
                    modifier = Modifier.fillMaxSize(),
                    contentAlignment = Alignment.Center
                ) {
                    Text(
                        "No saved machines.\nTap + to add your workstation.",
                        style = MaterialTheme.typography.bodyLarge,
                        color = MaterialTheme.colorScheme.onSurfaceVariant
                    )
                }
            } else {
                LazyColumn(modifier = Modifier.fillMaxSize()) {
                    items(savedHosts) { host ->
                        ListItem(
                            headlineContent = { Text(host.name) },
                            supportingContent = { Text(host.displayEndpoint) },
                            leadingContent = {
                                Icon(Icons.Default.Computer, contentDescription = null)
                            },
                            modifier = Modifier.clickable {
                                hostToConnect = host
                            }
                        )
                        HorizontalDivider()
                    }
                }
            }
        }
    }

    // Add Machine Dialog
    if (showAddDialog) {
        var name by remember { mutableStateOf("") }
        var address by remember { mutableStateOf("") }
        var portStr by remember { mutableStateOf("443") }

        AlertDialog(
            onDismissRequest = { showAddDialog = false },
            title = { Text("Add Machine") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedTextField(
                        value = name,
                        onValueChange = { name = it },
                        label = { Text("Name (e.g. Workstation)") },
                        modifier = Modifier.fillMaxWidth()
                    )
                    OutlinedTextField(
                        value = address,
                        onValueChange = { address = it },
                        label = { Text("Tailnet Address") },
                        modifier = Modifier.fillMaxWidth()
                    )
                    OutlinedTextField(
                        value = portStr,
                        onValueChange = { portStr = it },
                        label = { Text("Port") },
                        modifier = Modifier.fillMaxWidth()
                    )
                }
            },
            confirmButton = {
                Button(
                    onClick = {
                        val port = portStr.toIntOrNull() ?: 443
                        val host = SavedHost(
                            name = if (name.isNotBlank()) name else address,
                            address = address.trim(),
                            port = port
                        )
                        onAddHost(host)
                        showAddDialog = false
                    },
                    disabled = address.isBlank()
                ) {
                    Text("Save")
                }
            },
            dismissButton = {
                TextButton(onClick = { showAddDialog = false }) {
                    Text("Cancel")
                }
            }
        )
    }

    // Connect Token Dialog
    hostToConnect?.let { host ->
        AlertDialog(
            onDismissRequest = {
                hostToConnect = null
                authTokenInput = ""
            },
            title = { Text("Connect to ${host.name}") },
            text = {
                Column {
                    Text("Enter the single-use session bootstrap token provided by your host daemon:")
                    Spacer(modifier = Modifier.height(8.dp))
                    OutlinedTextField(
                        value = authTokenInput,
                        onValueChange = { authTokenInput = it },
                        label = { Text("Auth Token") },
                        modifier = Modifier.fillMaxWidth()
                    )
                }
            },
            confirmButton = {
                Button(
                    onClick = {
                        val token = authTokenInput.trim()
                        hostToConnect = null
                        authTokenInput = ""
                        onConnect(host, token)
                    },
                    disabled = authTokenInput.isBlank()
                ) {
                    Text("Connect")
                }
            },
            dismissButton = {
                TextButton(onClick = {
                    hostToConnect = null
                    authTokenInput = ""
                }) {
                    Text("Cancel")
                }
            }
        )
    }
}
