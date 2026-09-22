package com.frankenremote.client

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test

class FrankenClientTest {

    @Test(expected = IllegalArgumentException::class)
    fun testEmptyHostAddressFails() {
        FrankenClient("", "auth_token")
    }

    @Test(expected = IllegalArgumentException::class)
    fun testEmptyAuthTokenFails() {
        FrankenClient("100.64.0.1:443", "")
    }

    @Test
    fun testErrorCodesMapping() {
        assertEquals(0, ErrorCodes.OK)
        assertEquals(-1, ErrorCodes.ERR_INVALID_ARGUMENT)
        assertEquals(-2, ErrorCodes.ERR_STALE_HANDLE)
        assertEquals(-3, ErrorCodes.ERR_ALREADY_CLOSED)
        assertEquals(-4, ErrorCodes.ERR_CONNECTION_FAILED)

        val staleEx = FrankenException.fromCode(ErrorCodes.ERR_STALE_HANDLE)
        assertEquals(ErrorCodes.ERR_STALE_HANDLE, staleEx.errorCode)
        assertTrue(staleEx.message?.contains("Stale") == true)
    }

    @Test
    fun testConnectionQualityDefaults() {
        val q = ConnectionQuality()
        assertEquals(0, q.rttMs)
        assertEquals(0, q.fps)
        assertEquals(ConnectionQuality.QualityTier.POOR, q.qualityTier)
    }
}
