package com.theconnman.agentviewer

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.ExperimentalFoundationApi
import android.app.Activity
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.runtime.SideEffect
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.Dp
import androidx.core.view.WindowCompat
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.wrapContentHeight
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import kotlinx.coroutines.delay
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

data class ViewerColors(
    val bg: Color,
    val surface: Color,
    val border: Color,
    val text: Color,
    val muted: Color,
    val faint: Color,
    val accent: Color,
    val selbg: Color,
    val ok: Color,
    val warn: Color,
    val attn: Color,
    val err: Color,
    val merged: Color,
    val cc: Color,
    val cx: Color,
)

enum class AppTheme(val displayName: String, val colors: ViewerColors) {
    ANALOG_AMBER(
        "analog amber",
        ViewerColors(
            bg = Color(0xFF16130D), surface = Color(0xFF1D1A12), border = Color(0xFF3A3427),
            text = Color(0xFFE6DFCC), muted = Color(0xFF8D8570), faint = Color(0xFF5C563F),
            accent = Color(0xFFDFA649), selbg = Color(0xFF2E2817), ok = Color(0xFF7FAE5E),
            warn = Color(0xFFD9A93F), attn = Color(0xFFE86FA6), err = Color(0xFFCF6A52),
            merged = Color(0xFFB08AC9), cc = Color(0xFFD97757), cx = Color(0xFF74AA9C),
        ),
    ),
    DRACULA(
        "dracula",
        ViewerColors(
            bg = Color(0xFF282A36), surface = Color(0xFF44475A), border = Color(0xFF6272A4),
            text = Color(0xFFF8F8F2), muted = Color(0xFF6272A4), faint = Color(0xFF6272A4),
            accent = Color(0xFFBD93F9), selbg = Color(0xFF44475A), ok = Color(0xFF50FA7B),
            warn = Color(0xFFF1FA8C), attn = Color(0xFFFF79C6), err = Color(0xFFFF5555),
            merged = Color(0xFFBD93F9), cc = Color(0xFFFFB86C), cx = Color(0xFF8BE9FD),
        ),
    ),
    TOKYO_NIGHT(
        "tokyo night",
        ViewerColors(
            bg = Color(0xFF1A1B26), surface = Color(0xFF24283B), border = Color(0xFF414868),
            text = Color(0xFFC0CAF5), muted = Color(0xFFA9B1D6), faint = Color(0xFF565F89),
            accent = Color(0xFF7AA2F7), selbg = Color(0xFF33467C), ok = Color(0xFF9ECE6A),
            warn = Color(0xFFE0AF68), attn = Color(0xFFFF007C), err = Color(0xFFF7768E),
            merged = Color(0xFFBB9AF7), cc = Color(0xFFFF9E64), cx = Color(0xFF7DCFFF),
        ),
    ),
    PAPER_LIGHT(
        "paper light",
        ViewerColors(
            bg = Color(0xFFF4F1E8), surface = Color(0xFFEAE5D8), border = Color(0xFFCFC7B3),
            text = Color(0xFF2A2620), muted = Color(0xFF6E6857), faint = Color(0xFF9A937F),
            accent = Color(0xFFA1652A), selbg = Color(0xFFE2D9C1), ok = Color(0xFF4F7A3A),
            warn = Color(0xFF8A6410), attn = Color(0xFFA82F6B), err = Color(0xFFA8402C),
            merged = Color(0xFF7A4F96), cc = Color(0xFFB05436), cx = Color(0xFF3F7A6D),
        ),
    );

    companion object {
        val default: AppTheme = ANALOG_AMBER
    }
}

private enum class BottomTab { SESSIONS, TRIAGE, NEW }
internal enum class SessionFilter { ALL, PROJECT, STATE }

/** The filter chips, in the order the row draws them. There is no hidden chip: the server only
 * ever hands the phone visible sessions, so archived and companion threads stay on the desktop. */
internal val sessionFilters: List<Pair<SessionFilter, String>> = listOf(
    SessionFilter.ALL to "all",
    SessionFilter.PROJECT to "by project",
    SessionFilter.STATE to "by state",
)

internal fun normalSessionRows(sessions: List<Session>): List<Session> =
    sessions.filter { !it.hidden && !it.companion && !it.title.equals("Hold", ignoreCase = true) }

/** The phone's triage inbox, in the TUI's order (`ui::triage::triage_queue`): every needs-input
 * session, then every done or idle session, longest wait first within each group. Working, error
 * and unknown rows have nothing to review yet. The visibility rules are `normalSessionRows`. */
internal fun triageSessionRows(sessions: List<Session>): List<Session> =
    normalSessionRows(sessions)
        .filter { it.status is Status.NeedsInput || it.status == Status.Done || it.status == Status.Idle }
        .sortedWith(compareBy({ if (it.status is Status.NeedsInput) 0 else 1 }, { it.updatedAtMs }, { it.backend.name }, { it.id }))

internal fun Session.key(): String = "${backend.name}:$id"

internal class SessionSubmitter(
    private val create: suspend (NewSessionRequest) -> Unit,
) {
    private val mutex = Mutex()
    private var inFlight = false

    suspend fun submit(request: NewSessionRequest): Result<Unit>? {
        val accepted = mutex.withLock {
            if (inFlight) {
                false
            } else {
                inFlight = true
                true
            }
        }
        if (!accepted) return null
        return try {
            create(request)
            Result.success(Unit)
        } catch (error: CancellationException) {
            throw error
        } catch (error: Throwable) {
            Result.failure(error)
        } finally {
            mutex.withLock { inFlight = false }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun AgentViewerApp() {
    var appTheme by rememberSaveable { mutableStateOf(AppTheme.default) }
    var tab by rememberSaveable { mutableStateOf(BottomTab.SESSIONS) }
    var selectedBackend by rememberSaveable { mutableStateOf<BackendKind?>(null) }
    var selectedSessionId by rememberSaveable { mutableStateOf<String?>(null) }
    var sessions by remember { mutableStateOf<List<Session>>(emptyList()) }
    var loadError by remember { mutableStateOf<String?>(null) }
    var notice by remember { mutableStateOf<String?>(null) }
    // The row whose long-press action sheet is open, if any.
    var actionsKey by remember { mutableStateOf<String?>(null) }
    // Capabilities are backend-advertised per session and fetched on first use, so a tap on a row
    // that cannot be attached (Grok) is a notice rather than a broken terminal screen.
    val capabilityCache = remember { mutableStateMapOf<String, Capabilities>() }
    val api = remember { ApiClient() }
    val foreground = rememberForeground()
    val scope = rememberCoroutineScope()
    val colors = appTheme.colors
    val selectedSession = selectedBackend?.let { backend ->
        selectedSessionId?.let { id -> sessions.firstOrNull { it.backend == backend && it.id == id } }
    }
    val actionsSession = actionsKey?.let { key -> sessions.firstOrNull { it.key() == key } }

    LaunchedEffect(foreground) {
        if (!foreground) return@LaunchedEffect
        while (isActive) {
            runCatching { api.sessions() }
                .onSuccess {
                    sessions = it
                    loadError = null
                }
                .onFailure { loadError = it.message ?: "Could not load sessions" }
            delay(2_000)
        }
    }
    LaunchedEffect(notice) {
        if (notice != null) {
            delay(3_500)
            notice = null
        }
    }

    val refresh: () -> Unit = {
        scope.launch {
            runCatching { api.sessions() }
                .onSuccess {
                    sessions = it
                    loadError = null
                }
                .onFailure { loadError = it.message ?: "Could not load sessions" }
        }
        Unit
    }
    suspend fun capabilitiesFor(session: Session): Capabilities? =
        capabilityCache[session.key()]
            ?: runCatching { api.capabilities(session.id) }.getOrNull()?.also { capabilityCache[session.key()] = it }
    LaunchedEffect(actionsKey) {
        actionsSession?.let { capabilitiesFor(it) }
    }
    // A tap on any row goes straight into the session's terminal; there is no detail screen.
    val open: (Session) -> Unit = { session ->
        scope.launch {
            val capabilities = capabilitiesFor(session)
            when {
                capabilities == null -> notice = "could not check the ${session.backend.displayName()} session's capabilities"
                !capabilities.attach -> notice = "${session.backend.displayName()} sessions cannot be attached from the phone"
                else -> {
                    selectedBackend = session.backend
                    selectedSessionId = session.id
                }
            }
        }
        Unit
    }
    val openActions: (Session) -> Unit = { actionsKey = it.key() }
    val scheme = if (appTheme == AppTheme.PAPER_LIGHT) {
        lightColorScheme(primary = colors.accent, surface = colors.surface, onSurface = colors.text)
    } else {
        darkColorScheme(primary = colors.accent, surface = colors.surface, onSurface = colors.text)
    }

    // The window is edge to edge, so the status and navigation bars are drawn over our own
    // background; their icons have to follow the theme or they vanish (white on white).
    val view = LocalView.current
    val lightBars = appTheme == AppTheme.PAPER_LIGHT
    SideEffect {
        (view.context as? Activity)?.window?.let { window ->
            val controller = WindowCompat.getInsetsController(window, view)
            controller.isAppearanceLightStatusBars = lightBars
            controller.isAppearanceLightNavigationBars = lightBars
        }
    }

    MaterialTheme(colorScheme = scheme) {
        // The Surface paints the whole window, bars included; the content is inset from them.
        Surface(
            modifier = Modifier.fillMaxSize(),
            color = colors.bg,
            contentColor = colors.text,
        ) {
            Box(modifier = Modifier.fillMaxSize().safeDrawingPadding()) {
                selectedSession?.let { session ->
                    TerminalConsole(
                        session = session,
                        colors = colors,
                        foreground = foreground,
                        onBack = {
                            selectedBackend = null
                            selectedSessionId = null
                        },
                    )
                } ?: Column(modifier = Modifier.fillMaxSize()) {
                    Box(modifier = Modifier.weight(1f)) {
                        when (tab) {
                            BottomTab.SESSIONS -> SessionListScreen(
                                sessions = sessions,
                                error = loadError,
                                colors = colors,
                                onTheme = { appTheme = nextTheme(appTheme) },
                                onOpen = open,
                                onActions = openActions,
                            )

                            BottomTab.TRIAGE -> TriageScreen(
                                sessions = sessions,
                                colors = colors,
                                onOpen = open,
                                onActions = openActions,
                                onBack = { tab = BottomTab.SESSIONS },
                            )

                            BottomTab.NEW -> ComposerScreen(
                                api = api,
                                colors = colors,
                                onCreated = {
                                    tab = BottomTab.SESSIONS
                                    refresh()
                                },
                                onClose = { tab = BottomTab.SESSIONS },
                            )
                        }
                        notice?.let { FooterNotice(it, colors, Modifier.align(Alignment.BottomCenter)) }
                    }
                    if (tab != BottomTab.NEW) {
                        BottomNavigation(tab = tab, sessions = sessions, colors = colors) { tab = it }
                    }
                }
            }
            actionsSession?.let { session ->
                SessionActionsSheet(
                    session = session,
                    capabilities = capabilityCache[session.key()],
                    api = api,
                    colors = colors,
                    onDismiss = { actionsKey = null },
                    onOpen = {
                        actionsKey = null
                        open(session)
                    },
                    onMutated = refresh,
                )
            }
        }
    }
}

/** The phone's footer notice: a short line that fades after a few seconds, never a modal. */
@Composable
private fun FooterNotice(text: String, colors: ViewerColors, modifier: Modifier = Modifier) {
    Text(
        text,
        color = colors.text,
        fontFamily = FontFamily.Monospace,
        fontSize = 11.sp,
        modifier = modifier.padding(horizontal = 18.dp, vertical = 10.dp)
            .background(colors.surface, RoundedCornerShape(9.dp))
            .border(1.dp, colors.border, RoundedCornerShape(9.dp))
            .padding(horizontal = 12.dp, vertical = 8.dp),
    )
}

@Composable
private fun rememberForeground(): Boolean {
    val owner = LocalLifecycleOwner.current
    var foreground by remember(owner) {
        mutableStateOf(owner.lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED))
    }
    DisposableEffect(owner) {
        val observer = LifecycleEventObserver { _, _ ->
            foreground = owner.lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED)
        }
        owner.lifecycle.addObserver(observer)
        onDispose { owner.lifecycle.removeObserver(observer) }
    }
    return foreground
}

@Composable
private fun SessionListScreen(
    sessions: List<Session>,
    error: String?,
    colors: ViewerColors,
    onTheme: () -> Unit,
    onOpen: (Session) -> Unit,
    onActions: (Session) -> Unit,
) {
    var filter by remember { mutableStateOf(SessionFilter.ALL) }
    var expandedProjects by remember { mutableStateOf(emptySet<String>()) }
    var searchVisible by remember { mutableStateOf(false) }
    var query by remember { mutableStateOf("") }
    val normalSessions = normalSessionRows(sessions)
    val filtered = normalSessions
        .filter { session ->
            query.isBlank() || listOf(
                session.title,
                session.summary,
                session.cwd,
                session.projectRoot.orEmpty(),
            ).any { it.contains(query, ignoreCase = true) }
        }
    val groups = if (filter == SessionFilter.STATE) {
        filtered.groupBy { statusLabel(it.status) }
            .toList()
            .sortedBy { (_, rows) -> rows.minOf(::statusRank) }
    } else {
        filtered.groupBy { it.projectRoot ?: it.cwd }.toSortedMap().toList()
    }
    val waiting = normalSessions.count { it.status is Status.NeedsInput }
    val working = normalSessions.count { it.status == Status.Working }
    val done = normalSessions.count { it.status == Status.Done }

    Column(modifier = Modifier.fillMaxSize()) {
        MainHeader(
            total = normalSessions.size,
            colors = colors,
            onSearch = {
                searchVisible = !searchVisible
                if (!searchVisible) query = ""
            },
            onTheme = onTheme,
        )
        if (searchVisible) {
            OutlinedTextField(
                value = query,
                onValueChange = { query = it },
                modifier = Modifier.fillMaxWidth().padding(horizontal = 18.dp, vertical = 4.dp),
                singleLine = true,
                placeholder = { Text("search sessions", color = colors.faint) },
                textStyle = androidx.compose.ui.text.TextStyle(
                    color = colors.text,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 12.sp,
                ),
            )
        }
        Row(
            modifier = Modifier.fillMaxWidth().padding(start = 18.dp, end = 18.dp, bottom = 12.dp),
            horizontalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            StateCount("waiting", waiting, colors.attn, colors)
            StateCount("working", working, colors.accent, colors)
            StateCount("done", done, colors.ok, colors)
        }
        FilterRow(filter, colors) { filter = it }
        if (error != null) {
            Text(
                text = error,
                color = colors.err,
                fontSize = 11.sp,
                fontFamily = FontFamily.Monospace,
                modifier = Modifier.padding(horizontal = 18.dp, vertical = 4.dp),
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
            )
        }
        LazyColumn(modifier = Modifier.weight(1f)) {
            groups.forEach { (group, rows) ->
                val expandable = filter == SessionFilter.PROJECT
                val expanded = !expandable || group in expandedProjects
                item(key = "group-$filter-$group") {
                    ProjectHeader(
                        label = group,
                        count = rows.size,
                        colors = colors,
                        expanded = expanded,
                        onToggle = if (expandable) {
                            {
                                expandedProjects = if (expanded) {
                                    expandedProjects - group
                                } else {
                                    expandedProjects + group
                                }
                            }
                        } else {
                            null
                        },
                    )
                }
                if (expanded) {
                    items(rows, key = { it.key() }) { session ->
                        SessionRow(session, colors, onOpen = { onOpen(session) }, onActions = { onActions(session) })
                    }
                }
            }
            if (groups.isEmpty()) {
                item { EmptyState("no sessions here", colors) }
            }
        }
    }
}

@Composable
private fun MainHeader(
    total: Int,
    colors: ViewerColors,
    onSearch: () -> Unit,
    onTheme: () -> Unit,
) {
    Row(
        modifier = Modifier.fillMaxWidth().height(68.dp).padding(horizontal = 18.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(modifier = Modifier.weight(1f)) {
            Text("sessions", color = colors.text, fontSize = 21.sp, fontWeight = FontWeight.SemiBold)
            Text(
                "remote-dev · $total total",
                color = colors.faint,
                fontSize = 10.sp,
                fontFamily = FontFamily.Monospace,
                letterSpacing = 0.4.sp,
            )
        }
        HeaderControl(onClick = onSearch) { SearchGlyph(colors.muted) }
        HeaderControl(onClick = onTheme) { GlobeGlyph(colors.muted) }
    }
}

@Composable
private fun HeaderControl(onClick: () -> Unit, content: @Composable () -> Unit) {
    Box(
        modifier = Modifier.size(44.dp).clip(CircleShape).clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) { content() }
}

@Composable
private fun SearchGlyph(color: Color) {
    Canvas(modifier = Modifier.size(20.dp)) {
        val stroke = 1.7.dp.toPx()
        drawCircle(
            color = color,
            radius = 6.dp.toPx(),
            center = Offset(9.dp.toPx(), 9.dp.toPx()),
            style = Stroke(width = stroke),
        )
        drawLine(
            color = color,
            start = Offset(13.5.dp.toPx(), 13.5.dp.toPx()),
            end = Offset(19.dp.toPx(), 19.dp.toPx()),
            strokeWidth = stroke,
        )
    }
}

@Composable
private fun GlobeGlyph(color: Color) {
    Canvas(modifier = Modifier.size(20.dp)) {
        val stroke = 1.7.dp.toPx()
        val center = Offset(10.dp.toPx(), 10.dp.toPx())
        drawCircle(color = color, radius = 8.dp.toPx(), center = center, style = Stroke(width = stroke))
        drawOval(
            color = color,
            topLeft = Offset(6.dp.toPx(), 2.dp.toPx()),
            size = androidx.compose.ui.geometry.Size(8.dp.toPx(), 16.dp.toPx()),
            style = Stroke(width = stroke),
        )
        drawLine(
            color = color,
            start = Offset(2.dp.toPx(), 10.dp.toPx()),
            end = Offset(18.dp.toPx(), 10.dp.toPx()),
            strokeWidth = stroke,
        )
    }
}

@Composable
private fun Header(
    title: String,
    subtitle: String?,
    colors: ViewerColors,
    leading: String? = null,
    trailing: String? = null,
    onLeading: (() -> Unit)? = null,
    onTrailing: (() -> Unit)? = null,
) {
    Row(
        modifier = Modifier.fillMaxWidth().height(68.dp).padding(horizontal = 18.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (leading != null) {
            Text(
                text = leading,
                color = colors.muted,
                fontSize = 26.sp,
                modifier = Modifier.padding(end = 12.dp).clickable { onLeading?.invoke() },
            )
        }
        Column(modifier = Modifier.weight(1f)) {
            Text(title, color = colors.text, fontSize = 21.sp, fontWeight = FontWeight.SemiBold)
            if (subtitle != null) {
                Text(
                    subtitle,
                    color = colors.faint,
                    fontSize = 10.sp,
                    fontFamily = FontFamily.Monospace,
                    letterSpacing = 0.4.sp,
                )
            }
        }
        if (trailing != null) {
            Text(
                text = trailing,
                color = colors.muted,
                fontSize = 10.sp,
                fontFamily = FontFamily.Monospace,
                modifier = Modifier.clip(RoundedCornerShape(7.dp)).clickable { onTrailing?.invoke() }.padding(8.dp),
            )
        }
    }
}

@Composable
private fun StateCount(label: String, count: Int, color: Color, colors: ViewerColors) {
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(7.dp)) {
        StatusDot(color)
        Text(count.toString(), color = colors.text, fontWeight = FontWeight.SemiBold, fontSize = 13.sp)
        Text(label, color = colors.muted, fontSize = 13.sp)
    }
}

@Composable
private fun FilterRow(selected: SessionFilter, colors: ViewerColors, onSelected: (SessionFilter) -> Unit) {
    val scroll = rememberScrollState()
    Row(
        modifier = Modifier.fillMaxWidth().horizontalScroll(scroll).padding(start = 18.dp, end = 18.dp, bottom = 14.dp),
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        sessionFilters.forEach { (filter, label) ->
            val active = filter == selected
            Text(
                text = label,
                color = if (active) colors.accent else colors.muted,
                fontSize = 12.sp,
                fontWeight = if (active) FontWeight.SemiBold else FontWeight.Normal,
                modifier = Modifier
                    .clip(RoundedCornerShape(20.dp))
                    .background(if (active) colors.selbg else Color.Transparent)
                    .border(1.dp, if (active) colors.accent else colors.border, RoundedCornerShape(20.dp))
                    .clickable { onSelected(filter) }
                    .padding(horizontal = 14.dp, vertical = 7.dp),
            )
        }
    }
}

@Composable
private fun ProjectHeader(
    label: String,
    count: Int,
    colors: ViewerColors,
    expanded: Boolean,
    onToggle: (() -> Unit)?,
) {
    Row(
        modifier = Modifier.fillMaxWidth().background(colors.surface).border(1.dp, colors.border)
            .then(if (onToggle != null) Modifier.clickable(onClick = onToggle) else Modifier)
            .padding(horizontal = 18.dp, vertical = 7.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(if (expanded) "⌄" else "›", color = colors.faint, fontSize = 14.sp)
        Spacer(Modifier.width(8.dp))
        Text(
            label,
            color = colors.muted,
            fontFamily = FontFamily.Monospace,
            fontSize = 11.sp,
            modifier = Modifier.weight(1f),
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        Text(count.toString(), color = colors.faint, fontFamily = FontFamily.Monospace, fontSize = 11.sp)
    }
}

/** One list row. A tap opens the session's terminal; a long press opens its action sheet. */
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun SessionRow(session: Session, colors: ViewerColors, onOpen: () -> Unit, onActions: () -> Unit) {
    Row(
        modifier = Modifier.fillMaxWidth().background(if (session.status is Status.NeedsInput) colors.selbg else colors.bg)
            .combinedClickable(onClick = onOpen, onLongClick = onActions)
            .drawBehind {
                drawLine(
                    color = colors.border,
                    start = Offset(0f, size.height),
                    end = Offset(size.width, size.height),
                    strokeWidth = 1.dp.toPx(),
                )
            }
            .padding(horizontal = 18.dp, vertical = 13.dp),
        horizontalArrangement = Arrangement.spacedBy(11.dp),
        verticalAlignment = Alignment.Top,
    ) {
        StatusDot(statusColor(session.status, colors), Modifier.padding(top = 5.dp))
        Column(modifier = Modifier.weight(1f)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                BackendIcon(session.backend, colors, size = 13.dp)
                Spacer(Modifier.width(7.dp))
                Text(
                    session.title,
                    color = colors.text,
                    fontSize = 14.sp,
                    fontWeight = FontWeight.Medium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            Text(
                session.summary.ifBlank { statusSummary(session.status) },
                color = colors.faint,
                fontFamily = FontFamily.Monospace,
                fontSize = 11.sp,
                lineHeight = 15.sp,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.padding(top = 4.dp),
            )
            Row(modifier = Modifier.padding(top = 4.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(statusLabel(session.status), color = statusColor(session.status, colors), fontSize = 11.sp, fontWeight = FontWeight.SemiBold)
                session.prRefs.firstOrNull()?.let { pr ->
                    Spacer(Modifier.width(8.dp))
                    Text(
                        "#${pr.id}",
                        color = colors.merged,
                        fontFamily = FontFamily.Monospace,
                        fontSize = 10.sp,
                        modifier = Modifier.border(1.dp, colors.merged, RoundedCornerShape(4.dp)).padding(horizontal = 5.dp, vertical = 1.dp),
                    )
                }
            }
        }
        Text(sessionAge(session.updatedAtMs), color = colors.faint, fontFamily = FontFamily.Monospace, fontSize = 11.sp)
    }
}

@Composable
private fun TriageScreen(
    sessions: List<Session>,
    colors: ViewerColors,
    onOpen: (Session) -> Unit,
    onActions: (Session) -> Unit,
    onBack: () -> Unit,
) {
    val queue = triageSessionRows(sessions)
    Column(modifier = Modifier.fillMaxSize()) {
        TriageHeader(queue.size, colors, onBack)
        LazyColumn(
            modifier = Modifier.weight(1f),
            verticalArrangement = Arrangement.spacedBy(10.dp),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(horizontal = 16.dp, vertical = 6.dp),
        ) {
            items(queue, key = { "triage-${it.key()}" }) { session ->
                TriageCard(session, colors, onOpen = { onOpen(session) }, onActions = { onActions(session) })
            }
            item {
                Row(
                    modifier = Modifier.fillMaxWidth().padding(vertical = 12.dp),
                    horizontalArrangement = Arrangement.Center,
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    CheckGlyph(colors.ok)
                    Spacer(Modifier.width(8.dp))
                    Text("nothing else is waiting on you", color = colors.muted, fontSize = 12.sp)
                }
            }
        }
    }
}

@Composable
private fun TriageHeader(count: Int, colors: ViewerColors, onBack: () -> Unit) {
    Row(
        modifier = Modifier.fillMaxWidth().height(68.dp).padding(horizontal = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        HeaderControl(onClick = onBack) { BackGlyph(colors.muted) }
        Column(modifier = Modifier.weight(1f)) {
            Text("triage", color = colors.text, fontSize = 21.sp, fontWeight = FontWeight.SemiBold)
            Text(
                "needs input, then done · longest wait first",
                color = colors.faint,
                fontFamily = FontFamily.Monospace,
                fontSize = 10.sp,
            )
        }
        Box(
            modifier = Modifier.height(26.dp).widthIn(min = 26.dp).background(colors.attn, CircleShape).padding(horizontal = 8.dp),
            contentAlignment = Alignment.Center,
        ) {
            Text(count.toString(), color = colors.bg, fontSize = 13.sp, fontWeight = FontWeight.Bold)
        }
        Spacer(Modifier.width(8.dp))
    }
}

/** One triage item. Like a list row, a tap opens the terminal and a long press opens the actions;
 * the card itself only says what the session is waiting for. */
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun TriageCard(
    session: Session,
    colors: ViewerColors,
    onOpen: () -> Unit,
    onActions: () -> Unit,
) {
    val waiting = session.status is Status.NeedsInput
    val accent = if (waiting) colors.attn else colors.ok
    val reason = when (val status = session.status) {
        is Status.NeedsInput -> status.reason ?: "Waiting for your input"
        Status.Done -> "Finished; review it or archive it"
        Status.Idle -> "Idle; review it or archive it"
        else -> statusLabel(status)
    }
    Column(
        modifier = Modifier.fillMaxWidth().clip(RoundedCornerShape(12.dp)).background(colors.surface)
            .combinedClickable(onClick = onOpen, onLongClick = onActions)
            .drawBehind {
                drawLine(
                    color = accent,
                    start = Offset(1.5.dp.toPx(), 6.dp.toPx()),
                    end = Offset(1.5.dp.toPx(), size.height - 6.dp.toPx()),
                    strokeWidth = 3.dp.toPx(),
                )
            }
            .padding(13.dp),
        verticalArrangement = Arrangement.spacedBy(9.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(sessionAge(session.updatedAtMs), color = accent, fontSize = 19.sp, fontWeight = FontWeight.Bold)
            Spacer(Modifier.width(9.dp))
            BackendIcon(session.backend, colors, size = 15.dp)
            Spacer(Modifier.weight(1f))
            Text(
                session.projectRoot ?: session.cwd,
                color = colors.faint,
                fontFamily = FontFamily.Monospace,
                fontSize = 10.sp,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        Text(session.title, color = colors.text, fontSize = 15.sp, fontWeight = FontWeight.SemiBold)
        Text(
            reason,
            color = if (waiting) colors.warn else colors.muted,
            fontFamily = FontFamily.Monospace,
            fontSize = 11.sp,
            lineHeight = 16.sp,
            modifier = Modifier.fillMaxWidth().background(colors.bg, RoundedCornerShape(7.dp)).padding(11.dp),
        )
        Text(statusLabel(session.status), color = accent, fontSize = 11.sp, fontWeight = FontWeight.SemiBold)
    }
}

@Composable
private fun ComposerScreen(
    api: ApiClient,
    colors: ViewerColors,
    onCreated: () -> Unit,
    onClose: () -> Unit,
) {
    var provider by rememberSaveable { mutableStateOf(ComposerProvider.AUTO) }
    var models by remember { mutableStateOf<Map<ComposerProvider, List<String>>>(emptyMap()) }
    var modelsLoaded by remember { mutableStateOf(false) }
    var model by rememberSaveable { mutableStateOf("auto") }
    var folder by rememberSaveable { mutableStateOf("") }
    var task by rememberSaveable { mutableStateOf("") }
    var submitting by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()
    val submitter = remember(api) { SessionSubmitter { request -> api.create(request) } }
    LaunchedEffect(Unit) {
        runCatching { api.models() }
            .onSuccess {
                models = it
                modelsLoaded = true
            }
            .onFailure { error = it.message ?: "Could not load models" }
    }
    fun fallbackModel(value: ComposerProvider): String =
        if (value == ComposerProvider.AUTO) "auto" else "default"
    fun discoveredModels(value: ComposerProvider): List<String> =
        models[value].orEmpty().ifEmpty { listOf(fallbackModel(value)) }
    val choices = if (modelsLoaded) {
        discoveredModels(provider)
    } else {
        listOf(model)
    }
    LaunchedEffect(modelsLoaded, models) {
        if (modelsLoaded) {
            val discovered = discoveredModels(provider)
            if (model !in discovered) model = discovered.first()
        }
    }

    Column(modifier = Modifier.fillMaxSize()) {
        Header("new session", null, colors, leading = "×", onLeading = onClose)
        Column(
            modifier = Modifier.weight(1f).padding(horizontal = 18.dp),
            verticalArrangement = Arrangement.spacedBy(18.dp),
        ) {
            FormLabel("provider", colors)
            Row(
                modifier = Modifier.fillMaxWidth().background(colors.surface, RoundedCornerShape(10.dp)).padding(4.dp),
                horizontalArrangement = Arrangement.spacedBy(2.dp),
            ) {
                ComposerProvider.entries.forEach { option ->
                    val active = option == provider
                    Box(
                        modifier = Modifier.weight(1f).clip(RoundedCornerShape(7.dp))
                            .background(if (active) colors.selbg else Color.Transparent)
                            .clickable {
                                if (provider != option) {
                                    provider = option
                                    model = if (modelsLoaded) {
                                        discoveredModels(option).first()
                                    } else {
                                        fallbackModel(option)
                                    }
                                }
                            }
                            .padding(vertical = 8.dp),
                        contentAlignment = Alignment.Center,
                    ) {
                        if (option == ComposerProvider.AUTO) {
                            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                                Text(
                                    "auto",
                                    color = if (active) colors.text else colors.muted,
                                    fontSize = 14.sp,
                                    fontWeight = if (active) FontWeight.SemiBold else FontWeight.Normal,
                                )
                                Text("router", color = colors.accent, fontFamily = FontFamily.Monospace, fontSize = 9.sp)
                            }
                        } else {
                            val backend = if (option == ComposerProvider.CLAUDE) BackendKind.CLAUDE else BackendKind.CODEX
                            Row(verticalAlignment = Alignment.CenterVertically) {
                                BackendIcon(backend, colors, size = 14.dp)
                                Spacer(Modifier.width(6.dp))
                                Text(
                                    option.wireName,
                                    color = if (active) colors.text else colors.muted,
                                    fontSize = 14.sp,
                                )
                            }
                        }
                    }
                }
            }
            FormLabel("folder", colors)
            OutlinedTextField(
                value = folder,
                onValueChange = { folder = it },
                modifier = Modifier.fillMaxWidth(),
                singleLine = true,
                placeholder = { Text("/home/brian/git/project", color = colors.faint) },
                textStyle = androidx.compose.ui.text.TextStyle(fontFamily = FontFamily.Monospace, fontSize = 12.sp, color = colors.text),
                leadingIcon = { FolderGlyph(colors.accent) },
                trailingIcon = { ChevronGlyph(colors.faint) },
            )
            FormLabel("model", colors)
            Row(
                modifier = Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                choices.forEach { choice ->
                    val active = choice == model
                    Text(
                        choice,
                        color = if (active) colors.accent else colors.muted,
                        fontFamily = FontFamily.Monospace,
                        fontSize = 12.sp,
                        modifier = Modifier.clip(RoundedCornerShape(20.dp)).background(if (active) colors.selbg else Color.Transparent)
                            .border(1.dp, if (active) colors.accent else colors.border, RoundedCornerShape(20.dp))
                            .clickable { model = choice }.padding(horizontal = 15.dp, vertical = 11.dp),
                    )
                }
            }
            FormLabel("task", colors)
            OutlinedTextField(
                value = task,
                onValueChange = { task = it },
                modifier = Modifier.fillMaxWidth().weight(1f),
                placeholder = { Text("Describe the work to start", color = colors.faint) },
                textStyle = androidx.compose.ui.text.TextStyle(fontFamily = FontFamily.Monospace, fontSize = 13.sp, color = colors.text),
            )
            error?.let { Text(it, color = colors.err, fontSize = 11.sp, fontFamily = FontFamily.Monospace) }
        }
        Column(modifier = Modifier.fillMaxWidth().padding(horizontal = 18.dp, vertical = 16.dp)) {
            LaunchButton(
                colors = colors,
                modifier = Modifier.fillMaxWidth().height(52.dp),
                enabled = task.isNotBlank() && folder.isNotBlank() && !submitting,
            ) {
                if (submitting) return@LaunchButton
                submitting = true
                scope.launch {
                    val result = submitter.submit(NewSessionRequest(provider, model, folder, task))
                        ?: return@launch
                    result.onSuccess { onCreated() }
                        .onFailure { failure ->
                            submitting = false
                            error = failure.message ?: "Could not start session"
                        }
                }
            }
            Text(
                "routes through agent-router · runs detached on remote-dev",
                color = colors.faint,
                fontSize = 10.sp,
                fontFamily = FontFamily.Monospace,
                modifier = Modifier.fillMaxWidth().padding(top = 10.dp),
                textAlign = androidx.compose.ui.text.style.TextAlign.Center,
            )
        }
    }
}

/**
 * The long-press sheet: what the detail screen used to offer, minus the read-only tail (the
 * terminal shows the live screen instead). Actions are gated on the session's advertised
 * capabilities; with none fetched yet the sheet says so rather than guessing.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun SessionActionsSheet(
    session: Session,
    capabilities: Capabilities?,
    api: ApiClient,
    colors: ViewerColors,
    onDismiss: () -> Unit,
    onOpen: () -> Unit,
    onMutated: () -> Unit,
) {
    var actionError by remember(session.key()) { mutableStateOf<String?>(null) }
    var editingName by remember(session.key()) { mutableStateOf(false) }
    var name by remember(session.key()) { mutableStateOf(session.title) }
    var busy by remember(session.key()) { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    fun mutate(failure: String, action: suspend () -> Unit) {
        scope.launch {
            busy = true
            runCatching { action() }
                .onSuccess {
                    actionError = null
                    onMutated()
                    onDismiss()
                }
                .onFailure { actionError = it.message ?: failure }
            busy = false
        }
    }
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        containerColor = colors.surface,
        contentColor = colors.text,
    ) {
        Column(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 18.dp).padding(bottom = 28.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                BackendIcon(session.backend, colors, size = 16.dp)
                Spacer(Modifier.width(9.dp))
                Text(
                    session.title,
                    color = colors.text,
                    fontSize = 16.sp,
                    fontWeight = FontWeight.SemiBold,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
            }
            Row(
                modifier = Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()),
                horizontalArrangement = Arrangement.spacedBy(7.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                StatusPill(session.status, colors)
                session.gitBranch?.let { DetailBadge(it, colors.muted, colors.bg) }
                session.prRefs.forEach { DetailBadge("#${it.id}", colors.merged, colors.bg) }
                Text(sessionAge(session.updatedAtMs), color = colors.faint, fontFamily = FontFamily.Monospace, fontSize = 11.sp)
            }
            Text(
                session.projectRoot ?: session.cwd,
                color = colors.faint,
                fontFamily = FontFamily.Monospace,
                fontSize = 10.sp,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            (session.status as? Status.NeedsInput)?.reason?.let { reason ->
                Text(reason, color = colors.warn, fontFamily = FontFamily.Monospace, fontSize = 11.sp, lineHeight = 16.sp)
            }
            Spacer(Modifier.fillMaxWidth().height(1.dp).background(colors.border))
            if (capabilities == null) {
                Text("checking what this session supports", color = colors.faint, fontFamily = FontFamily.Monospace, fontSize = 11.sp)
            } else {
                if (capabilities.attach) {
                    SheetAction("open terminal", null, colors.accent, colors, !busy, onOpen) { TerminalGlyph(colors.accent) }
                }
                if (capabilities.stop) {
                    SheetAction("stop", "interrupt the running turn", colors.err, colors, !busy, {
                        mutate("Could not stop session") { api.stop(session.id) }
                    }) { StopGlyph(colors.err) }
                }
                if (capabilities.canArchive) {
                    val detail = if (capabilities.archive == "Deletes") {
                        "stops the thread and deletes it"
                    } else {
                        "hides it from the list; unhide from the desktop"
                    }
                    SheetAction("archive", detail, colors.muted, colors, !busy, {
                        mutate("Could not archive session") { api.hide(session.id, session.hidden) }
                    }) { ArchiveGlyph(colors.muted) }
                }
                if (capabilities.rename) {
                    SheetAction("rename", null, colors.muted, colors, !busy, { editingName = !editingName }) {
                        PencilGlyph(colors.muted)
                    }
                }
                if (!capabilities.attach && !capabilities.stop && !capabilities.canArchive && !capabilities.rename) {
                    Text(
                        "nothing can be done with this session from the phone",
                        color = colors.faint,
                        fontFamily = FontFamily.Monospace,
                        fontSize = 11.sp,
                    )
                }
            }
            if (editingName) {
                Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                    OutlinedTextField(value = name, onValueChange = { name = it }, modifier = Modifier.weight(1f), singleLine = true)
                    Spacer(Modifier.width(8.dp))
                    ActionButton("save", colors.accent, colors.bg, Modifier.height(44.dp), enabled = !busy && name.isNotBlank()) {
                        mutate("Could not rename session") { api.rename(session.id, name) }
                    }
                }
            }
            actionError?.let {
                Text(it, color = colors.err, fontFamily = FontFamily.Monospace, fontSize = 10.sp)
            }
        }
    }
}

@Composable
private fun SheetAction(
    label: String,
    detail: String?,
    color: Color,
    colors: ViewerColors,
    enabled: Boolean,
    onClick: () -> Unit,
    glyph: @Composable () -> Unit,
) {
    Row(
        modifier = Modifier.fillMaxWidth().clip(RoundedCornerShape(10.dp))
            .clickable(enabled = enabled, onClick = onClick)
            .semantics { contentDescription = label }
            .padding(horizontal = 6.dp, vertical = 11.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        Box(modifier = Modifier.size(24.dp), contentAlignment = Alignment.Center) { glyph() }
        Column {
            Text(label, color = if (enabled) color else colors.faint, fontSize = 15.sp, fontWeight = FontWeight.Medium)
            if (detail != null) {
                Text(detail, color = colors.faint, fontFamily = FontFamily.Monospace, fontSize = 10.sp)
            }
        }
    }
}

@Composable
private fun StatusPill(status: Status, colors: ViewerColors) {
    val color = statusColor(status, colors)
    Row(
        modifier = Modifier.height(26.dp).background(color, CircleShape).padding(horizontal = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        Box(Modifier.size(6.dp).background(colors.bg, CircleShape))
        Text(statusLabel(status), color = colors.bg, fontSize = 11.sp, fontWeight = FontWeight.SemiBold)
    }
}

@Composable
private fun DetailBadge(label: String, color: Color, background: Color) {
    Text(
        label,
        color = color,
        fontSize = 11.sp,
        fontFamily = FontFamily.Monospace,
        modifier = Modifier.border(1.dp, color, RoundedCornerShape(20.dp)).background(background, RoundedCornerShape(20.dp))
            .padding(horizontal = 10.dp, vertical = 5.dp),
    )
}

@Composable
private fun BottomNavigation(tab: BottomTab, sessions: List<Session>, colors: ViewerColors, onSelected: (BottomTab) -> Unit) {
    // The badge is what is blocked on you, not the whole review queue.
    val waiting = normalSessionRows(sessions).count { it.status is Status.NeedsInput }
    Row(
        modifier = Modifier.fillMaxWidth().background(colors.surface).border(1.dp, colors.border).padding(bottom = 20.dp),
        horizontalArrangement = Arrangement.SpaceEvenly,
    ) {
        BottomTab.entries.forEach { item ->
            val active = item == tab
            val label = when (item) {
                BottomTab.SESSIONS -> "sessions"
                BottomTab.TRIAGE -> "triage"
                BottomTab.NEW -> "new"
            }
            val itemColor = if (active) colors.accent else colors.muted
            Box(
                modifier = Modifier.weight(1f).height(64.dp).clickable { onSelected(item) },
                contentAlignment = Alignment.Center,
            ) {
                Column(horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    BottomGlyph(item, itemColor)
                    Text(
                        label,
                        color = itemColor,
                        fontSize = 10.sp,
                        fontWeight = if (active) FontWeight.SemiBold else FontWeight.Normal,
                    )
                }
                if (item == BottomTab.TRIAGE && waiting > 0) {
                    Box(
                        modifier = Modifier.align(Alignment.TopCenter).offset(x = 12.dp, y = 8.dp)
                            .height(17.dp).widthIn(min = 17.dp).background(colors.attn, CircleShape).padding(horizontal = 4.dp),
                        contentAlignment = Alignment.Center,
                    ) {
                        Text(waiting.toString(), color = colors.bg, fontSize = 10.sp, fontWeight = FontWeight.Bold)
                    }
                }
            }
        }
    }
}

@Composable
private fun FormLabel(text: String, colors: ViewerColors) {
    Text(text, color = colors.faint, fontFamily = FontFamily.Monospace, fontSize = 10.sp, letterSpacing = 1.sp)
}

@Composable
private fun ActionButton(
    text: String,
    background: Color,
    content: Color,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    onClick: () -> Unit,
) {
    Button(
        onClick = onClick,
        enabled = enabled,
        modifier = modifier,
        shape = RoundedCornerShape(9.dp),
        colors = ButtonDefaults.buttonColors(containerColor = background, contentColor = content, disabledContainerColor = background.copy(alpha = 0.35f)),
    ) { Text(text, fontWeight = FontWeight.SemiBold, fontSize = 14.sp) }
}

@Composable
private fun LaunchButton(
    colors: ViewerColors,
    modifier: Modifier,
    enabled: Boolean,
    onClick: () -> Unit,
) {
    Button(
        onClick = onClick,
        enabled = enabled,
        modifier = modifier,
        shape = RoundedCornerShape(10.dp),
        colors = ButtonDefaults.buttonColors(
            containerColor = colors.accent,
            contentColor = colors.bg,
            disabledContainerColor = colors.accent.copy(alpha = 0.35f),
        ),
    ) {
        ArrowRightGlyph(colors.bg)
        Spacer(Modifier.width(9.dp))
        Text("kick it off", fontSize = 15.sp, fontWeight = FontWeight.SemiBold)
    }
}

@Composable
private fun BottomGlyph(tab: BottomTab, color: Color) {
    Canvas(Modifier.size(21.dp)) {
        val stroke = 1.8.dp.toPx()
        when (tab) {
            BottomTab.SESSIONS -> listOf(5f, 10.5f, 16f).forEach { y ->
                drawLine(color, Offset(2.dp.toPx(), y.dp.toPx()), Offset(19.dp.toPx(), y.dp.toPx()), stroke)
            }
            BottomTab.TRIAGE -> {
                drawLine(color, Offset(10.5.dp.toPx(), 2.dp.toPx()), Offset(10.5.dp.toPx(), 13.dp.toPx()), stroke)
                drawCircle(color, 1.2.dp.toPx(), Offset(10.5.dp.toPx(), 18.dp.toPx()))
            }
            BottomTab.NEW -> {
                drawLine(color, Offset(10.5.dp.toPx(), 3.dp.toPx()), Offset(10.5.dp.toPx(), 18.dp.toPx()), stroke)
                drawLine(color, Offset(3.dp.toPx(), 10.5.dp.toPx()), Offset(18.dp.toPx(), 10.5.dp.toPx()), stroke)
            }
        }
    }
}

@Composable
private fun BackGlyph(color: Color) {
    Canvas(Modifier.size(22.dp)) {
        val stroke = 1.9.dp.toPx()
        drawLine(color, Offset(15.dp.toPx(), 4.dp.toPx()), Offset(7.dp.toPx(), 11.dp.toPx()), stroke)
        drawLine(color, Offset(7.dp.toPx(), 11.dp.toPx()), Offset(15.dp.toPx(), 18.dp.toPx()), stroke)
    }
}

@Composable
private fun CheckGlyph(color: Color) {
    Canvas(Modifier.size(15.dp)) {
        val stroke = 2.2.dp.toPx()
        drawLine(color, Offset(1.dp.toPx(), 8.dp.toPx()), Offset(6.dp.toPx(), 13.dp.toPx()), stroke)
        drawLine(color, Offset(6.dp.toPx(), 13.dp.toPx()), Offset(14.dp.toPx(), 3.dp.toPx()), stroke)
    }
}

@Composable
private fun TerminalGlyph(color: Color) {
    Canvas(Modifier.size(17.dp)) {
        val stroke = 2.dp.toPx()
        drawLine(color, Offset(1.dp.toPx(), 3.dp.toPx()), Offset(6.dp.toPx(), 8.5.dp.toPx()), stroke)
        drawLine(color, Offset(6.dp.toPx(), 8.5.dp.toPx()), Offset(1.dp.toPx(), 14.dp.toPx()), stroke)
        drawLine(color, Offset(9.dp.toPx(), 14.dp.toPx()), Offset(16.dp.toPx(), 14.dp.toPx()), stroke)
    }
}

@Composable
private fun StopGlyph(color: Color) {
    Canvas(Modifier.size(18.dp)) {
        drawRoundRect(
            color = color,
            topLeft = Offset(3.dp.toPx(), 3.dp.toPx()),
            size = androidx.compose.ui.geometry.Size(12.dp.toPx(), 12.dp.toPx()),
            cornerRadius = androidx.compose.ui.geometry.CornerRadius(1.5.dp.toPx()),
            style = Stroke(1.8.dp.toPx()),
        )
    }
}

@Composable
private fun ArchiveGlyph(color: Color) {
    Canvas(Modifier.size(18.dp)) {
        val stroke = 1.8.dp.toPx()
        drawRoundRect(
            color = color,
            topLeft = Offset(1.dp.toPx(), 2.dp.toPx()),
            size = androidx.compose.ui.geometry.Size(16.dp.toPx(), 5.dp.toPx()),
            cornerRadius = androidx.compose.ui.geometry.CornerRadius(1.dp.toPx()),
            style = Stroke(stroke),
        )
        drawLine(color, Offset(3.dp.toPx(), 7.dp.toPx()), Offset(3.dp.toPx(), 16.dp.toPx()), stroke)
        drawLine(color, Offset(15.dp.toPx(), 7.dp.toPx()), Offset(15.dp.toPx(), 16.dp.toPx()), stroke)
        drawLine(color, Offset(3.dp.toPx(), 16.dp.toPx()), Offset(15.dp.toPx(), 16.dp.toPx()), stroke)
        drawLine(color, Offset(7.dp.toPx(), 11.dp.toPx()), Offset(11.dp.toPx(), 11.dp.toPx()), stroke)
    }
}

@Composable
private fun PencilGlyph(color: Color) {
    Canvas(Modifier.size(18.dp)) {
        val stroke = 1.8.dp.toPx()
        drawLine(color, Offset(3.dp.toPx(), 15.dp.toPx()), Offset(14.dp.toPx(), 4.dp.toPx()), stroke)
        drawLine(color, Offset(5.dp.toPx(), 17.dp.toPx()), Offset(16.dp.toPx(), 6.dp.toPx()), stroke)
        drawLine(color, Offset(14.dp.toPx(), 4.dp.toPx()), Offset(16.dp.toPx(), 6.dp.toPx()), stroke)
        drawLine(color, Offset(3.dp.toPx(), 15.dp.toPx()), Offset(5.dp.toPx(), 17.dp.toPx()), stroke)
    }
}

@Composable
private fun ArrowRightGlyph(color: Color) {
    Canvas(Modifier.size(18.dp)) {
        val stroke = 2.dp.toPx()
        drawLine(color, Offset(1.dp.toPx(), 9.dp.toPx()), Offset(17.dp.toPx(), 9.dp.toPx()), stroke)
        drawLine(color, Offset(12.dp.toPx(), 4.dp.toPx()), Offset(17.dp.toPx(), 9.dp.toPx()), stroke)
        drawLine(color, Offset(17.dp.toPx(), 9.dp.toPx()), Offset(12.dp.toPx(), 14.dp.toPx()), stroke)
    }
}

@Composable
private fun FolderGlyph(color: Color) {
    Canvas(Modifier.size(18.dp)) {
        val stroke = 1.7.dp.toPx()
        drawLine(color, Offset(1.dp.toPx(), 5.dp.toPx()), Offset(7.dp.toPx(), 5.dp.toPx()), stroke)
        drawLine(color, Offset(7.dp.toPx(), 5.dp.toPx()), Offset(9.dp.toPx(), 7.dp.toPx()), stroke)
        drawLine(color, Offset(9.dp.toPx(), 7.dp.toPx()), Offset(17.dp.toPx(), 7.dp.toPx()), stroke)
        drawLine(color, Offset(1.dp.toPx(), 5.dp.toPx()), Offset(1.dp.toPx(), 16.dp.toPx()), stroke)
        drawLine(color, Offset(1.dp.toPx(), 16.dp.toPx()), Offset(17.dp.toPx(), 16.dp.toPx()), stroke)
        drawLine(color, Offset(17.dp.toPx(), 7.dp.toPx()), Offset(17.dp.toPx(), 16.dp.toPx()), stroke)
    }
}

@Composable
private fun ChevronGlyph(color: Color) {
    Canvas(Modifier.size(16.dp)) {
        val stroke = 2.dp.toPx()
        drawLine(color, Offset(5.dp.toPx(), 2.dp.toPx()), Offset(11.dp.toPx(), 8.dp.toPx()), stroke)
        drawLine(color, Offset(11.dp.toPx(), 8.dp.toPx()), Offset(5.dp.toPx(), 14.dp.toPx()), stroke)
    }
}

@Composable
private fun StatusDot(color: Color, modifier: Modifier = Modifier) {
    Box(modifier = modifier.size(9.dp).clip(CircleShape).background(color))
}

@Composable
private fun EmptyState(text: String, colors: ViewerColors, color: Color = colors.muted) {
    Text(
        text,
        color = color,
        fontSize = 12.sp,
        modifier = Modifier.fillMaxWidth().wrapContentHeight().padding(24.dp),
        textAlign = androidx.compose.ui.text.style.TextAlign.Center,
    )
}

// Exhaustive on purpose: a new backend must not silently inherit another one's mark. Tags and
// colors match the TUI (`ui::mod::mark_for`), where Grok rides the plain text color. On the phone
// the tag is the icon's accessibility text; the drawn mark is the vendor's logo.
internal fun BackendKind.tag(): String = when (this) {
    BackendKind.CODEX -> "[cx]"
    BackendKind.CLAUDE -> "[cc]"
    BackendKind.GROK -> "[gx]"
}

internal fun BackendKind.displayName(): String = when (this) {
    BackendKind.CODEX -> "Codex"
    BackendKind.CLAUDE -> "Claude"
    BackendKind.GROK -> "Grok"
}

/** Vendor marks from lobehub/lobe-icons (MIT), see `res/drawable/ic_backend_*.xml`. */
internal fun BackendKind.iconRes(): Int = when (this) {
    BackendKind.CODEX -> R.drawable.ic_backend_codex
    BackendKind.CLAUDE -> R.drawable.ic_backend_claude
    BackendKind.GROK -> R.drawable.ic_backend_grok
}

@Composable
internal fun BackendIcon(backend: BackendKind, colors: ViewerColors, size: Dp, modifier: Modifier = Modifier) {
    Icon(
        painter = painterResource(backend.iconRes()),
        contentDescription = backend.tag(),
        tint = backendColor(backend, colors),
        modifier = modifier.size(size),
    )
}

internal fun backendColor(backend: BackendKind, colors: ViewerColors): Color = when (backend) {
    BackendKind.CODEX -> colors.cx
    BackendKind.CLAUDE -> colors.cc
    BackendKind.GROK -> colors.text
}

private fun statusLabel(status: Status): String = when (status) {
    Status.Working -> "working"
    is Status.NeedsInput -> "needs input"
    Status.Idle -> "idle"
    Status.Done -> "done"
    Status.Error -> "error"
    Status.Unknown -> "unknown"
}

private fun statusSummary(status: Status): String = when (status) {
    is Status.NeedsInput -> status.reason ?: "waiting for input"
    else -> statusLabel(status)
}

private fun statusColor(status: Status, colors: ViewerColors): Color = when (status) {
    Status.Working -> colors.accent
    is Status.NeedsInput -> colors.attn
    Status.Idle, Status.Unknown -> colors.faint
    Status.Done -> colors.ok
    Status.Error -> colors.err
}

private fun statusRank(session: Session): Int = when (session.status) {
    is Status.NeedsInput -> 0
    Status.Working -> 1
    Status.Idle, Status.Unknown -> 2
    Status.Error -> 3
    Status.Done -> 4
}

private fun sessionAge(updatedAtMs: Long): String {
    val minutes = ((System.currentTimeMillis() - updatedAtMs).coerceAtLeast(0) / 60_000).toInt()
    return when {
        minutes < 1 -> "now"
        minutes < 60 -> "${minutes}m"
        minutes < 1_440 -> "${minutes / 60}h"
        else -> "${minutes / 1_440}d"
    }
}

private fun nextTheme(current: AppTheme): AppTheme = AppTheme.entries[(current.ordinal + 1) % AppTheme.entries.size]
