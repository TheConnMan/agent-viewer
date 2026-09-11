package com.theconnman.agentviewer

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.drawable.AdaptiveIconDrawable
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * The launcher mark is a trimmed robot-on-monitor vector (sage field, charcoal glyph). A
 * missing mipmap or a path that does not punch the screen and eye holes only shows up on the
 * home screen, so inflate the adaptive icon and check the sampled pixels here.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
@GraphicsMode(GraphicsMode.Mode.NATIVE)
class LauncherIconTest {

    @Test
    fun `application points at the adaptive launcher icon`() {
        val context = RuntimeEnvironment.getApplication()
        assertEquals(R.mipmap.ic_launcher, context.applicationInfo.icon)
        val round = context.getDrawable(R.mipmap.ic_launcher_round)
        assertNotNull("round launcher icon inflates", round)
        assertTrue(
            "round launcher icon is adaptive, got ${round!!::class.java.name}",
            round is AdaptiveIconDrawable,
        )
    }

    @Test
    fun `adaptive icon paints sage field and charcoal monitor`() {
        val context = RuntimeEnvironment.getApplication()
        val drawable = context.getDrawable(R.mipmap.ic_launcher)
        assertNotNull("launcher icon inflates", drawable)
        assertTrue(
            "launcher icon is adaptive, got ${drawable!!::class.java.name}",
            drawable is AdaptiveIconDrawable,
        )

        val adaptive = drawable as AdaptiveIconDrawable
        assertSage("background", rasterPoint(adaptive.background, 54, 54))
        assertCharcoal("foreground bezel", rasterPoint(adaptive.foreground, 54, 32))

        // The mask (circle/squircle) plus the 25% layer inset shift glyph coordinates, so
        // count colours rather than pinning a pixel.
        val size = 108
        val bitmap = Bitmap.createBitmap(size, size, Bitmap.Config.ARGB_8888)
        adaptive.setBounds(0, 0, size, size)
        adaptive.draw(Canvas(bitmap))
        var sage = 0
        var charcoal = 0
        for (x in 0 until size) {
            for (y in 0 until size) {
                val p = bitmap.getPixel(x, y)
                if (Color.alpha(p) < 200) continue
                if (Color.red(p) == 0x99 && Color.green(p) == 0xB1 && Color.blue(p) == 0x8D) sage++
                if (Color.red(p) < 40 && Color.green(p) < 40 && Color.blue(p) < 40) charcoal++
            }
        }
        assertTrue("sage field shows through the mask (sage=$sage)", sage > 1000)
        assertTrue("charcoal glyph shows through the mask (charcoal=$charcoal)", charcoal > 200)
    }

    @Test
    fun `foreground glyph punches the screen and eyes`() {
        val context = RuntimeEnvironment.getApplication()
        val drawable = context.getDrawable(R.drawable.ic_launcher_foreground)
        assertNotNull("foreground inflates", drawable)
        val size = 108
        val bitmap = Bitmap.createBitmap(size, size, Bitmap.Config.ARGB_8888)
        drawable!!.setBounds(0, 0, size, size)
        drawable.draw(Canvas(bitmap))
        // Viewport 630 maps 1:1 onto this 108px raster (no adaptive inset).
        assertEquals("corner stays transparent", 0, Color.alpha(bitmap.getPixel(2, 2)))
        assertEquals("screen interior is a hole", 0, Color.alpha(bitmap.getPixel(34, 38)))
        assertEquals("left eye is a hole", 0, Color.alpha(bitmap.getPixel(50, 51)))
        assertCharcoal("monitor bezel", bitmap.getPixel(54, 32))
        assertCharcoal("robot head", bitmap.getPixel(46, 46))
    }

    private fun rasterPoint(drawable: android.graphics.drawable.Drawable, x: Int, y: Int): Int {
        val size = 108
        val bitmap = Bitmap.createBitmap(size, size, Bitmap.Config.ARGB_8888)
        drawable.setBounds(0, 0, size, size)
        drawable.draw(Canvas(bitmap))
        return bitmap.getPixel(x, y)
    }

    private fun assertSage(label: String, pixel: Int) {
        assertTrue("$label alpha", Color.alpha(pixel) > 200)
        assertEquals("$label red", 0x99, Color.red(pixel))
        assertEquals("$label green", 0xB1, Color.green(pixel))
        assertEquals("$label blue", 0x8D, Color.blue(pixel))
    }

    private fun assertCharcoal(label: String, pixel: Int) {
        assertTrue("$label alpha", Color.alpha(pixel) > 200)
        assertTrue("$label is dark (r=${Color.red(pixel)} g=${Color.green(pixel)} b=${Color.blue(pixel)})",
            Color.red(pixel) < 40 && Color.green(pixel) < 40 && Color.blue(pixel) < 40)
    }
}
