package com.theconnman.agentviewer

import android.graphics.Bitmap
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.Surface
import androidx.compose.ui.Modifier
import android.graphics.Canvas
import android.view.View
import androidx.activity.ComponentActivity
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.longClick
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.swipeDown
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.foundation.layout.Box
import java.io.File
import kotlin.test.assertEquals
import kotlin.test.assertNull
import kotlin.test.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * Renders the list, triage and navigation surfaces with sample sessions, drives the header
 * gestures (tap folds, long press starts work) through the real composables, and writes each
 * frame to `build/screenshots/` so a change to the layout can be looked at rather than argued
 * about. NATIVE graphics: the legacy stub paints nothing.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "w411dp-h891dp-xxhdpi")
@GraphicsMode(GraphicsMode.Mode.NATIVE)
class ScreenshotTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val colors = AppTheme.ANALOG_AMBER.colors
    private val now = System.currentTimeMillis()

    private fun session(
        id: String,
        title: String,
        status: Status,
        folder: String,
        backend: BackendKind = BackendKind.CLAUDE,
        ageMinutes: Long = 5,
        pr: String? = null,
    ) = Session(
        backend = backend,
        id = id,
        origin = SessionOrigin.BACKGROUND,
        title = title,
        cwd = folder,
        gitBranch = "task/${id}",
        status = status,
        createdAtMs = now - ageMinutes * 60_000 - 60_000,
        updatedAtMs = now - ageMinutes * 60_000,
        summary = "reading crates/agent-viewer-server/src/lib.rs",
        prRefs = listOfNotNull(pr?.let { PrRef(it) }),
        projectRoot = folder,
    )

    private val viewer = "/home/brian/git/theconnman/agent-viewer"
    private val router = "/home/brian/git/theconnman/agent-router"
    private val wiki = "/home/brian/wiki"
    private val sessions = listOf(
        session("a1", "Phone feedback: spawn from folder headers", Status.Working, viewer),
        session("a2", "Keep the companion server up", Status.NeedsInput("Approve the unit file"), viewer, BackendKind.CODEX, 42, pr = "118"),
        session("a3", "Review icon placement", Status.Done, viewer, BackendKind.GROK, 130),
        session("b1", "Route Grok spawns", Status.Idle, router, BackendKind.CODEX, 900),
        // A parked thread holding the wiki folder open: never a row, but the folder is a place.
        session("h1", "Hold", Status.Done, wiki, BackendKind.CODEX, 4000),
    )

    /** Software-draws the window into a PNG: Robolectric never delivers the draw callback the
     * compose-test `captureToImage` waits on, but `View.draw` under NATIVE graphics paints. */
    private fun save(name: String) {
        compose.waitForIdle()
        val view: View = compose.activity.window.decorView
        val width = if (view.width > 0) view.width else 1080
        val height = if (view.height > 0) view.height else 2340
        if (view.width == 0) {
            view.measure(
                View.MeasureSpec.makeMeasureSpec(width, View.MeasureSpec.EXACTLY),
                View.MeasureSpec.makeMeasureSpec(height, View.MeasureSpec.EXACTLY),
            )
            view.layout(0, 0, width, height)
        }
        val bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
        view.draw(Canvas(bitmap))
        val dir = File("build/screenshots").apply { mkdirs() }
        File(dir, "$name.png").outputStream().use { bitmap.compress(Bitmap.CompressFormat.PNG, 100, it) }
        var painted = 0
        for (x in 0 until width step 8) for (y in 0 until height step 8) {
            if (bitmap.getPixel(x, y) != 0) painted++
        }
        assertTrue(painted > 0, "$name painted nothing")
    }

    @Test
    fun `list folds a group on tap and starts work on a long press`() {
        var filter = SessionFilter.ALL
        var toggled = emptySet<String>()
        var spawnFolder: String? = null
        compose.setContent {
            var filterState by remember { mutableStateOf(filter) }
            var toggledState by remember { mutableStateOf(toggled) }
            Surface(modifier = Modifier.fillMaxSize(), color = colors.bg, contentColor = colors.text) {
                Column(modifier = Modifier.fillMaxSize()) {
                    Box(modifier = Modifier.weight(1f)) {
                        SessionListScreen(
                            sessions = sessions,
                            error = null,
                            colors = colors,
                            filter = filterState,
                            toggledGroups = toggledState,
                            onFilter = {
                                filterState = it
                                filter = it
                            },
                            onToggleGroup = {
                                toggledState = toggleGroup(filterState, it, toggledState)
                                toggled = toggledState
                            },
                            onTheme = {},
                            onOpen = {},
                            onActions = {},
                            onSpawn = { spawnFolder = it },
                            isRefreshing = false,
                            onRefresh = {},
                        )
                    }
                    BottomNavigationPreview(sessions, colors)
                }
            }
        }
        compose.waitForIdle()
        compose.onNodeWithText("Keep the companion server up").assertIsDisplayed()
        compose.onNodeWithText("Hold").assertDoesNotExist()
        compose.onNodeWithText(wiki).assertIsDisplayed()
        save("list-all")

        compose.onNodeWithContentDescription("collapse $wiki").performTouchInput { longClick() }
        compose.waitForIdle()
        assertEquals(wiki, spawnFolder, "a folder held open by a hold thread still starts work")
        spawnFolder = null

        compose.onNodeWithContentDescription("collapse $viewer").performClick()
        compose.waitForIdle()
        assertTrue(groupToggleKey(SessionFilter.ALL, viewer) in toggled)
        compose.onNodeWithText("Keep the companion server up").assertDoesNotExist()
        compose.onNodeWithText("Route Grok spawns").assertIsDisplayed()
        save("list-folded")

        compose.onNodeWithContentDescription("expand $viewer").performTouchInput { longClick() }
        compose.waitForIdle()
        assertEquals(viewer, spawnFolder)

        spawnFolder = null
        compose.onNodeWithText("by state").performClick()
        compose.waitForIdle()
        assertEquals(SessionFilter.STATE, filter)
        compose.onNodeWithContentDescription("collapse needs input").performTouchInput { longClick() }
        compose.waitForIdle()
        assertNull(spawnFolder, "a state header is not a folder and cannot start work")
        assertEquals(emptySet(), toggled.filter { it.startsWith("STATE|") }.toSet(), "the long press did not fold it either")
        compose.onNodeWithText("Keep the companion server up").assertIsDisplayed()
        compose.onNodeWithText(wiki).assertDoesNotExist()
        save("list-by-state")
    }

    @Test
    fun `swiping down on the session list refreshes`() {
        var refreshes = 0
        compose.setContent {
            Surface(modifier = Modifier.fillMaxSize(), color = colors.bg, contentColor = colors.text) {
                SessionListScreen(
                    sessions = sessions,
                    error = null,
                    colors = colors,
                    filter = SessionFilter.ALL,
                    toggledGroups = emptySet(),
                    onFilter = {},
                    onToggleGroup = {},
                    onTheme = {},
                    onOpen = {},
                    onActions = {},
                    onSpawn = {},
                    isRefreshing = false,
                    onRefresh = { refreshes += 1 },
                )
            }
        }
        compose.waitForIdle()

        compose.onNodeWithTag("session-list").performTouchInput { swipeDown() }
        compose.waitForIdle()

        assertEquals(1, refreshes)
    }

    @Test
    fun `triage renders its queue`() {
        compose.setContent {
            Surface(modifier = Modifier.fillMaxSize(), color = colors.bg, contentColor = colors.text) {
                TriageScreenPreview(sessions, colors)
            }
        }
        compose.waitForIdle()
        compose.onNodeWithText("Keep the companion server up").assertIsDisplayed()
        save("triage")
    }
}
