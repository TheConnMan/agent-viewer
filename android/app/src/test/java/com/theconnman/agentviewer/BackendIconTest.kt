package com.theconnman.agentviewer

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import org.robolectric.RuntimeEnvironment
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * The backend marks are vendor SVG paths transcribed into vector drawables. A malformed path only
 * fails when the drawable is inflated, which on the phone is the first list render, so inflate and
 * rasterise every one here.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
@GraphicsMode(GraphicsMode.Mode.NATIVE)
class BackendIconTest {

    @Test
    fun `every backend has a mark that inflates and paints pixels`() {
        val context: android.content.Context = RuntimeEnvironment.getApplication()
        for (backend in BackendKind.entries) {
            val drawable = context.getDrawable(backend.iconRes())
            assertNotNull("${backend.name} mark inflates", drawable)
            val bitmap = Bitmap.createBitmap(48, 48, Bitmap.Config.ARGB_8888)
            drawable!!.setBounds(0, 0, 48, 48)
            drawable.draw(Canvas(bitmap))
            var painted = 0
            for (x in 0 until 48) for (y in 0 until 48) if (Color.alpha(bitmap.getPixel(x, y)) > 0) painted++
            assertTrue("${backend.name} mark paints something (painted=$painted)", painted > 100)
            assertTrue("${backend.name} mark is not a filled square (painted=$painted)", painted < 48 * 48)
        }
    }

    @Test
    fun `every backend has a distinct tag and display name`() {
        assertTrue(BackendKind.entries.map { it.tag() }.toSet().size == BackendKind.entries.size)
        assertTrue(BackendKind.entries.map { it.displayName() }.toSet().size == BackendKind.entries.size)
        assertTrue(BackendKind.entries.map { it.iconRes() }.toSet().size == BackendKind.entries.size)
    }
}
