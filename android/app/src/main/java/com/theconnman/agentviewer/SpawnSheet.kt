package com.theconnman.agentviewer

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.launch

/**
 * The spawn sheet, opened by a long press on a folder header. The folder is the one pressed and
 * cannot be edited here; the only decisions left are who does the work and what the work is.
 * There is no model picker: the server's routing chooses, as an `auto` spawn in the TUI does.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun SpawnSheet(
    folder: String,
    api: ApiClient,
    colors: ViewerColors,
    onDismiss: () -> Unit,
    onStarted: () -> Unit,
) {
    var provider by rememberSaveable(folder) { mutableStateOf(ComposerProvider.AUTO) }
    var task by rememberSaveable(folder) { mutableStateOf("") }
    var submitting by remember(folder) { mutableStateOf(false) }
    var error by remember(folder) { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()
    val submitter = remember(api) { SessionSubmitter { request -> api.create(request) } }

    ModalBottomSheet(
        onDismissRequest = onDismiss,
        containerColor = colors.surface,
        contentColor = colors.text,
    ) {
        Column(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 18.dp).padding(bottom = 28.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                FolderGlyph(colors.accent)
                Spacer(Modifier.width(10.dp))
                Column(modifier = Modifier.weight(1f)) {
                    Text("new session", color = colors.text, fontSize = 16.sp, fontWeight = FontWeight.SemiBold)
                    Text(
                        folder,
                        color = colors.faint,
                        fontFamily = FontFamily.Monospace,
                        fontSize = 10.sp,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
            Spacer(Modifier.fillMaxWidth().height(1.dp).background(colors.border))
            ProviderChips(provider, colors, enabled = !submitting) { provider = it }
            OutlinedTextField(
                value = task,
                onValueChange = { task = it },
                modifier = Modifier.fillMaxWidth().heightIn(min = 112.dp),
                enabled = !submitting,
                placeholder = { Text("what should it do?", color = colors.faint) },
                textStyle = TextStyle(fontFamily = FontFamily.Monospace, fontSize = 13.sp, color = colors.text),
            )
            error?.let { Text(it, color = colors.err, fontSize = 11.sp, fontFamily = FontFamily.Monospace) }
            Button(
                onClick = {
                    if (submitting) return@Button
                    submitting = true
                    error = null
                    scope.launch {
                        val request = NewSessionRequest(provider, model = null, folder = folder, task = task)
                        val result = submitter.submit(request) ?: return@launch
                        result.onSuccess { onStarted() }
                            .onFailure { failure ->
                                submitting = false
                                error = failure.message ?: "Could not start session"
                            }
                    }
                },
                enabled = task.isNotBlank() && !submitting,
                modifier = Modifier.fillMaxWidth().height(50.dp),
                shape = RoundedCornerShape(10.dp),
                colors = ButtonDefaults.buttonColors(
                    containerColor = colors.accent,
                    contentColor = colors.bg,
                    disabledContainerColor = colors.accent.copy(alpha = 0.35f),
                ),
            ) {
                Text(if (submitting) "starting" else "start", fontSize = 15.sp, fontWeight = FontWeight.SemiBold)
            }
        }
    }
}

/** Who does the work: the router's choice, or a pinned CLI. Same three entries as the server. */
@Composable
private fun ProviderChips(
    selected: ComposerProvider,
    colors: ViewerColors,
    enabled: Boolean,
    onSelected: (ComposerProvider) -> Unit,
) {
    Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        ComposerProvider.entries.forEach { option ->
            val active = option == selected
            Row(
                modifier = Modifier.weight(1f).clip(RoundedCornerShape(20.dp))
                    .background(if (active) colors.selbg else Color.Transparent)
                    .border(1.dp, if (active) colors.accent else colors.border, RoundedCornerShape(20.dp))
                    .clickable(enabled = enabled) { onSelected(option) }
                    .padding(vertical = 9.dp),
                horizontalArrangement = Arrangement.Center,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                val backend = when (option) {
                    ComposerProvider.AUTO -> null
                    ComposerProvider.CLAUDE -> BackendKind.CLAUDE
                    ComposerProvider.CODEX -> BackendKind.CODEX
                }
                if (backend != null) {
                    BackendIcon(backend, colors, size = 13.dp)
                    Spacer(Modifier.width(6.dp))
                }
                Text(
                    option.wireName,
                    color = if (active) colors.accent else colors.muted,
                    fontSize = 12.sp,
                    fontWeight = if (active) FontWeight.SemiBold else FontWeight.Normal,
                )
            }
        }
    }
}

@Composable
private fun FolderGlyph(color: Color) {
    // Ink spans 1..17 on both axes of the 18 dp canvas.
    Canvas(Modifier.size(18.dp)) {
        val stroke = 1.7.dp.toPx()
        drawLine(color, Offset(1.dp.toPx(), 3.dp.toPx()), Offset(7.dp.toPx(), 3.dp.toPx()), stroke)
        drawLine(color, Offset(7.dp.toPx(), 3.dp.toPx()), Offset(9.dp.toPx(), 5.dp.toPx()), stroke)
        drawLine(color, Offset(9.dp.toPx(), 5.dp.toPx()), Offset(17.dp.toPx(), 5.dp.toPx()), stroke)
        drawLine(color, Offset(1.dp.toPx(), 3.dp.toPx()), Offset(1.dp.toPx(), 15.dp.toPx()), stroke)
        drawLine(color, Offset(1.dp.toPx(), 15.dp.toPx()), Offset(17.dp.toPx(), 15.dp.toPx()), stroke)
        drawLine(color, Offset(17.dp.toPx(), 5.dp.toPx()), Offset(17.dp.toPx(), 15.dp.toPx()), stroke)
    }
}
