import Foundation

// Compiled inside EXUpdates, not StAppUpdates: SDK 57 exposes no safe post-start URL setter.
// Unlike its preview override, this changes only the in-memory download endpoint. The scope,
// embedded bundle, signature certificate, runtime selection and recovery remain build-owned.
extension EnabledAppController {
  public func setSmalltalkPairedGateway(_ gateway: String) throws {
    guard Bundle.main.bundleIdentifier == "com.compoundingtech.smalltalk",
      !config.disableAntiBrickingMeasures,
      config.hasEmbeddedUpdate,
      config.checkOnLaunch == .Never,
      config.codeSigningConfiguration != nil,
      let base = URLComponents(string: gateway),
      base.path.isEmpty || base.path == "/",
      base.user == nil, base.password == nil, base.query == nil, base.fragment == nil,
      let host = base.host?.lowercased(),
      base.scheme == "https" || (base.scheme == "http" && Self.isPrivateGateway(host))
    else { throw NSError(domain: "StAppUpdates", code: 1) }
    var endpoint = base
    endpoint.path = "/v1/client/app-updates/manifest"
    endpoint.queryItems = [
      URLQueryItem(name: "app", value: "com.compoundingtech.smalltalk"),
      URLQueryItem(name: "channel", value: "daily")
    ]
    guard let url = endpoint.url else { throw NSError(domain: "StAppUpdates", code: 2) }
    // Header overrides still use Expo's stock setter. No URL is persisted in UserDefaults.
    // A fixed scope keeps signed cached updates usable offline on the next process launch.
    config = try UpdatesConfig.configWithExpoPlist(mergingOtherDictionary: [
      UpdatesConfig.EXUpdatesConfigUpdateUrlKey: url.absoluteString,
      UpdatesConfig.EXUpdatesConfigScopeKeyKey: config.scopeKey
    ])
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
