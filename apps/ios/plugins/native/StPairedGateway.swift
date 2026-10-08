import Foundation

// Transport state never enters UpdatesConfig, UserDefaults, or the update database.
// SDK 57 compares the complete configured URL/headers when selecting cached updates.
final class SmalltalkPairedGatewayTransport: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
  static let shared = SmalltalkPairedGatewayTransport()
  static let identityURL = URL(string: "https://app-updates.invalid/v1/client/app-updates/manifest?app=com.compoundingtech.smalltalk&channel=daily")!
  private static let identityQueryItems = URLComponents(url: identityURL, resolvingAgainstBaseURL: false)!.queryItems
  private let lock = NSLock()
  private var origin: URLComponents?
  private var token: String?
  private var expiresAtUnixMs: Double = 0

  func setGateway(_ gateway: String) throws {
    guard let base = URLComponents(string: gateway),
      base.path.isEmpty || base.path == "/",
      base.user == nil, base.password == nil, base.query == nil, base.fragment == nil,
      let host = base.host?.lowercased(),
      base.scheme == "https" || (base.scheme == "http" && Self.isPrivateGateway(host)),
      base.url != nil
    else { throw NSError(domain: "StAppUpdates", code: 1) }
    lock.lock()
    defer { lock.unlock() }
    origin = base
    token = nil
    expiresAtUnixMs = 0
  }

  func setToken(_ value: String?, expiresAtUnixMs expiry: Double) throws {
    lock.lock()
    defer { lock.unlock() }
    token = nil
    expiresAtUnixMs = 0
    guard let value else { return }
    let remaining = expiry - Date().timeIntervalSince1970 * 1000
    guard origin != nil, !value.isEmpty, !value.unicodeScalars.contains(where: { $0.value == 10 || $0.value == 13 }),
      remaining > 0, remaining <= 15 * 60 * 1000
    else { throw NSError(domain: "StAppUpdates", code: 2) }
    token = value
    expiresAtUnixMs = expiry
  }

  func request(_ request: URLRequest, updateURL: URL) throws -> URLRequest {
    guard updateURL == Self.identityURL else { return request } // Dev/other apps are unchanged.
    lock.lock()
    defer { lock.unlock() }
    guard let base = origin, let token, expiresAtUnixMs > Date().timeIntervalSince1970 * 1000,
      let source = request.url,
      var destination = URLComponents(url: source, resolvingAgainstBaseURL: false)
    else { throw NSError(domain: "StAppUpdates", code: 4) }
    if source == Self.identityURL {
      destination.scheme = base.scheme
      destination.host = base.host
      destination.port = base.port
    } else {
      // Signed assets may only receive this token on the paired origin's fixed route.
      let prefix = "/v1/client/app-updates/assets/"
      let hash = destination.path.dropFirst(prefix.count)
      guard destination.scheme == base.scheme, destination.host == base.host,
        destination.port == base.port, destination.user == nil, destination.password == nil,
        destination.fragment == nil, destination.path.hasPrefix(prefix), hash.count == 64,
        hash.allSatisfy({ "0123456789abcdef".contains($0) }),
        destination.queryItems == Self.identityQueryItems
      else { throw NSError(domain: "StAppUpdates", code: 5) }
    }
    guard let url = destination.url else { throw NSError(domain: "StAppUpdates", code: 6) }
    var authorized = request
    authorized.url = url
    authorized.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
    return authorized
  }

  func urlSession(_ session: URLSession, task: URLSessionTask,
    willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest,
    completionHandler: @escaping (URLRequest?) -> Void) {
    // Never carry update authorization to a redirect target, including same-origin redirects.
    completionHandler(task.originalRequest?.value(forHTTPHeaderField: "Authorization") == nil ? request : nil)
  }

  private static func isPrivateGateway(_ host: String) -> Bool {
    if host.hasSuffix(".local") { return true }
    let parts = host.split(separator: ".")
    guard parts.count == 4 else { return false }
    let octets = parts.compactMap { UInt8($0) }
    guard octets.count == 4 else { return false }
    return octets[0] == 10 || (octets[0] == 100 && (64...127).contains(octets[1]))
      || (octets[0] == 172 && (16...31).contains(octets[1]))
      || (octets[0] == 192 && octets[1] == 168)
  }
}

// Compiled inside source-built EXUpdates. Config and stock selection/recovery stay build-owned.
extension EnabledAppController {
  public func setSmalltalkPairedGateway(_ gateway: String) throws {
    guard Bundle.main.bundleIdentifier == "com.compoundingtech.smalltalk",
      !config.disableAntiBrickingMeasures, config.hasEmbeddedUpdate,
      config.checkOnLaunch == .Never, config.codeSigningConfiguration != nil,
      config.updateUrl == SmalltalkPairedGatewayTransport.identityURL,
      config.requestHeaders == config.originalEmbeddedRequestHeaders
    else { throw NSError(domain: "StAppUpdates", code: 7) }
    try SmalltalkPairedGatewayTransport.shared.setGateway(gateway)
  }

  public func setSmalltalkUpdateToken(_ token: String?, expiresAtUnixMs: Double) throws {
    try SmalltalkPairedGatewayTransport.shared.setToken(token, expiresAtUnixMs: expiresAtUnixMs)
  }
}
