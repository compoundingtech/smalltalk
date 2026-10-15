import ExpoModulesCore
import Foundation
import SocketRocket

private final class CollectionSocket: NSObject, SRWebSocketDelegate {
  let id: Int
  let socket: SRWebSocket
  let emit: ([String: Any]) -> Void
  let ended: () -> Void
  init(id: Int, request: URLRequest, protocols: [String], emit: @escaping ([String: Any]) -> Void, ended: @escaping () -> Void) {
    self.id = id
    self.socket = SRWebSocket(urlRequest: request, protocols: protocols)
    self.emit = emit
    self.ended = ended
    super.init()
    socket.delegate = self
  }
  func event(_ kind: String, _ fields: [String: Any] = [:]) {
    emit(fields.merging(["id": id, "kind": kind]) { _, next in next })
  }
  func webSocketDidOpen(_ webSocket: SRWebSocket) { event("open") }
  func webSocket(_ webSocket: SRWebSocket, didReceiveMessage message: Any) {
    guard let text = message as? String else { return }
    event("message", ["data": text])
  }
  @objc(webSocket:didReceivePingWithData:)
  func webSocket(_ webSocket: SRWebSocket, didReceivePingWith data: Data?) { event("ping") }
  func webSocket(_ webSocket: SRWebSocket, didFailWithError error: Error) {
    event("error"); ended()
  }
  func webSocket(_ webSocket: SRWebSocket, didCloseWithCode code: Int, reason: String?, wasClean: Bool) {
    event("close", ["code": code, "reason": reason ?? ""]); ended()
  }
}

public final class StCollectionsModule: Module {
  private var sockets: [Int: CollectionSocket] = [:]
  public func definition() -> ModuleDefinition {
    Name("StCollections")
    Events("socket")
    AsyncFunction("open") { (id: Int, url: URL, protocols: [String], headers: [String: String]) in
      var request = URLRequest(url: url)
      for (name, value) in headers { request.setValue(value, forHTTPHeaderField: name) }
      let held = CollectionSocket(id: id, request: request, protocols: protocols, emit: { [weak self] in self?.sendEvent("socket", $0) }, ended: { [weak self] in self?.sockets.removeValue(forKey: id) })
      self.sockets[id] = held
      held.socket.open()
    }.runOnQueue(.main)
    AsyncFunction("send") { (id: Int, text: String) in
      try self.sockets[id]?.socket.send(string: text)
    }.runOnQueue(.main)
    AsyncFunction("close") { (id: Int, code: Int) in
      guard let held = self.sockets.removeValue(forKey: id) else { return }
      held.socket.delegate = nil
      held.socket.close(withCode: code, reason: nil)
    }.runOnQueue(.main)
    OnDestroy {
      DispatchQueue.main.async {
        for held in self.sockets.values { held.socket.delegate = nil; held.socket.close() }
        self.sockets.removeAll()
      }
    }
  }
}
