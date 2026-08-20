package com.theconnman.agentviewer

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
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
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
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
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

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
private enum class SessionFilter { ALL, PROJECT, STATE, HIDDEN }

@Composable
fun AgentViewerApp() {
    var appTheme by remember { mutableStateOf(AppTheme.default) }
    var tab by remember { mutableStateOf(BottomTab.SESSIONS) }
    var selectedSession by remember { mutableStateOf<Session?>(null) }
    var sessions by remember { mutableStateOf<List<Session>>(emptyList()) }
    var loadError by remember { mutableStateOf<String?>(null) }
    val api = remember { ApiClient() }
    val foreground = rememberForeground()
    val scope = rememberCoroutineScope()
    val colors = appTheme.colors

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
    val scheme = if (appTheme == AppTheme.PAPER_LIGHT) {
        lightColorScheme(primary = colors.accent, surface = colors.surface, onSurface = colors.text)
    } else {
        darkColorScheme(primary = colors.accent, surface = colors.surface, onSurface = colors.text)
    }

    MaterialTheme(colorScheme = scheme) {
        Surface(modifier = Modifier.fillMaxSize(), color = colors.bg, contentColor = colors.text) {
            selectedSession?.let { session ->
                SessionDetailScreen(
                    session = session,
                    api = api,
                    colors = colors,
                    onBack = { selectedSession = null },
                    onMutated = refresh,
                )
            } ?: Column(modifier = Modifier.fillMaxSize()) {
                Box(modifier = Modifier.weight(1f)) {
                    when (tab) {
                        BottomTab.SESSIONS -> SessionListScreen(
                            sessions = sessions,
                            error = loadError,
                            colors = colors,
                            theme = appTheme,
                            onTheme = { appTheme = nextTheme(appTheme) },
                            onSelected = { selectedSession = it },
                        )

                        BottomTab.TRIAGE -> TriageScreen(
                            sessions = sessions,
                            api = api,
                            colors = colors,
                            onSelected = { selectedSession = it },
                            onMutated = refresh,
                        )

                        BottomTab.NEW -> ComposerScreen(
                            api = api,
                            colors = colors,
                            onCreated = {
                                tab = BottomTab.SESSIONS
                                refresh()
                            },
                        )
                    }
                }
                BottomNavigation(tab = tab, sessions = sessions, colors = colors) { tab = it }
            }
        }
    }
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
    theme: AppTheme,
    onTheme: () -> Unit,
    onSelected: (Session) -> Unit,
) {
    var filter by remember { mutableStateOf(SessionFilter.ALL) }
    val filtered = sessions
        .filter { if (filter == SessionFilter.HIDDEN) it.hidden else !it.hidden }
        .let { rows -> if (filter == SessionFilter.STATE) rows.sortedBy(::statusRank) else rows }
    val groups = filtered.groupBy { it.projectRoot ?: it.cwd }.toSortedMap()
    val waiting = sessions.count { it.status is Status.NeedsInput && !it.hidden }
    val working = sessions.count { it.status == Status.Working && !it.hidden }
    val done = sessions.count { it.status == Status.Done && !it.hidden }

    Column(modifier = Modifier.fillMaxSize()) {
        Header(
            title = "sessions",
            subtitle = "remote-dev · ${sessions.size} total",
            colors = colors,
            trailing = "${theme.displayName} ›",
            onTrailing = onTheme,
        )
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
            groups.forEach { (project, rows) ->
                item(key = "project-$project") { ProjectHeader(project, rows.size, colors) }
                items(rows, key = { "${it.backend}-${it.id}" }) { session ->
                    SessionRow(session, colors) { onSelected(session) }
                }
            }
            if (groups.isEmpty()) {
                item { EmptyState("no sessions here", colors) }
            }
        }
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
        listOf(
            SessionFilter.ALL to "all",
            SessionFilter.PROJECT to "by project",
            SessionFilter.STATE to "by state",
            SessionFilter.HIDDEN to "hidden",
        ).forEach { (filter, label) ->
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
private fun ProjectHeader(project: String, count: Int, colors: ViewerColors) {
    Row(
        modifier = Modifier.fillMaxWidth().background(colors.surface).border(1.dp, colors.border)
            .padding(horizontal = 18.dp, vertical = 7.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text("⌄", color = colors.faint, fontSize = 14.sp)
        Spacer(Modifier.width(8.dp))
        Text(
            project,
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

@Composable
private fun SessionRow(session: Session, colors: ViewerColors, onClick: () -> Unit) {
    Row(
        modifier = Modifier.fillMaxWidth().background(if (session.status is Status.NeedsInput) colors.selbg else colors.bg)
            .clickable(onClick = onClick).padding(horizontal = 18.dp, vertical = 13.dp),
        horizontalArrangement = Arrangement.spacedBy(11.dp),
        verticalAlignment = Alignment.Top,
    ) {
        StatusDot(statusColor(session.status, colors), Modifier.padding(top = 5.dp))
        Column(modifier = Modifier.weight(1f)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    session.backend.tag(),
                    color = backendColor(session.backend, colors),
                    fontFamily = FontFamily.Monospace,
                    fontSize = 11.sp,
                    fontWeight = FontWeight.Medium,
                )
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
    api: ApiClient,
    colors: ViewerColors,
    onSelected: (Session) -> Unit,
    onMutated: () -> Unit,
) {
    val queue = sessions.filter { it.status is Status.NeedsInput && !it.hidden }.sortedBy { it.updatedAtMs }
    Column(modifier = Modifier.fillMaxSize()) {
        Header("triage", "longest wait first", colors, leading = "‹")
        LazyColumn(
            modifier = Modifier.weight(1f),
            verticalArrangement = Arrangement.spacedBy(10.dp),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(horizontal = 16.dp, vertical = 6.dp),
        ) {
            items(queue, key = { "triage-${it.backend}-${it.id}" }) { session ->
                TriageCard(session, api, colors, onSelected, onMutated)
            }
            item { EmptyState("nothing else is waiting on you", colors, colors.ok) }
        }
    }
}

@Composable
private fun TriageCard(
    session: Session,
    api: ApiClient,
    colors: ViewerColors,
    onSelected: (Session) -> Unit,
    onMutated: () -> Unit,
) {
    var capabilities by remember(session.id) { mutableStateOf<Capabilities?>(null) }
    val scope = rememberCoroutineScope()
    LaunchedEffect(session.id) {
        capabilities = runCatching { api.capabilities(session.id) }.getOrNull()
    }
    val reason = (session.status as? Status.NeedsInput)?.reason ?: "Waiting for your input"
    Column(
        modifier = Modifier.fillMaxWidth().background(colors.surface, RoundedCornerShape(12.dp))
            .border(1.dp, colors.attn, RoundedCornerShape(12.dp)).padding(13.dp),
        verticalArrangement = Arrangement.spacedBy(9.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(sessionAge(session.updatedAtMs), color = colors.attn, fontSize = 19.sp, fontWeight = FontWeight.Bold)
            Spacer(Modifier.width(9.dp))
            Text(session.backend.tag(), color = backendColor(session.backend, colors), fontFamily = FontFamily.Monospace, fontSize = 11.sp)
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
            color = colors.warn,
            fontFamily = FontFamily.Monospace,
            fontSize = 11.sp,
            lineHeight = 16.sp,
            modifier = Modifier.fillMaxWidth().background(colors.bg, RoundedCornerShape(7.dp)).padding(11.dp),
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            ActionButton("view", colors.accent, colors.bg, Modifier.weight(1f)) { onSelected(session) }
            if (capabilities?.stop == true) {
                OutlinedButton(
                    onClick = {
                        scope.launch {
                            if (runCatching { api.stop(session.id) }.isSuccess) onMutated()
                        }
                    },
                    modifier = Modifier.height(44.dp),
                ) { Text("stop", color = colors.err) }
            }
        }
    }
}

@Composable
private fun ComposerScreen(api: ApiClient, colors: ViewerColors, onCreated: () -> Unit) {
    var provider by remember { mutableStateOf(ComposerProvider.AUTO) }
    var models by remember { mutableStateOf<Map<ComposerProvider, List<String>>>(emptyMap()) }
    var model by remember { mutableStateOf("auto") }
    var folder by remember { mutableStateOf("") }
    var task by remember { mutableStateOf("") }
    var error by remember { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()
    LaunchedEffect(Unit) {
        models = runCatching { api.models() }.getOrDefault(emptyMap())
    }
    val choices = models[provider].orEmpty().ifEmpty {
        listOf(if (provider == ComposerProvider.AUTO) "auto" else "default")
    }
    LaunchedEffect(provider, models) {
        if (model !in choices) model = choices.first()
    }

    Column(modifier = Modifier.fillMaxSize()) {
        Header("new session", null, colors, leading = "×")
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
                    Text(
                        text = option.wireName,
                        color = if (active) colors.text else colors.muted,
                        fontSize = 14.sp,
                        fontWeight = if (active) FontWeight.SemiBold else FontWeight.Normal,
                        modifier = Modifier.weight(1f).clip(RoundedCornerShape(7.dp))
                            .background(if (active) colors.selbg else Color.Transparent)
                            .clickable { provider = option }.padding(vertical = 13.dp),
                        textAlign = androidx.compose.ui.text.style.TextAlign.Center,
                    )
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
            ActionButton(
                "kick it off",
                colors.accent,
                colors.bg,
                Modifier.fillMaxWidth().height(52.dp),
                enabled = task.isNotBlank() && folder.isNotBlank(),
            ) {
                scope.launch {
                    runCatching { api.create(NewSessionRequest(provider, model, folder, task)) }
                        .onSuccess { onCreated() }
                        .onFailure { error = it.message ?: "Could not start session" }
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

@Composable
private fun SessionDetailScreen(
    session: Session,
    api: ApiClient,
    colors: ViewerColors,
    onBack: () -> Unit,
    onMutated: () -> Unit,
) {
    var capabilities by remember(session.id) { mutableStateOf<Capabilities?>(null) }
    var tail by remember(session.id) { mutableStateOf<List<TailEvent>>(emptyList()) }
    var error by remember(session.id) { mutableStateOf<String?>(null) }
    var editingName by remember(session.id) { mutableStateOf(false) }
    var name by remember(session.id) { mutableStateOf(session.title) }
    val scope = rememberCoroutineScope()
    LaunchedEffect(session.id) {
        runCatching { api.capabilities(session.id) }.onSuccess { capabilities = it }
        runCatching { api.tail(session.id) }.onSuccess { tail = it }.onFailure { error = it.message }
    }
    Column(modifier = Modifier.fillMaxSize()) {
        Header(
            title = session.title,
            subtitle = session.backend.tag(),
            colors = colors,
            leading = "‹",
            onLeading = onBack,
        )
        Row(
            modifier = Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()).padding(horizontal = 18.dp, vertical = 4.dp),
            horizontalArrangement = Arrangement.spacedBy(7.dp),
        ) {
            DetailBadge(statusLabel(session.status), statusColor(session.status, colors), colors.bg)
            session.gitBranch?.let { DetailBadge(it, colors.muted, colors.bg) }
            session.prRefs.forEach { DetailBadge("#${it.id}", colors.merged, colors.bg) }
            Text(sessionAge(session.updatedAtMs), color = colors.faint, fontFamily = FontFamily.Monospace, fontSize = 11.sp, modifier = Modifier.padding(top = 7.dp))
        }
        Row(
            modifier = Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()).padding(horizontal = 18.dp, vertical = 10.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            if (capabilities?.stop == true) {
                ActionButton("stop", colors.err, colors.bg, Modifier.height(44.dp)) {
                    scope.launch {
                        if (runCatching { api.stop(session.id) }.isSuccess) onMutated()
                    }
                }
            }
            if (capabilities?.archive == true) {
                OutlinedButton(
                    onClick = {
                        scope.launch {
                            if (runCatching { api.hide(session.id, session.hidden) }.isSuccess) onMutated()
                        }
                    },
                    modifier = Modifier.height(44.dp),
                ) { Text(if (session.hidden) "unhide" else "hide", color = colors.muted) }
            }
            if (capabilities?.rename == true) {
                OutlinedButton(onClick = { editingName = !editingName }, modifier = Modifier.height(44.dp)) {
                    Text("rename", color = colors.muted)
                }
            }
        }
        if (editingName) {
            Row(modifier = Modifier.fillMaxWidth().padding(horizontal = 18.dp), verticalAlignment = Alignment.CenterVertically) {
                OutlinedTextField(value = name, onValueChange = { name = it }, modifier = Modifier.weight(1f), singleLine = true)
                Spacer(Modifier.width(8.dp))
                ActionButton("save", colors.accent, colors.bg, Modifier.height(44.dp)) {
                    scope.launch {
                        runCatching { api.rename(session.id, name) }
                            .onSuccess {
                                editingName = false
                                onMutated()
                            }
                            .onFailure { error = it.message }
                    }
                }
            }
        }
        Text(
            "recent turns",
            color = colors.faint,
            fontFamily = FontFamily.Monospace,
            fontSize = 10.sp,
            letterSpacing = 1.sp,
            modifier = Modifier.padding(horizontal = 18.dp, vertical = 10.dp),
        )
        val reason = (session.status as? Status.NeedsInput)?.reason
        if (reason != null) {
            Text(
                reason,
                color = colors.text,
                fontSize = 13.sp,
                fontWeight = FontWeight.Medium,
                modifier = Modifier.fillMaxWidth().padding(horizontal = 18.dp).background(colors.surface, RoundedCornerShape(9.dp))
                    .border(1.dp, colors.attn, RoundedCornerShape(9.dp)).padding(12.dp),
            )
        }
        LazyColumn(
            modifier = Modifier.weight(1f),
            verticalArrangement = Arrangement.spacedBy(11.dp),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(horizontal = 18.dp, vertical = 8.dp),
        ) {
            items(tail) { event -> TailRow(event, colors) }
            if (tail.isEmpty()) item { EmptyState(error ?: "no recent turns", colors) }
        }
    }
}

@Composable
private fun TailRow(event: TailEvent, colors: ViewerColors) {
    when (event) {
        is TailEvent.Agent -> Text(event.text, color = colors.text, fontSize = 13.sp, lineHeight = 20.sp)
        is TailEvent.User -> Column(
            modifier = Modifier.fillMaxWidth().background(colors.selbg, RoundedCornerShape(9.dp)).padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(3.dp),
        ) {
            Text("you", color = colors.muted, fontFamily = FontFamily.Monospace, fontSize = 9.sp, letterSpacing = 1.sp)
            Text(event.text, color = colors.text, fontSize = 13.sp, lineHeight = 19.sp)
        }

        is TailEvent.Tool -> Row {
            Text("|", color = colors.faint, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
            Spacer(Modifier.width(8.dp))
            Text(event.name, color = colors.cx, fontFamily = FontFamily.Monospace, fontSize = 11.sp, fontWeight = FontWeight.Medium)
            Spacer(Modifier.width(8.dp))
            Text(event.detail, color = colors.faint, fontFamily = FontFamily.Monospace, fontSize = 11.sp, maxLines = 1, overflow = TextOverflow.Ellipsis)
        }
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
    val waiting = sessions.count { it.status is Status.NeedsInput && !it.hidden }
    Row(
        modifier = Modifier.fillMaxWidth().background(colors.surface).border(1.dp, colors.border).padding(bottom = 14.dp),
        horizontalArrangement = Arrangement.SpaceEvenly,
    ) {
        BottomTab.entries.forEach { item ->
            val active = item == tab
            val label = when (item) {
                BottomTab.SESSIONS -> "sessions"
                BottomTab.TRIAGE -> if (waiting > 0) "triage $waiting" else "triage"
                BottomTab.NEW -> "new"
            }
            Text(
                label,
                color = if (active) colors.accent else colors.muted,
                fontSize = 11.sp,
                fontWeight = if (active) FontWeight.SemiBold else FontWeight.Normal,
                modifier = Modifier.weight(1f).clickable { onSelected(item) }.padding(top = 18.dp, bottom = 10.dp),
                textAlign = androidx.compose.ui.text.style.TextAlign.Center,
            )
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

private fun BackendKind.tag(): String = if (this == BackendKind.CLAUDE) "[cc]" else "[cx]"
private fun backendColor(backend: BackendKind, colors: ViewerColors): Color = if (backend == BackendKind.CLAUDE) colors.cc else colors.cx

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
