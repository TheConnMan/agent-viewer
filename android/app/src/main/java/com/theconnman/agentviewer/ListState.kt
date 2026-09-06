package com.theconnman.agentviewer

import android.content.Context
import android.content.SharedPreferences

/**
 * The list's remembered shape: which filter chip is active and which group headers have been
 * toggled away from their default. Both outlive the process, because "I collapsed that folder"
 * has to still be true when the app is reopened an hour later, not only across a terminal visit.
 *
 * A group is stored as a toggle rather than as "collapsed" so the two defaults coexist: `all` and
 * `by state` open every group until one is tapped shut, `by project` starts as a folder overview
 * with every group shut until one is tapped open.
 */
internal class ListPrefs(private val prefs: SharedPreferences) {
    constructor(context: Context) : this(context.getSharedPreferences("list", Context.MODE_PRIVATE))

    var filter: SessionFilter
        get() = prefs.getString(FILTER, null)?.let { stored ->
            SessionFilter.entries.firstOrNull { it.name == stored }
        } ?: SessionFilter.ALL
        set(value) = prefs.edit().putString(FILTER, value.name).apply()

    /** A fresh copy: the set a `SharedPreferences` hands back must never be mutated in place. */
    fun toggled(): Set<String> = prefs.getStringSet(TOGGLED, null)?.toSet().orEmpty()

    fun setToggled(toggled: Set<String>) {
        prefs.edit().putStringSet(TOGGLED, toggled.toSet()).apply()
    }

    private companion object {
        const val FILTER = "filter"
        const val TOGGLED = "toggled_groups"
    }
}

/** PURE: the persisted key of one group header under one filter. */
internal fun groupToggleKey(filter: SessionFilter, group: String): String = "${filter.name}|$group"

/** PURE: `by project` groups start collapsed, every other grouping starts open; a toggle flips it. */
internal fun groupExpanded(filter: SessionFilter, group: String, toggled: Set<String>): Boolean {
    val openByDefault = filter != SessionFilter.PROJECT
    return openByDefault != (groupToggleKey(filter, group) in toggled)
}

/** PURE: the toggled set after a tap on the header. */
internal fun toggleGroup(filter: SessionFilter, group: String, toggled: Set<String>): Set<String> {
    val key = groupToggleKey(filter, group)
    return if (key in toggled) toggled - key else toggled + key
}

/** PURE: the toggled set with the group forced open, for the folder a session was just started in. */
internal fun expandGroup(filter: SessionFilter, group: String, toggled: Set<String>): Set<String> =
    if (groupExpanded(filter, group, toggled)) toggled else toggleGroup(filter, group, toggled)

/** PURE: whether a grouping's headers are folders, which is what a long press can start work in. */
internal fun groupIsFolder(filter: SessionFilter): Boolean = filter != SessionFilter.STATE
