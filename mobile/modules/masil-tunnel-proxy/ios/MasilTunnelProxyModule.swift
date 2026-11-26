import ExpoModulesCore
import Foundation
import Network
import UIKit

private final class ListenerEntry {
  let listenerId: String
  var targetPort: Int
  var listener: NWListener?
  var localPort: UInt16 = 0

  init(listenerId: String, targetPort: Int) {
    self.listenerId = listenerId
    self.targetPort = targetPort
  }
}

public class MasilTunnelProxyModule: Module {
  private let queue = DispatchQueue(label: "dev.masil.tunnelproxy", qos: .userInitiated)
  private var listeners: [String: ListenerEntry] = [:]
  private var connections: [UInt16: NWConnection] = [:]
  private var nextStreamId: UInt16 = 1
  private var foregroundObserver: NSObjectProtocol?

  public func definition() -> ModuleDefinition {
    Name("MasilTunnelProxy")

    Events("onStreamOpen", "onStreamData", "onStreamClose", "onListenerRebound")

    OnCreate {
      self.foregroundObserver = NotificationCenter.default.addObserver(
        forName: UIApplication.willEnterForegroundNotification,
        object: nil,
        queue: nil
      ) { [weak self] _ in
        self?.rebindAllInBackground()
      }
    }

    OnDestroy {
      if let obs = self.foregroundObserver {
        NotificationCenter.default.removeObserver(obs)
        self.foregroundObserver = nil
      }
      self.queue.sync {
        for (_, conn) in self.connections {
          conn.cancel()
        }
        self.connections.removeAll()
        for (_, entry) in self.listeners {
          entry.listener?.cancel()
        }
        self.listeners.removeAll()
      }
    }

    AsyncFunction("startListener") { (targetPort: Int, promise: Promise) in
      let listenerId = UUID().uuidString
      self.queue.async {
        let entry = ListenerEntry(listenerId: listenerId, targetPort: targetPort)
        do {
          try self.bindEntry(entry) { result in
            switch result {
            case .success(let localPort):
              self.listeners[listenerId] = entry
              promise.resolve([
                "listenerId": listenerId,
                "localPort": Int(localPort),
                "targetPort": targetPort,
              ])
            case .failure(let err):
              promise.reject("E_TUNNEL_BIND", err.localizedDescription)
            }
          }
        } catch {
          promise.reject("E_TUNNEL_BIND", error.localizedDescription)
        }
      }
    }

    AsyncFunction("rebindListener") { (listenerId: String, promise: Promise) in
      self.queue.async {
        guard let entry = self.listeners[listenerId] else {
          promise.reject("E_TUNNEL_NOT_FOUND", "Unknown listenerId: \(listenerId)")
          return
        }
        entry.listener?.cancel()
        entry.listener = nil
        do {
          try self.bindEntry(entry) { result in
            switch result {
            case .success(let localPort):
              promise.resolve([
                "listenerId": listenerId,
                "localPort": Int(localPort),
                "targetPort": entry.targetPort,
              ])
            case .failure(let err):
              promise.reject("E_TUNNEL_REBIND", err.localizedDescription)
            }
          }
        } catch {
          promise.reject("E_TUNNEL_REBIND", error.localizedDescription)
        }
      }
    }

    AsyncFunction("stopListener") { (listenerId: String) in
      self.queue.async {
        if let entry = self.listeners.removeValue(forKey: listenerId) {
          entry.listener?.cancel()
          entry.listener = nil
        }
      }
    }

    Function("writeStream") { (streamId: Int, dataBase64: String) in
      guard let sid = UInt16(exactly: streamId),
            let data = Data(base64Encoded: dataBase64)
      else { return }
      self.queue.async {
        guard let conn = self.connections[sid] else { return }
        conn.send(content: data, completion: .contentProcessed { [weak self] err in
          if err != nil {
            self?.closeConnectionLocked(sid: sid, reason: "local_write_error")
          }
        })
      }
    }

    Function("closeStream") { (streamId: Int) in
      guard let sid = UInt16(exactly: streamId) else { return }
      self.queue.async {
        if let conn = self.connections.removeValue(forKey: sid) {
          conn.cancel()
        }
      }
    }
  }

  private func rebindAllInBackground() {
    self.queue.async {
      for (listenerId, entry) in self.listeners {
        if let listener = entry.listener, case .ready = listener.state {
          continue
        }
        entry.listener?.cancel()
        entry.listener = nil
        try? self.bindEntry(entry) { [weak self] res in
          if case .success(let localPort) = res {
            self?.sendEvent("onListenerRebound", [
              "listenerId": listenerId,
              "localPort": Int(localPort),
              "targetPort": entry.targetPort,
            ])
          }
        }
      }
    }
  }

  private func bindEntry(
    _ entry: ListenerEntry,
    completion: @escaping (Result<UInt16, Error>) -> Void
  ) throws {
    let params = NWParameters.tcp
    params.requiredLocalEndpoint = NWEndpoint.hostPort(
      host: NWEndpoint.Host.ipv4(.loopback),
      port: .any
    )
    let listener = try NWListener(using: params)
    var settled = false
    listener.stateUpdateHandler = { [weak self] state in
      switch state {
      case .ready:
        if !settled, let port = listener.port?.rawValue {
          settled = true
          entry.localPort = port
          completion(.success(port))
        }
      case .failed(let err):
        if !settled {
          settled = true
          completion(.failure(err))
        } else {
          self?.queue.async {
            entry.listener = nil
          }
        }
      case .cancelled:
        if !settled {
          settled = true
          completion(.failure(NSError(domain: "MasilTunnelProxy", code: -1)))
        }
      default:
        break
      }
    }
    listener.newConnectionHandler = { [weak self] conn in
      self?.acceptConnection(conn, entry: entry)
    }
    entry.listener = listener
    listener.start(queue: self.queue)
  }

  private func allocStreamIdLocked() -> UInt16 {
    for _ in 0..<65535 {
      let cand = self.nextStreamId
      self.nextStreamId = self.nextStreamId == UInt16.max ? 1 : self.nextStreamId + 1
      if cand != 0 && self.connections[cand] == nil {
        return cand
      }
    }
    return 1
  }

  private func acceptConnection(_ conn: NWConnection, entry: ListenerEntry) {
    let sid = self.allocStreamIdLocked()
    self.connections[sid] = conn
    conn.stateUpdateHandler = { [weak self] state in
      switch state {
      case .ready:
        self?.sendEvent("onStreamOpen", [
          "listenerId": entry.listenerId,
          "streamId": Int(sid),
          "targetPort": entry.targetPort,
        ])
        self?.pumpRead(sid: sid, conn: conn)
      case .failed(let err):
        self?.closeConnectionLocked(sid: sid, reason: err.localizedDescription)
      case .cancelled:
        self?.closeConnectionLocked(sid: sid, reason: "cancelled")
      default:
        break
      }
    }
    conn.start(queue: self.queue)
  }

  private func pumpRead(sid: UInt16, conn: NWConnection) {
    conn.receive(minimumIncompleteLength: 1, maximumLength: 32768) { [weak self] data, _, isComplete, error in
      guard let self = self else { return }
      if let chunk = data, !chunk.isEmpty {
        self.sendEvent("onStreamData", [
          "streamId": Int(sid),
          "dataBase64": chunk.base64EncodedString(),
        ])
      }
      if isComplete || error != nil {
        self.closeConnectionLocked(sid: sid, reason: error?.localizedDescription ?? "eof")
        return
      }
      if self.connections[sid] != nil {
        self.pumpRead(sid: sid, conn: conn)
      }
    }
  }

  private func closeConnectionLocked(sid: UInt16, reason: String) {
    guard let conn = self.connections.removeValue(forKey: sid) else { return }
    conn.cancel()
    self.sendEvent("onStreamClose", [
      "streamId": Int(sid),
      "reason": reason,
    ])
  }
}
