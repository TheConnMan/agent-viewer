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
import kotlinx.serialization.decodeFromString
import kotlinx.serialization.encodeToString
import kotlinx.serialization.descriptors.SerialDescriptor
import kotlinx.serialization.descriptors.buildClassSerialDescriptor
import kotlinx.serialization.encoding.Decoder
import kotlinx.serialization.encoding.Encoder
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonDecoder
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonEncoder
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive

object ApiConfig {
    const val DEFAULT_BASE_URL = "https://remote-dev.tail26e2ce.ts.net:8443"
}

object ApiJson {
    val json = Json {
        ignoreUnknownKeys = true
        explicitNulls = false
    }
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
        val jsonDecoder = decoder as? JsonDecoder ?: error("Status requires JSON")
        return when (val element = jsonDecoder.decodeJsonElement()) {
            is JsonPrimitive -> when (element.content) {
                "Working" -> Status.Working
                "Idle" -> Status.Idle
                "Done" -> Status.Done
                "Error" -> Status.Error
                "Unknown" -> Status.Unknown
                else -> Status.Unknown
            }

            is JsonObject -> {
                val reason = element["NeedsInput"]
                    ?.jsonObject
                    ?.get("reason")
                    ?.jsonPrimitive
                    ?.contentOrNull
                Status.NeedsInput(reason)
            }

            else -> Status.Unknown
        }
    }

    override fun serialize(encoder: Encoder, value: Status) {
        val jsonEncoder = encoder as? JsonEncoder ?: error("Status requires JSON")
        val element = when (value) {
            Status.Working -> JsonPrimitive("Working")
            Status.Idle -> JsonPrimitive("Idle")
            Status.Done -> JsonPrimitive("Done")
            Status.Error -> JsonPrimitive("Error")
            Status.Unknown -> JsonPrimitive("Unknown")
            is Status.NeedsInput -> buildJsonObject {
                put("NeedsInput", buildJsonObject {
                    value.reason?.let { put("reason", JsonPrimitive(it)) }
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

@Serializable(with = TailEventSerializer::class)
sealed interface TailEvent {
    data class Agent(val text: String) : TailEvent
    data class User(val text: String) : TailEvent
    data class Tool(val name: String, val detail: String) : TailEvent
}

object TailEventSerializer : KSerializer<TailEvent> {
    override val descriptor: SerialDescriptor = buildClassSerialDescriptor("TailEvent")

    override fun deserialize(decoder: Decoder): TailEvent {
        val jsonDecoder = decoder as? JsonDecoder ?: error("TailEvent requires JSON")
        val objectValue = jsonDecoder.decodeJsonElement().jsonObject
        objectValue["Agent"]?.jsonPrimitive?.contentOrNull?.let(TailEvent::Agent)?.let { return it }
        objectValue["User"]?.jsonPrimitive?.contentOrNull?.let(TailEvent::User)?.let { return it }
        val tool = objectValue["Tool"]?.jsonObject
        return TailEvent.Tool(
            name = tool?.get("name")?.jsonPrimitive?.contentOrNull.orEmpty(),
            detail = tool?.get("detail")?.jsonPrimitive?.contentOrNull.orEmpty(),
        )
    }

    override fun serialize(encoder: Encoder, value: TailEvent) {
        val jsonEncoder = encoder as? JsonEncoder ?: error("TailEvent requires JSON")
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

class ApiClient(private val baseUrl: String = ApiConfig.DEFAULT_BASE_URL) {
    suspend fun sessions(): List<Session> = decode(get("/sessions"))

    suspend fun tail(sessionId: String, count: Int = 12): List<TailEvent> =
        decode(get("/sessions/${pathPart(sessionId)}/tail?n=$count"))

    suspend fun capabilities(sessionId: String): Capabilities =
        decode(get("/sessions/${pathPart(sessionId)}/caps"))

    suspend fun pulse(sessionId: String): List<Long> =
        decode(get("/sessions/${pathPart(sessionId)}/pulse"))

    suspend fun models(): Map<ComposerProvider, List<String>> {
        val response = ApiJson.json.parseToJsonElement(get("/models")).jsonObject
        fun values(key: String): List<String> = response[key]
            ?.jsonArray
            ?.mapNotNull { it.jsonPrimitive.contentOrNull }
            .orEmpty()
        val codex = values("Codex")
        val claude = values("Claude")
        val auto = if (codex.isNotEmpty() || claude.isNotEmpty()) listOf("auto") else emptyList()
        return mapOf(
            ComposerProvider.AUTO to auto,
            ComposerProvider.CLAUDE to claude,
            ComposerProvider.CODEX to codex,
        )
    }

    suspend fun create(request: NewSessionRequest) {
        request("POST", "/sessions", ApiJson.json.encodeToString(NewSessionRequest.serializer(), request))
    }

    suspend fun stop(sessionId: String) {
        request("POST", "/sessions/${pathPart(sessionId)}/stop")
    }

    suspend fun hide(sessionId: String, hidden: Boolean) {
        val action = if (hidden) "unhide" else "hide"
        request("POST", "/sessions/${pathPart(sessionId)}/$action")
    }

    suspend fun rename(sessionId: String, name: String) {
        request(
            "PATCH",
            "/sessions/${pathPart(sessionId)}",
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

    private fun pathPart(value: String): String = URLEncoder.encode(value, "UTF-8")
}
