package dev.masil.tunnelproxy

import android.util.Base64
import expo.modules.kotlin.modules.Module
import expo.modules.kotlin.modules.ModuleDefinition
import java.io.OutputStream
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicInteger

private data class AndroidListenerEntry(
  val listenerId: String,
  val targetPort: Int,
  @Volatile var serverSocket: ServerSocket?,
  @Volatile var localPort: Int
)

private data class AndroidStreamEntry(
  val socket: Socket,
  val out: OutputStream
)

class MasilTunnelProxyModule : Module() {
  private val executor = Executors.newCachedThreadPool()
  private val listeners = ConcurrentHashMap<String, AndroidListenerEntry>()
  private val streams = ConcurrentHashMap<Int, AndroidStreamEntry>()
  private val nextStreamId = AtomicInteger(1)

  override fun definition() = ModuleDefinition {
    Name("MasilTunnelProxy")

    Events("onStreamOpen", "onStreamData", "onStreamClose", "onListenerRebound")

    OnDestroy {
      for ((_, s) in streams) {
        runCatching { s.socket.close() }
      }
      streams.clear()
      for ((_, l) in listeners) {
        runCatching { l.serverSocket?.close() }
      }
      listeners.clear()
      executor.shutdownNow()
    }

    AsyncFunction("startListener") { targetPort: Int ->
      val listenerId = UUID.randomUUID().toString()
      val server = ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))
      val localPort = server.localPort
      val entry = AndroidListenerEntry(listenerId, targetPort, server, localPort)
      listeners[listenerId] = entry
      startAcceptLoop(entry, server)
      mapOf(
        "listenerId" to listenerId,
        "localPort" to localPort,
        "targetPort" to targetPort
      )
    }

    AsyncFunction("rebindListener") { listenerId: String ->
      val entry = listeners[listenerId]
        ?: throw IllegalArgumentException("Unknown listenerId: $listenerId")
      runCatching { entry.serverSocket?.close() }
      val server = ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))
      entry.serverSocket = server
      entry.localPort = server.localPort
      startAcceptLoop(entry, server)
      mapOf(
        "listenerId" to listenerId,
        "localPort" to server.localPort,
        "targetPort" to entry.targetPort
      )
    }

    AsyncFunction("stopListener") { listenerId: String ->
      val entry = listeners.remove(listenerId)
      runCatching { entry?.serverSocket?.close() }
    }

    Function("writeStream") { streamId: Int, dataBase64: String ->
      val entry = streams[streamId] ?: return@Function
      val bytes = Base64.decode(dataBase64, Base64.DEFAULT)
      executor.execute {
        try {
          synchronized(entry.out) {
            entry.out.write(bytes)
            entry.out.flush()
          }
        } catch (_: Exception) {
          closeStreamInternal(streamId, "local_write_error")
        }
      }
    }

    Function("closeStream") { streamId: Int ->
      val entry = streams.remove(streamId)
      runCatching { entry?.socket?.close() }
    }
  }

  private fun allocStreamId(): Int {
    for (i in 0 until 65535) {
      val cand = (nextStreamId.getAndIncrement() and 0xFFFF).let { if (it == 0) 1 else it }
      if (!streams.containsKey(cand)) return cand
    }
    return 1
  }

  private fun startAcceptLoop(entry: AndroidListenerEntry, server: ServerSocket) {
    executor.execute {
      while (!server.isClosed) {
        val sock = try {
          server.accept()
        } catch (_: Exception) {
          break
        }
        val sid = allocStreamId()
        sock.tcpNoDelay = true
        val streamEntry = AndroidStreamEntry(sock, sock.getOutputStream())
        streams[sid] = streamEntry
        sendEvent(
          "onStreamOpen",
          mapOf(
            "listenerId" to entry.listenerId,
            "streamId" to sid,
            "targetPort" to entry.targetPort
          )
        )
        executor.execute {
          val buf = ByteArray(32768)
          val input = sock.getInputStream()
          try {
            while (true) {
              val n = input.read(buf)
              if (n <= 0) break
              val b64 = Base64.encodeToString(buf, 0, n, Base64.NO_WRAP)
              sendEvent(
                "onStreamData",
                mapOf(
                  "streamId" to sid,
                  "dataBase64" to b64
                )
              )
            }
          } catch (_: Exception) {
            // Closed or reset
          } finally {
            closeStreamInternal(sid, "eof")
          }
        }
      }
    }
  }

  private fun closeStreamInternal(streamId: Int, reason: String) {
    val entry = streams.remove(streamId) ?: return
    runCatching { entry.socket.close() }
    sendEvent(
      "onStreamClose",
      mapOf(
        "streamId" to streamId,
        "reason" to reason
      )
    )
  }
}
