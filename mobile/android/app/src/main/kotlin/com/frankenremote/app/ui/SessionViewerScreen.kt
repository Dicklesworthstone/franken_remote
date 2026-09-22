package com.frankenremote.app.ui

import android.view.SurfaceHolder
import android.view.SurfaceView
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectDragGestures
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import com.frankenremote.app.models.CoordinateTransformer
import com.frankenremote.app.models.TouchMode
import com.frankenremote.client.FrankenClient
import com.frankenremote.client.KeyAction
import com.frankenremote.client.PointerAction
import com.frankenremote.client.PointerButton
import kotlinx.coroutines.launch

@Composable
fun SessionViewerScreen(
    client: FrankenClient,
    onDisconnect: () -> Unit
) {
    val scope = rememberCoroutineScope()
    var touchMode by remember { mutableStateOf(TouchMode.DIRECT_TOUCH) }
    var isKeyboardVisible by remember { mutableStateOf(false) }
    var isModifierBarVisible by remember { mutableStateOf(false) }
    var textInputBuffer by remember { mutableStateOf("") }

    val transformer = remember { CoordinateTransformer() }
    var virtualCursorX by remember { mutableFloatStateOf(200f) }
    var virtualCursorY by remember { mutableFloatStateOf(300f) }
    var isVirtualCursorPressed by remember { mutableStateOf(false) }

    val isMicEnabled by client.isMicEnabled.collectAsState()

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black)
    ) {
        // Zero-copy hardware decode presentation via Android SurfaceView
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory = { context ->
                SurfaceView(context).apply {
                    holder.addCallback(object : SurfaceHolder.Callback {
                        override fun surfaceCreated(holder: SurfaceHolder) {
                            client.attachSurface(holder.surface)
                        }

                        override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
                            transformer.viewportWidth = width.toFloat()
                            transformer.viewportHeight = height.toFloat()
                        }

                        override fun surfaceDestroyed(holder: SurfaceHolder) {
                            // Surface destroyed; host presentation paused
                        }
                    })
                }
            }
        )

        // Gesture Overlay
        Box(
            modifier = Modifier
                .fillMaxSize()
                .pointerInput(touchMode) {
                    detectDragGestures(
                        onDragStart = { offset ->
                            when (touchMode) {
                                TouchMode.DIRECT_TOUCH -> {
                                    transformer.viewPointToDesktopPoint(offset.x, offset.y)?.let { (dx, dy) ->
                                        client.sendPointer(dx, dy, PointerAction.DOWN, PointerButton.PRIMARY)
                                    }
                                }
                                TouchMode.TRACKPAD -> {
                                    isVirtualCursorPressed = true
                                }
                            }
                        },
                        onDrag = { change, dragAmount ->
                            change.consume()
                            when (touchMode) {
                                TouchMode.DIRECT_TOUCH -> {
                                    transformer.viewPointToDesktopPoint(change.position.x, change.position.y)?.let { (dx, dy) ->
                                        client.sendPointer(dx, dy, PointerAction.MOVE, PointerButton.PRIMARY)
                                    }
                                }
                                TouchMode.TRACKPAD -> {
                                    val updated = transformer.applyTrackpadDelta(
                                        virtualCursorX.toInt(),
                                        virtualCursorY.toInt(),
                                        dragAmount.x,
                                        dragAmount.y
                                    )
                                    virtualCursorX = updated.first.toFloat()
                                    virtualCursorY = updated.second.toFloat()
                                    client.sendPointer(updated.first, updated.second, PointerAction.MOVE, PointerButton.NONE)
                                }
                            }
                        },
                        onDragEnd = {
                            when (touchMode) {
                                TouchMode.DIRECT_TOUCH -> {
                                    // Pointer up
                                }
                                TouchMode.TRACKPAD -> {
                                    isVirtualCursorPressed = false
                                    client.sendPointer(virtualCursorX.toInt(), virtualCursorY.toInt(), PointerAction.DOWN, PointerButton.PRIMARY)
                                    client.sendPointer(virtualCursorX.toInt(), virtualCursorY.toInt(), PointerAction.UP, PointerButton.PRIMARY)
                                }
                            }
                        }
                    )
                }
        )

        // Virtual Cursor overlay for Trackpad Mode
        if (touchMode == TouchMode.TRACKPAD) {
            Box(
                modifier = Modifier
                    .offset(x = (virtualCursorX - 12).dp, y = (virtualCursorY - 12).dp)
                    .size(24.dp)
                    .background(
                        color = if (isVirtualCursorPressed) MaterialTheme.colorScheme.primary else Color.White,
                        shape = CircleShape
                    )
            )
        }

        // Top Status Header
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(16.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically
        ) {
            Surface(
                color = MaterialTheme.colorScheme.surface.copy(alpha = 0.8f),
                shape = RoundedCornerShape(16.dp)
            ) {
                Row(modifier = Modifier.padding(horizontal = 12.dp, vertical = 6.dp), verticalAlignment = Alignment.CenterVertically) {
                    Box(
                        modifier = Modifier
                            .size(8.dp)
                            .background(Color.Green, CircleShape)
                    )
                    Spacer(modifier = Modifier.width(6.dp))
                    Text("Connected", style = MaterialTheme.typography.bodySmall)
                }
            }

            Surface(
                color = MaterialTheme.colorScheme.surface.copy(alpha = 0.8f),
                shape = RoundedCornerShape(16.dp)
            ) {
                Text(
                    touchMode.displayName,
                    style = MaterialTheme.typography.bodySmall,
                    modifier = Modifier.padding(horizontal = 12.dp, vertical = 6.dp)
                )
            }
        }

        // Bottom Control HUD
        Column(
            modifier = Modifier
                .align(Alignment.BottomCenter)
                .fillMaxWidth()
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp)
        ) {
            // Modifiers Bar
            if (isModifierBarVisible) {
                Surface(
                    color = MaterialTheme.colorScheme.surface.copy(alpha = 0.9f),
                    shape = RoundedCornerShape(12.dp)
                ) {
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(4.dp),
                        horizontalArrangement = Arrangement.SpaceEvenly
                    ) {
                        listOf("Esc" to 0x35, "Tab" to 0x30, "Ctrl" to 0x3B, "Alt" to 0x3A, "Shift" to 0x38, "Enter" to 0x24).forEach { (label, keycode) ->
                            TextButton(
                                onClick = {
                                    client.sendKey(keycode, KeyAction.DOWN)
                                    client.sendKey(keycode, KeyAction.UP)
                                }
                            ) {
                                Text(label, style = MaterialTheme.typography.bodySmall)
                            }
                        }
                    }
                }
            }

            // Keyboard text commit bar
            if (isKeyboardVisible) {
                Surface(
                    color = MaterialTheme.colorScheme.surface.copy(alpha = 0.9f),
                    shape = RoundedCornerShape(12.dp)
                ) {
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(8.dp),
                        verticalAlignment = Alignment.CenterVertically
                    ) {
                        OutlinedTextField(
                            value = textInputBuffer,
                            onValueChange = { textInputBuffer = it },
                            label = { Text("Type text for host...") },
                            modifier = Modifier.weight(1f)
                        )
                        Spacer(modifier = Modifier.width(8.dp))
                        Button(
                            onClick = {
                                if (textInputBuffer.isNotBlank()) {
                                    client.sendText(textInputBuffer)
                                    textInputBuffer = ""
                                }
                            }
                        ) {
                            Text("Send")
                        }
                    }
                }
            }

            // Main HUD toolbar
            Surface(
                color = MaterialTheme.colorScheme.surface.copy(alpha = 0.9f),
                shape = RoundedCornerShape(16.dp)
            ) {
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(8.dp),
                    horizontalArrangement = Arrangement.SpaceBetween,
                    verticalAlignment = Alignment.CenterVertically
                ) {
                    // Mode Toggle
                    FilledTonalButton(
                        onClick = {
                            touchMode = if (touchMode == TouchMode.DIRECT_TOUCH) TouchMode.TRACKPAD else TouchMode.DIRECT_TOUCH
                        }
                    ) {
                        Icon(
                            if (touchMode == TouchMode.DIRECT_TOUCH) Icons.Default.TouchApp else Icons.Default.Mouse,
                            contentDescription = null
                        )
                        Spacer(modifier = Modifier.width(4.dp))
                        Text(touchMode.displayName)
                    }

                    // Keyboard Toggle
                    IconButton(onClick = { isKeyboardVisible = !isKeyboardVisible }) {
                        Icon(Icons.Default.Keyboard, contentDescription = "Keyboard")
                    }

                    // Modifiers Toggle
                    IconButton(onClick = { isModifierBarVisible = !isModifierBarVisible }) {
                        Icon(Icons.Default.Code, contentDescription = "Modifiers")
                    }

                    // Mic Push-to-Talk
                    IconButton(
                        onClick = {
                            client.setMicEnabled(!isMicEnabled)
                        }
                    ) {
                        Icon(
                            if (isMicEnabled) Icons.Default.Mic else Icons.Default.MicOff,
                            contentDescription = "Microphone",
                            tint = if (isMicEnabled) Color.Green else LocalContentColor.current
                        )
                    }

                    // Disconnect
                    IconButton(
                        onClick = {
                            client.disconnect()
                            client.close()
                            onDisconnect()
                        }
                    ) {
                        Icon(Icons.Default.Close, contentDescription = "Disconnect", tint = Color.Red)
                    }
                }
            }
        }
    }
}
