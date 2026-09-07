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

    private fun session(title: String, folder: String) = Session(
        backend = BackendKind.CLAUDE,
        id = "$folder/$title",
        origin = SessionOrigin.BACKGROUND,
        title = title,
        cwd = folder,
        status = Status.Done,
        createdAtMs = 0,
        updatedAtMs = 0,
        projectRoot = folder,
    )

    @Test
    fun `a folder whose only session is a hold thread keeps its header with nothing under it`() {
        val wiki = "/home/brian/wiki"
        val groups = folderGroups(
            listOf(
                session("Hold", wiki),
                session("Route Grok spawns", folder),
                session("HOLD", folder),
                session("Stale", "/home/brian/old").copy(hidden = true),
            ),
        )
        assertEquals(listOf(folder, wiki), groups.map { it.first }, "folders sort by path; a hidden row makes no folder")
        assertEquals(listOf("Route Grok spawns"), groups[0].second.map { it.title }, "hold rows are not drawn or counted")
        assertEquals(emptyList(), groups[1].second)
    }

    @Test
    fun `a search keeps a hold-only folder whose path matches and drops one that does not`() {
        val groups = folderGroups(listOf(session("Hold", "/home/brian/wiki"), session("Hold", folder))) { "wiki" in it.cwd }
        assertEquals(listOf("/home/brian/wiki"), groups.map { it.first })
    }

    @Test
    fun `an unknown stored filter falls back to all`() {
        val context = RuntimeEnvironment.getApplication()
        context.getSharedPreferences("list", android.content.Context.MODE_PRIVATE)
            .edit().putString("filter", "HIDDEN").commit()
        assertEquals(SessionFilter.ALL, ListPrefs(context).filter)
    }
}
