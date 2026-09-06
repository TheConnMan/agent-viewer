package com.theconnman.agentviewer

import androidx.activity.ComponentActivity
import androidx.compose.ui.test.assertIsEnabled
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
import kotlin.test.assertEquals
import kotlin.test.assertTrue
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * The spawn sheet's one job: turn a folder long press plus a task into the `POST /sessions`
 * the server expects, with the folder it was opened on and no model of its own.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "w411dp-h891dp-xxhdpi")
@GraphicsMode(GraphicsMode.Mode.NATIVE)
class SpawnSheetTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    @Test
    fun `start posts the pressed folder and the typed task, pinned to the chosen provider`() {
        val folder = "/home/brian/git/theconnman/agent-viewer"
        LoopbackServer("""{"backend":"Claude","session_id":"abc","pid":null}""").use { server ->
            var started = 0
            compose.setContent {
                SpawnSheet(
                    folder = folder,
                    api = ApiClient(server.baseUrl),
                    colors = AppTheme.ANALOG_AMBER.colors,
                    onDismiss = {},
                    onStarted = { started += 1 },
                )
            }
            compose.waitForIdle()
            compose.onNodeWithText(folder).assertExists()
            compose.onNodeWithText("start").assertIsNotEnabled()

            compose.onNodeWithText("claude").performClick()
            compose.onNode(hasSetTextAction()).performTextInput("Keep the companion server up")
            compose.onNodeWithText("start").assertIsEnabled()
            compose.onNodeWithText("start").performClick()
            compose.waitUntil(10_000) { started == 1 }

            val request = server.requests.single()
            assertEquals("POST", request.method)
            assertEquals("/sessions", request.path)
            val body = Json.parseToJsonElement(request.body).jsonObject
            assertEquals("claude", body.getValue("provider").jsonPrimitive.content)
            assertEquals(folder, body.getValue("dir").jsonPrimitive.content)
            assertEquals("Keep the companion server up", body.getValue("task").jsonPrimitive.content)
            assertTrue(body.getValue("model") is JsonNull, "no model picker on the phone: $body")
        }
    }
}
