/*
 * FrankenClient.kt — FrankenRemote Android Kotlin/JNI Client Wrapper
 *
 * Wraps the fr_native mobile FFI boundary in idiomatic Kotlin with Coroutines,
 * StateFlow UI state observation, AutoCloseable lifecycle, and generation safety.
 *
 * Constitutional Invariants (Plan §16.2 & ADR 0003):
 * 1. All logic stays in Rust (fr-client); Kotlin only passes commands and callbacks.
 * 2. Generation-checked opaque handles safely reject stale calls or double-free.
 * 3. Zero-copy decoded video via ANativeWindow / Surface directly to MediaCodec.
 */

package com.frankenremote.client

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.withContext
import java.io.Closeable

/**
 * Result / Error codes matching fr_mobile.h.
 */
object ErrorCodes {
    const val OK = 0
    const val ERR_INVALID_ARGUMENT = -1
    const val ERR_STALE_HANDLE = -2
    const val ERR_ALREADY_CLOSED = -3
    const val ERR_CONNECTION_FAILED = -4
    const val ERR_TIMEOUT = -5
    const val ERR_PERMISSION_DENIED = -6
    const val ERR_UNSUPPORTED = -7
}

/**
 * Exception thrown on FrankenRemote FFI failures.
 */
class FrankenException(val errorCode: Int, message: String) : Exception(message) {
    companion object {
        fun fromCode(code: Int): FrankenException = when (code) {
            ErrorCodes.ERR_INVALID_ARGUMENT -> FrankenException(code, "Invalid argument passed to session")
            ErrorCodes.ERR_STALE_HANDLE -> FrankenException(code, "Stale session handle: generation expired")
            ErrorCodes.ERR_ALREADY_CLOSED -> FrankenException(code, "Session is already closed")
            ErrorCodes.ERR_CONNECTION_FAILED -> FrankenException(code, "Connection to host failed")
            ErrorCodes.ERR_TIMEOUT -> FrankenException(code, "Operation timed out")
            ErrorCodes.ERR_PERMISSION_DENIED -> FrankenException(code, "Permission denied by host policy")
            ErrorCodes.ERR_UNSUPPORTED -> FrankenException(code, "Operation unsupported on host or device")
            else -> FrankenException(code, "FrankenRemote native error: $code")
        }
    }
}

/**
 * Lifecycle state of a remote workstation session.
 */
enum class SessionState {
    DISCONNECTED,
    CONNECTING,
    AUTHENTICATING,
    CONNECTED,
    RECONNECTING,
    FAILED
}

/**
 * Pointer input actions for touch and trackpad modes.
 */
enum class PointerAction(val value: Int) {
    MOVE(0),
    DOWN(1),
    UP(2),
    CANCEL(3)
}

/**
 * Pointer buttons.
 */
enum class PointerButton(val value: Int) {
    NONE(0),
    PRIMARY(1),
    SECONDARY(2),
    MIDDLE(3)
}

/**
 * Key action events.
 */
enum class KeyAction(val value: Int) {
    DOWN(0),
    UP(1)
}

/**
 * Connection quality metrics for UI status bar.
 */
data class ConnectionQuality(
    val rttMs: Int = 0,
    val jitterMs: Int = 0,
    val lossPermille: Int = 0,
    val fps: Int = 0,
    val bitrateKbps: Int = 0,
    val decodeTimeUs: Int = 0,
    val renderTimeUs: Int = 0,
    val qualityTier: QualityTier = QualityTier.POOR
) {
    enum class QualityTier {
        EXCELLENT,
        GOOD,
        DEGRADED,
        POOR
    }
}

/**
 * Idiomatic Kotlin client for FrankenRemote sessions on Android.
 */
class FrankenClient(
    val hostAddress: String,
    val authToken: String
) : Closeable {

    private var sessionHandle: Long = 0L
    private var isClosed: Boolean = false

    private val _sessionState = MutableStateFlow(SessionState.DISCONNECTED)
    val sessionState: StateFlow<SessionState> = _sessionState.asStateFlow()

    private val _isMicEnabled = MutableStateFlow(false)
    val isMicEnabled: StateFlow<Boolean> = _isMicEnabled.asStateFlow()

    init {
        require(hostAddress.isNotBlank()) { "hostAddress must not be blank" }
        require(authToken.isNotBlank()) { "authToken must not be blank" }

        ensureInitialized()

        val handle = nativeCreateSession(hostAddress, authToken)
        if (handle < 0) {
            throw FrankenException.fromCode(handle.toInt())
        }
        this.sessionHandle = handle
    }

    /**
     * Connect to remote workstation asynchronously.
     */
    suspend fun connect(): Result<Unit> = withContext(Dispatchers.IO) {
        synchronized(this@FrankenClient) {
            if (isClosed || sessionHandle == 0L) {
                return@withContext Result.failure(FrankenException.fromCode(ErrorCodes.ERR_ALREADY_CLOSED))
            }
            _sessionState.value = SessionState.CONNECTING
        }

        val res = nativeConnect(sessionHandle)
        if (res == ErrorCodes.OK) {
            _sessionState.value = SessionState.CONNECTED
            Result.success(Unit)
        } else {
            _sessionState.value = SessionState.FAILED
            Result.failure(FrankenException.fromCode(res))
        }
    }

    /**
     * Disconnect from remote workstation.
     */
    fun disconnect() {
        synchronized(this) {
            if (isClosed || sessionHandle == 0L) return
            nativeDisconnect(sessionHandle)
            _sessionState.value = SessionState.DISCONNECTED
        }
    }

    /**
     * Attach an Android Surface (ANativeWindow) for zero-copy hardware decode presentation.
     */
    fun attachSurface(surface: Any): Result<Unit> {
        synchronized(this) {
            if (isClosed || sessionHandle == 0L) {
                return Result.failure(FrankenException.fromCode(ErrorCodes.ERR_ALREADY_CLOSED))
            }
            val res = nativeSetSurface(sessionHandle, surface)
            return if (res == ErrorCodes.OK) Result.success(Unit) else Result.failure(FrankenException.fromCode(res))
        }
    }

    /**
     * Submit pointer events (touch tap, drag, trackpad movement).
     */
    fun sendPointer(x: Int, y: Int, action: PointerAction, button: PointerButton = PointerButton.NONE): Result<Unit> {
        synchronized(this) {
            if (isClosed || sessionHandle == 0L) {
                return Result.failure(FrankenException.fromCode(ErrorCodes.ERR_ALREADY_CLOSED))
            }
            val res = nativeSendPointer(sessionHandle, x, y, action.value, button.value)
            return if (res == ErrorCodes.OK) Result.success(Unit) else Result.failure(FrankenException.fromCode(res))
        }
    }

    /**
     * Submit keyboard events (hardware keyboard or virtual keycodes).
     */
    fun sendKey(keyCode: Int, action: KeyAction): Result<Unit> {
        synchronized(this) {
            if (isClosed || sessionHandle == 0L) {
                return Result.failure(FrankenException.fromCode(ErrorCodes.ERR_ALREADY_CLOSED))
            }
            val res = nativeSendKey(sessionHandle, keyCode, action.value)
            return if (res == ErrorCodes.OK) Result.success(Unit) else Result.failure(FrankenException.fromCode(res))
        }
    }

    /**
     * Submit scroll wheel or 2-finger scroll delta.
     */
    fun sendScroll(dx: Int, dy: Int): Result<Unit> {
        synchronized(this) {
            if (isClosed || sessionHandle == 0L) {
                return Result.failure(FrankenException.fromCode(ErrorCodes.ERR_ALREADY_CLOSED))
            }
            val res = nativeSendScroll(sessionHandle, dx, dy)
            return if (res == ErrorCodes.OK) Result.success(Unit) else Result.failure(FrankenException.fromCode(res))
        }
    }

    /**
     * Send direct UTF-8 text string (software keyboard IME commit).
     */
    fun sendText(text: String): Result<Unit> {
        synchronized(this) {
            if (isClosed || sessionHandle == 0L) {
                return Result.failure(FrankenException.fromCode(ErrorCodes.ERR_ALREADY_CLOSED))
            }
            val res = nativeSendText(sessionHandle, text)
            return if (res == ErrorCodes.OK) Result.success(Unit) else Result.failure(FrankenException.fromCode(res))
        }
    }

    /**
     * Toggle microphone uplink stream.
     */
    fun setMicEnabled(enabled: Boolean): Result<Unit> {
        synchronized(this) {
            if (isClosed || sessionHandle == 0L) {
                return Result.failure(FrankenException.fromCode(ErrorCodes.ERR_ALREADY_CLOSED))
            }
            val res = nativeSetMicEnabled(sessionHandle, enabled)
            return if (res == ErrorCodes.OK) {
                _isMicEnabled.value = enabled
                Result.success(Unit)
            } else {
                Result.failure(FrankenException.fromCode(res))
            }
        }
    }

    /**
     * Send clipboard text to remote host.
     */
    fun sendClipboard(text: String): Result<Unit> {
        synchronized(this) {
            if (isClosed || sessionHandle == 0L) {
                return Result.failure(FrankenException.fromCode(ErrorCodes.ERR_ALREADY_CLOSED))
            }
            val res = nativeSendClipboard(sessionHandle, text)
            return if (res == ErrorCodes.OK) Result.success(Unit) else Result.failure(FrankenException.fromCode(res))
        }
    }

    /**
     * Safely closes the session, invalidating the generation-checked handle.
     * Idempotent and thread-safe.
     */
    override fun close() {
        synchronized(this) {
            if (isClosed) return
            isClosed = true
            if (sessionHandle != 0L) {
                nativeDestroy(sessionHandle)
                sessionHandle = 0L
            }
            _sessionState.value = SessionState.DISCONNECTED
        }
    }

    companion object {
        private var isNativeLoaded = false

        private fun ensureInitialized() {
            if (!isNativeLoaded) {
                try {
                    System.loadLibrary("fr_native")
                } catch (_: UnsatisfiedLinkError) {
                    // Handled gracefully in tests or static linking
                }
                nativeInit()
                isNativeLoaded = true
            }
        }

        @JvmStatic private external fun nativeInit(): Int
        @JvmStatic private external fun nativeCreateSession(hostAddr: String, authToken: String): Long
        @JvmStatic private external fun nativeConnect(session: Long): Int
        @JvmStatic private external fun nativeDisconnect(session: Long): Int
        @JvmStatic private external fun nativeDestroy(session: Long): Int
        @JvmStatic private external fun nativeSendPointer(session: Long, x: Int, y: Int, action: Int, button: Int): Int
        @JvmStatic private external fun nativeSendKey(session: Long, keyCode: Int, action: Int): Int
        @JvmStatic private external fun nativeSendScroll(session: Long, dx: Int, dy: Int): Int
        @JvmStatic private external fun nativeSetSurface(session: Long, nativeWindow: Any?): Int
        @JvmStatic private external fun nativeSendText(session: Long, textUtf8: String): Int
        @JvmStatic private external fun nativeSetMicEnabled(session: Long, enabled: Boolean): Int
        @JvmStatic private external fun nativeSendClipboard(session: Long, textUtf8: String): Int
        @JvmStatic private external fun nativeGetQuality(session: Long, outQuality: Any?): Int
    }
}
