package com.frankenremote.app.lifecycle

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

class AppLifecycleManager(context: Context) : DefaultLifecycleObserver {

    private val connectivityManager =
        context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager

    private val _isNetworkAvailable = MutableStateFlow(true)
    val isNetworkAvailable: StateFlow<Boolean> = _isNetworkAvailable.asStateFlow()

    private val _isCellular = MutableStateFlow(false)
    val isCellular: StateFlow<Boolean> = _isCellular.asStateFlow()

    var onBackgroundEntered: (() -> Unit)? = null
    var onForegroundResumed: (() -> Unit)? = null
    var onNetworkRouteChanged: (() -> Unit)? = null

    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            val caps = connectivityManager.getNetworkCapabilities(network)
            val isCell = caps?.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) == true
            _isNetworkAvailable.value = true
            _isCellular.value = isCell
            onNetworkRouteChanged?.invoke()
        }

        override fun onLost(network: Network) {
            _isNetworkAvailable.value = false
            onNetworkRouteChanged?.invoke()
        }
    }

    init {
        val request = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .build()
        connectivityManager.registerNetworkCallback(request, networkCallback)
    }

    fun unregister() {
        try {
            connectivityManager.unregisterNetworkCallback(networkCallback)
        } catch (_: Exception) {}
    }

    override fun onStop(owner: LifecycleOwner) {
        // Plan §16.2: Backgrounding client immediately releases control and drops queued input.
        onBackgroundEntered?.invoke()
    }

    override fun onResume(owner: LifecycleOwner) {
        // Plan §16.2: Resuming obtains a fresh lease and recovery frame; never replays old inputs.
        onForegroundResumed?.invoke()
    }
}
