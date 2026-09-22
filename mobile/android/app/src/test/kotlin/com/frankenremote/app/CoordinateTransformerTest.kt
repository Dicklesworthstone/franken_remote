package com.frankenremote.app

import com.frankenremote.app.models.CoordinateTransformer
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test

class CoordinateTransformerTest {

    @Test
    fun testAspectFitCalculation() {
        val transformer = CoordinateTransformer(
            desktopWidth = 1920,
            desktopHeight = 1080,
            viewportWidth = 1080f,
            viewportHeight = 2400f,
            zoomScale = 1.0f
        )

        val rect = transformer.getFittedRect()
        assertEquals(1080f, rect.width, 0.1f)
        val expectedHeight = 1080f / (1920f / 1080f)
        assertEquals(expectedHeight, rect.height, 0.1f)
        assert(rect.top > 0f)
    }

    @Test
    fun testViewPointToDesktopPoint() {
        val transformer = CoordinateTransformer(
            desktopWidth = 1920,
            desktopHeight = 1080,
            viewportWidth = 1920f,
            viewportHeight = 1080f,
            zoomScale = 1.0f
        )

        val center = transformer.viewPointToDesktopPoint(960f, 540f)
        assertNotNull(center)
        assertEquals(960, center!!.first)
        assertEquals(540, center.second)

        val outside = transformer.viewPointToDesktopPoint(-10f, 500f)
        assertNull(outside)
    }

    @Test
    fun testTrackpadDeltaClamping() {
        val transformer = CoordinateTransformer(
            desktopWidth = 1920,
            desktopHeight = 1080,
            viewportWidth = 1920f,
            viewportHeight = 1080f,
            zoomScale = 1.0f
        )

        val updated = transformer.applyTrackpadDelta(
            currentDesktopX = 100,
            currentDesktopY = 100,
            deltaViewX = 50f,
            deltaViewY = 20f,
            sensitivity = 1.0f
        )
        assertEquals(150, updated.first)
        assertEquals(120, updated.second)

        val clamped = transformer.applyTrackpadDelta(
            currentDesktopX = 100,
            currentDesktopY = 100,
            deltaViewX = 5000f,
            deltaViewY = 5000f,
            sensitivity = 1.0f
        )
        assertEquals(1919, clamped.first)
        assertEquals(1079, clamped.second)
    }
}
