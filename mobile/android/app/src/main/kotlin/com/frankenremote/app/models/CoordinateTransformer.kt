package com.frankenremote.app.models

data class FittedRect(
    val left: Float,
    val top: Float,
    val width: Float,
    val height: Float
) {
    val right: Float get() = left + width
    val bottom: Float get() = top + height
}

class CoordinateTransformer(
    var desktopWidth: Int = 1920,
    var desktopHeight: Int = 1080,
    var viewportWidth: Float = 0f,
    var viewportHeight: Float = 0f,
    var zoomScale: Float = 1.0f,
    var panOffsetX: Float = 0f,
    var panOffsetY: Float = 0f
) {
    fun getFittedRect(): FittedRect {
        if (desktopWidth <= 0 || desktopHeight <= 0 || viewportWidth <= 0f || viewportHeight <= 0f) {
            return FittedRect(0f, 0f, 0f, 0f)
        }

        val aspectDesktop = desktopWidth.toFloat() / desktopHeight.toFloat()
        val aspectView = viewportWidth / viewportHeight

        val width: Float
        val height: Float

        if (aspectView > aspectDesktop) {
            height = viewportHeight * zoomScale
            width = height * aspectDesktop
        } else {
            width = viewportWidth * zoomScale
            height = width / aspectDesktop
        }

        val originX = (viewportWidth - width) / 2.0f + panOffsetX
        val originY = (viewportHeight - height) / 2.0f + panOffsetY

        return FittedRect(originX, originY, width, height)
    }

    fun viewPointToDesktopPoint(x: Float, y: Float): Pair<Int, Int>? {
        val rect = getFittedRect()
        if (rect.width <= 0f || rect.height <= 0f) return null

        val localX = x - rect.left
        val localY = y - rect.top

        if (localX < 0f || localX > rect.width || localY < 0f || localY > rect.height) {
            return null
        }

        val normX = localX / rect.width
        val normY = localY / rect.height

        val desktopX = (normX * desktopWidth).toInt().coerceIn(0, desktopWidth - 1)
        val desktopY = (normY * desktopHeight).toInt().coerceIn(0, desktopHeight - 1)

        return Pair(desktopX, desktopY)
    }

    fun applyTrackpadDelta(
        currentDesktopX: Int,
        currentDesktopY: Int,
        deltaViewX: Float,
        deltaViewY: Float,
        sensitivity: Float = 1.5f
    ): Pair<Int, Int> {
        val rect = getFittedRect()
        if (rect.width <= 0f || rect.height <= 0f) return Pair(currentDesktopX, currentDesktopY)

        val scaleX = desktopWidth.toFloat() / rect.width
        val scaleY = desktopHeight.toFloat() / rect.height

        val deltaDesktopX = deltaViewX * scaleX * sensitivity
        val deltaDesktopY = deltaViewY * scaleY * sensitivity

        val newX = (currentDesktopX + deltaDesktopX).toInt().coerceIn(0, desktopWidth - 1)
        val newY = (currentDesktopY + deltaDesktopY).toInt().coerceIn(0, desktopHeight - 1)

        return Pair(newX, newY)
    }
}
