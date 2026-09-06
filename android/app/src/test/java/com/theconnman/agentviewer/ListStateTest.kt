package com.theconnman.agentviewer

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/**
 * The list's fold state: which defaults each grouping starts from, what a tap does to it, and
 * that all of it survives the app being killed and reopened (a fresh `ListPrefs` over the same
 * store must answer the same).
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class ListStateTest {
    private val folder = "/home/brian/git/agent-viewer"

    @Test
    fun `all and by state open every group until tapped, by project starts folded`() {
        assertTrue(groupExpanded(SessionFilter.ALL, folder, emptySet()))
        assertTrue(groupExpanded(SessionFilter.STATE, "working", emptySet()))
        assertFalse(groupExpanded(SessionFilter.PROJECT, folder, emptySet()))
    }

    @Test
    fun `a tap folds an open group and a second tap opens it again`() {
        val folded = toggleGroup(SessionFilter.ALL, folder, emptySet())
        assertFalse(groupExpanded(SessionFilter.ALL, folder, folded))
        val reopened = toggleGroup(SessionFilter.ALL, folder, folded)
        assertTrue(groupExpanded(SessionFilter.ALL, folder, reopened))
        assertEquals(emptySet(), reopened)
    }

    @Test
    fun `a fold under one filter does not fold the same group under another`() {
        val folded = toggleGroup(SessionFilter.ALL, folder, emptySet())
        assertTrue(groupExpanded(SessionFilter.PROJECT, folder, toggleGroup(SessionFilter.PROJECT, folder, emptySet())))
        assertFalse(groupExpanded(SessionFilter.PROJECT, folder, folded))
    }

    @Test
    fun `expanding a group is a no-op when it is already open`() {
        assertEquals(emptySet(), expandGroup(SessionFilter.ALL, folder, emptySet()))
        val folded = toggleGroup(SessionFilter.ALL, folder, emptySet())
        assertTrue(groupExpanded(SessionFilter.ALL, folder, expandGroup(SessionFilter.ALL, folder, folded)))
        assertTrue(groupExpanded(SessionFilter.PROJECT, folder, expandGroup(SessionFilter.PROJECT, folder, emptySet())))
    }

    @Test
    fun `only folder groupings can start a session from their header`() {
        assertTrue(groupIsFolder(SessionFilter.ALL))
        assertTrue(groupIsFolder(SessionFilter.PROJECT))
        assertFalse(groupIsFolder(SessionFilter.STATE))
    }

    @Test
    fun `filter and folds survive a relaunch`() {
        val context = RuntimeEnvironment.getApplication()
        val first = ListPrefs(context)
        assertEquals(SessionFilter.ALL, first.filter)
        assertEquals(emptySet(), first.toggled())

        first.filter = SessionFilter.PROJECT
        first.setToggled(toggleGroup(SessionFilter.PROJECT, folder, emptySet()))

        val relaunched = ListPrefs(context)
        assertEquals(SessionFilter.PROJECT, relaunched.filter)
        assertTrue(groupExpanded(SessionFilter.PROJECT, folder, relaunched.toggled()))
        assertFalse(groupExpanded(SessionFilter.PROJECT, "/somewhere/else", relaunched.toggled()))
    }

    @Test
    fun `an unknown stored filter falls back to all`() {
        val context = RuntimeEnvironment.getApplication()
        context.getSharedPreferences("list", android.content.Context.MODE_PRIVATE)
            .edit().putString("filter", "HIDDEN").commit()
        assertEquals(SessionFilter.ALL, ListPrefs(context).filter)
    }
}
