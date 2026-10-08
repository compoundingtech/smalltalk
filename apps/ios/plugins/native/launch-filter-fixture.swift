import Foundation

// Minimal data-only SDK types let the unmodified SDK launcher/filter execute on macOS.
// The transport, launcher and manifest-filter implementations are the real source files.
public class UpdatesConfig: NSObject {
  public let updateUrl: URL
  public let requestHeaders: [String: String]
  init(_ url: URL, _ headers: [String: String]) { updateUrl = url; requestHeaders = headers }
}
public class FixtureManifest: NSObject {
  let metadata: [String: Any]
  init(_ metadata: [String: Any] = [:]) { self.metadata = metadata }
  public func getMetadata() -> [String: Any]? { metadata }
}
public class Update: NSObject {
  public let runtimeVersion = "0.1.0(2)"
  public let url: URL?
  public let requestHeaders: [String: String]?
  public let commitTime: Date
  public let manifest = FixtureManifest(["channel": "daily"])
  init(_ config: UpdatesConfig, time: TimeInterval = 1) {
    url = config.updateUrl; requestHeaders = config.requestHeaders
    commitTime = Date(timeIntervalSince1970: time)
  }
}
@objc public protocol LauncherSelectionPolicy {
  func launchableUpdate(fromUpdates updates: [Update], filters: [String: Any]?) -> Update?
}
