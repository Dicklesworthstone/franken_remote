package com.frankenremote.app.models

import java.util.UUID

data class SavedHost(
    val id: String = UUID.randomUUID().toString(),
    val name: String,
    val address: String,
    val port: Int = 443,
    val lastConnected: Long? = null
) {
    val displayEndpoint: String
        get() = if (port == 443) address else "$address:$port"
}

enum class TouchMode(val displayName: String) {
    DIRECT_TOUCH("Direct Touch"),
    TRACKPAD("Trackpad")
}
