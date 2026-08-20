package com.theconnman.agentviewer

import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.net.URLEncoder
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.KSerializer
import kotlinx.serialization.Serializable
import kotlinx.serialization.SerialName
import kotlinx.serialization.SerializationException
import kotlinx.serialization.decodeFromString
import kotlinx.serialization.encodeToString
import kotlinx.serialization.descriptors.SerialDescriptor
import kotlinx.serialization.descriptors.buildClassSerialDescriptor
import kotlinx.serialization.encoding.Decoder
import kotlinx.serialization.encoding.Encoder
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonDecoder
import kotlinx.serialization.json.JsonEncoder
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject

object ApiConfig {
    const val DEFAULT_BASE_URL = "https://remote-dev.tail26e2ce.ts.net:8443"
}

object ApiJson {
    val json = Json
}

@Serializable
enum class BackendKind {
    @SerialName("Codex")
    CODEX,

    @SerialName("Claude")
    CLAUDE,
}

@Serializable
enum class SessionOrigin {
    @SerialName("Background")
    BACKGROUND,

    @SerialName("Interactive")
    INTERACTIVE,

    @SerialName("Exec")
    EXEC,
}

@Serializable(with = StatusSerializer::class)
sealed interface Status {
    data object Working : Status
    data class NeedsInput(val reason: String?) : Status
    data object Idle : Status
    data object Done : Status
    data object Error : Status
    data object Unknown : Status
}

object StatusSerializer : KSerializer<Status> {
    override val descriptor: SerialDescriptor = buildClassSerialDescriptor("Status")

    override fun deserialize(decoder: Decoder): Status {
        val jsonDecoder = decoder as? JsonDecoder
            ?: throw SerializationException("Status requires JSON")
        return when (val element = jsonDecoder.decodeJsonElement()) {
            is JsonPrimitive -> {
                if (!element.isString) throw SerializationException("Status must be a string or externally tagged object")
                when (element.content) {
                    "Working" -> Status.Working
                    "Idle" -> Status.Idle
                    "Done" -> Status.Done
                    "Error" -> Status.Error
                    "Unknown" -> Status.Unknown
                    else -> throw SerializationException("Unknown Status variant: ${element.content}")
                }
            }

            is JsonObject -> {
                if (element.keys != setOf("NeedsInput")) {
                    throw SerializationException("Malformed Status object")
                }
                val payload = element["NeedsInput"] as? JsonObject
                    ?: throw SerializationException("NeedsInput payload must be an object")
                if (payload.keys != setOf("reason")) {
                    throw SerializationException("NeedsInput payload must contain only reason")
                }
                val reasonElement = payload.getValue("reason")
                val reason = when (reasonElement) {
                    kotlinx.serialization.json.JsonNull -> null
                    is JsonPrimitive -> if (reasonElement.isString) {
                        reasonElement.content
                    } else {
                        throw SerializationException("NeedsInput reason must be a string or null")
                    }
                    else -> throw SerializationException("NeedsInput reason must be a string or null")
                }
                Status.NeedsInput(reason)
            }

            else -> throw SerializationException("Malformed Status value")
        }
    }

    override fun serialize(encoder: Encoder, value: Status) {
        val jsonEncoder = encoder as? JsonEncoder
            ?: throw SerializationException("Status requires JSON")
        val element = when (value) {
            Status.Working -> JsonPrimitive("Working")
            Status.Idle -> JsonPrimitive("Idle")
            Status.Done -> JsonPrimitive("Done")
            Status.Error -> JsonPrimitive("Error")
            Status.Unknown -> JsonPrimitive("Unknown")
            is Status.NeedsInput -> buildJsonObject {
                put("NeedsInput", buildJsonObject {
                    put("reason", value.reason?.let(::JsonPrimitive) ?: kotlinx.serialization.json.JsonNull)
                })
            }
        }
        jsonEncoder.encodeJsonElement(element)
    }
}

@Serializable
data class PrRef(
    val id: String,
    val href: String? = null,
)

@Serializable
data class Session(
    val backend: BackendKind,
    val id: String,
    @SerialName("short_id") val shortId: String? = null,
    val origin: SessionOrigin,
    val title: String,
    val cwd: String,
    @SerialName("git_branch") val gitBranch: String? = null,
    val status: Status,
    @SerialName("created_at_ms") val createdAtMs: Long,
    @SerialName("updated_at_ms") val updatedAtMs: Long,
    val hidden: Boolean = false,
    val companion: Boolean = false,
    val subagent: Boolean = false,
    val summary: String = "",
    val pid: Long? = null,
    @SerialName("rollout_path") val rolloutPath: String? = null,
    @SerialName("pr_refs") val prRefs: List<PrRef> = emptyList(),
    @SerialName("daemon_hosted") val daemonHosted: Boolean = false,
    @SerialName("project_root") val projectRoot: String? = null,
)

@Serializable
data class Capabilities(
    val spawn: Boolean = false,
    val attach: Boolean = false,
    val rename: Boolean = false,
    val archive: Boolean = false,
    val delete: Boolean = false,
    val stop: Boolean = false,
)

@Serializable
data class TerminalScreen(
    val type: String,
    val seq: Long,
    val rows: Int,
    val cols: Int,
    @SerialName("cursor_row") val cursorRow: Int,
    @SerialName("cursor_col") val cursorCol: Int,
    val lines: List<String>,
    val running: Boolean,
)

@Serializable
sealed interface TerminalCommand {
    @Serializable
    @SerialName("input")
    data class Input(val text: String) : TerminalCommand

    @Serializable
    @SerialName("resize")
    data class Resize(val rows: Int, val cols: Int) : TerminalCommand

    @Serializable
    @SerialName("close")
    data object Close : TerminalCommand
}

internal fun terminalWebSocketUrl(
    baseUrl: String,
    sessionId: String,
    backend: BackendKind,
): String {
    val socketBase = when {
        baseUrl.startsWith("https://") -> "wss://${baseUrl.removePrefix("https://")}"
        baseUrl.startsWith("http://") -> "ws://${baseUrl.removePrefix("http://")}"
        else -> throw IllegalArgumentException("API base URL must use HTTP or HTTPS")
    }.trimEnd('/')
    val backendName = when (backend) {
        BackendKind.CODEX -> "Codex"
        BackendKind.CLAUDE -> "Claude"
    }
    return "$socketBase/sessions/${encodePathPart(sessionId)}/terminal?backend=$backendName"
}

private fun encodePathPart(value: String): String =
    URLEncoder.encode(value, "UTF-8").replace("+", "%20")

@Serializable(with = TailEventSerializer::class)
sealed interface TailEvent {
    data class Agent(val text: String) : TailEvent
    data class User(val text: String) : TailEvent
    data class Tool(val name: String, val detail: String) : TailEvent
}

object TailEventSerializer : KSerializer<TailEvent> {
    override val descriptor: SerialDescriptor = buildClassSerialDescriptor("TailEvent")

    override fun deserialize(decoder: Decoder): TailEvent {
        val jsonDecoder = decoder as? JsonDecoder
            ?: throw SerializationException("TailEvent requires JSON")
        val objectValue = jsonDecoder.decodeJsonElement() as? JsonObject
            ?: throw SerializationException("TailEvent must be an externally tagged object")
        if (objectValue.size != 1) throw SerializationException("TailEvent must contain exactly one variant")
        return when (val variant = objectValue.keys.single()) {
            "Agent", "User" -> {
                val text = objectValue.getValue(variant) as? JsonPrimitive
                    ?: throw SerializationException("$variant payload must be a string")
                if (!text.isString) throw SerializationException("$variant payload must be a string")
                if (variant == "Agent") TailEvent.Agent(text.content) else TailEvent.User(text.content)
            }
            "Tool" -> {
                val tool = objectValue.getValue("Tool") as? JsonObject
                    ?: throw SerializationException("Tool payload must be an object")
                if (tool.keys != setOf("name", "detail")) {
                    throw SerializationException("Tool payload must contain name and detail")
                }
                fun stringField(name: String): String {
                    val value = tool.getValue(name) as? JsonPrimitive
                        ?: throw SerializationException("Tool $name must be a string")
                    if (!value.isString) throw SerializationException("Tool $name must be a string")
                    return value.content
                }
                TailEvent.Tool(name = stringField("name"), detail = stringField("detail"))
            }
            else -> throw SerializationException("Unknown TailEvent variant: $variant")
        }
    }

    override fun serialize(encoder: Encoder, value: TailEvent) {
        val jsonEncoder = encoder as? JsonEncoder
            ?: throw SerializationException("TailEvent requires JSON")
        val element = when (value) {
            is TailEvent.Agent -> buildJsonObject { put("Agent", JsonPrimitive(value.text)) }
            is TailEvent.User -> buildJsonObject { put("User", JsonPrimitive(value.text)) }
            is TailEvent.Tool -> buildJsonObject {
                put("Tool", buildJsonObject {
                    put("name", JsonPrimitive(value.name))
                    put("detail", JsonPrimitive(value.detail))
                })
            }
        }
        jsonEncoder.encodeJsonElement(element)
    }
}

@Serializable
enum class ComposerProvider(val wireName: String) {
    @SerialName("auto")
    AUTO("auto"),

    @SerialName("claude")
    CLAUDE("claude"),

    @SerialName("codex")
    CODEX("codex"),
}

@Serializable
data class NewSessionRequest(
    val provider: ComposerProvider,
    val model: String,
    @SerialName("dir") val folder: String,
    val task: String,
)

@Serializable
private data class RenameRequest(val name: String)

@Serializable
private data class ModelsResponse(
    @SerialName("Codex") val codex: List<String>,
    @SerialName("Claude") val claude: List<String>,
    val auto: List<String> = emptyList(),
)

class ApiClient(private val baseUrl: String = ApiConfig.DEFAULT_BASE_URL) {
    suspend fun sessions(): List<Session> = decode(get("/sessions"))

    suspend fun tail(sessionId: String, count: Int = 12): List<TailEvent> =
        decode(get("/sessions/${encodePathPart(sessionId)}/tail?n=$count"))

    suspend fun capabilities(sessionId: String): Capabilities =
        decode(get("/sessions/${encodePathPart(sessionId)}/caps"))

    suspend fun pulse(sessionId: String): List<Long> =
        decode(get("/sessions/${encodePathPart(sessionId)}/pulse"))

    suspend fun models(): Map<ComposerProvider, List<String>> {
        val response: ModelsResponse = decode(get("/models"))
        return mapOf(
            ComposerProvider.AUTO to response.auto,
            ComposerProvider.CLAUDE to response.claude,
            ComposerProvider.CODEX to response.codex,
        )
    }

    suspend fun create(request: NewSessionRequest) {
        request("POST", "/sessions", ApiJson.json.encodeToString(NewSessionRequest.serializer(), request))
    }

    suspend fun stop(sessionId: String) {
        request("POST", "/sessions/${encodePathPart(sessionId)}/stop", "{}")
    }

    suspend fun hide(sessionId: String, hidden: Boolean) {
        val action = if (hidden) "unhide" else "hide"
        request("POST", "/sessions/${encodePathPart(sessionId)}/$action", "{}")
    }

    suspend fun rename(sessionId: String, name: String) {
        request(
            "PATCH",
            "/sessions/${encodePathPart(sessionId)}",
            ApiJson.json.encodeToString(RenameRequest.serializer(), RenameRequest(name)),
        )
    }

    private suspend inline fun <reified T> decode(body: String): T =
        ApiJson.json.decodeFromString(body)

    private suspend fun get(path: String): String = request("GET", path)

    private suspend fun request(method: String, path: String, body: String? = null): String =
        withContext(Dispatchers.IO) {
            val connection = (URL("${baseUrl.trimEnd('/')}$path").openConnection() as HttpURLConnection)
            try {
                connection.requestMethod = method
                connection.connectTimeout = 10_000
                connection.readTimeout = 15_000
                connection.setRequestProperty("Accept", "application/json")
                if (body != null) {
                    connection.doOutput = true
                    connection.setRequestProperty("Content-Type", "application/json")
                    connection.outputStream.bufferedWriter().use { it.write(body) }
                }
                val status = connection.responseCode
                val response = (if (status in 200..299) connection.inputStream else connection.errorStream)
                    ?.bufferedReader()
                    ?.use { it.readText() }
                    .orEmpty()
                if (status !in 200..299) {
                    throw IOException("$method $path failed with HTTP $status: $response")
                }
                response
            } finally {
                connection.disconnect()
            }
        }
}
