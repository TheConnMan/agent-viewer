package com.theconnman.agentviewer

import java.io.BufferedInputStream
import java.io.ByteArrayOutputStream
import java.io.Closeable
import java.net.InetAddress
import java.net.ServerSocket
import java.net.SocketException
import java.nio.charset.StandardCharsets.UTF_8
import java.util.concurrent.CopyOnWriteArrayList

/** A one-response loopback HTTP server that records every request it is sent. */
internal class LoopbackServer(private val responseBody: String) : Closeable {
    private val socket = ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))
    val requests = CopyOnWriteArrayList<CapturedRequest>()
    val baseUrl = "http://127.0.0.1:${socket.localPort}"
    private val thread = Thread {
        try {
            while (!socket.isClosed) {
                socket.accept().use(::serve)
            }
        } catch (error: SocketException) {
            if (!socket.isClosed) throw error
        }
    }.apply {
        name = "api-contract-loopback"
        isDaemon = true
        start()
    }

    private fun serve(connection: java.net.Socket) {
        val input = BufferedInputStream(connection.getInputStream())
        val headerText = readHeaders(input)
        val lines = headerText.split("\r\n")
        val requestLine = lines.first().split(' ')
        val headers = lines.drop(1).filter(String::isNotEmpty).associate { line ->
            val separator = line.indexOf(':')
            line.substring(0, separator).lowercase() to line.substring(separator + 1).trim()
        }
        val contentLength = headers["content-length"]?.toInt() ?: 0
        val body = ByteArray(contentLength)
        var offset = 0
        while (offset < body.size) {
            val count = input.read(body, offset, body.size - offset)
            check(count >= 0) { "request ended before its declared body length" }
            offset += count
        }
        requests += CapturedRequest(
            method = requestLine[0],
            path = requestLine[1],
            contentType = headers["content-type"],
            body = String(body, UTF_8),
        )

        val response = responseBody.toByteArray(UTF_8)
        connection.getOutputStream().use { output ->
            output.write(
                (
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ${response.size}\r\n" +
                        "Connection: close\r\n\r\n"
                ).toByteArray(UTF_8),
            )
            output.write(response)
        }
    }

    private fun readHeaders(input: BufferedInputStream): String {
        val bytes = ByteArrayOutputStream()
        var tail = 0
        while (tail != 0x0d0a0d0a) {
            val byte = input.read()
            check(byte >= 0) { "request ended before its headers" }
            bytes.write(byte)
            tail = (tail shl 8 or byte) and -1
        }
        return bytes.toString(UTF_8.name())
    }

    override fun close() {
        socket.close()
        thread.join(1_000)
    }
}

internal data class CapturedRequest(
    val method: String,
    val path: String,
    val contentType: String?,
    val body: String,
)
