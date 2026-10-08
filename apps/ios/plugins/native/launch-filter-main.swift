import Foundation

let identity = SmalltalkPairedGatewayTransport.identityURL
let launchConfig = UpdatesConfig(identity, ["Authorization": "Bearer unavailable"])
let policy = LauncherSelectionPolicyFilterAware(runtimeVersion: "0.1.0(2)", config: launchConfig)
let endpoint = URL(string: "https://gateway.example" + identity.path + "?" + identity.query!)!
// Reproduce SDK 57: downloaded header X is not launch header Y, regardless of manifest filters.
let brokenRow = Update(UpdatesConfig(endpoint, ["Authorization": "Bearer X"]))
assert(policy.launchableUpdate(fromUpdates: [brokenRow], filters: nil) == nil)
assert(policy.launchableUpdate(fromUpdates: [brokenRow], filters: ["channel": "daily"]) == nil)
let sameURLWrongHeader = Update(UpdatesConfig(identity, ["Authorization": "Bearer X"]))
assert(policy.launchableUpdate(fromUpdates: [sameURLWrongHeader], filters: nil) == nil)
let sameHeaderWrongURL = Update(UpdatesConfig(endpoint, launchConfig.requestHeaders))
assert(policy.launchableUpdate(fromUpdates: [sameHeaderWrongURL], filters: nil) == nil)

let transport = SmalltalkPairedGatewayTransport()
try transport.setGateway("https://gateway.example")
let expiry = Date().timeIntervalSince1970 * 1000 + 60_000
try transport.setToken("X", expiresAtUnixMs: expiry)
var manifest = URLRequest(url: identity)
manifest.setValue("Bearer unavailable", forHTTPHeaderField: "Authorization")
manifest.setValue("signature required", forHTTPHeaderField: "expo-expect-signature")
let wireX = try transport.request(manifest, updateURL: identity)
assert(wireX.url == endpoint)
assert(wireX.value(forHTTPHeaderField: "Authorization") == "Bearer X")
assert(wireX.value(forHTTPHeaderField: "expo-expect-signature") == "signature required")
// Expo creates/persists the row from config, never from the authorized URLRequest.
let downloadedRow = Update(launchConfig, time: 2)
let embeddedRow = Update(launchConfig)
try transport.setToken(nil, expiresAtUnixMs: 0)
let coldTransport = SmalltalkPairedGatewayTransport() // No Keychain hydration/network needed to launch.
assert(policy.launchableUpdate(fromUpdates: [embeddedRow, downloadedRow], filters: nil) === downloadedRow)
try transport.setToken("Y", expiresAtUnixMs: expiry)
let wireY = try transport.request(manifest, updateURL: identity)
assert(wireY.value(forHTTPHeaderField: "Authorization") == "Bearer Y")
assert(policy.launchableUpdate(fromUpdates: [downloadedRow], filters: ["channel": "daily"]) === downloadedRow)
assert(policy.launchableUpdate(fromUpdates: [downloadedRow], filters: ["channel": "other"]) == nil)
assert(downloadedRow.url == identity && downloadedRow.requestHeaders == launchConfig.requestHeaders)

let assetPath = "/v1/client/app-updates/assets/" + String(repeating: "a", count: 64) + "?" + identity.query!
let asset = URLRequest(url: URL(string: "https://gateway.example" + assetPath)!)
let authorizedAsset = try transport.request(asset, updateURL: identity)
assert(authorizedAsset.value(forHTTPHeaderField: "Authorization") == "Bearer Y")
func refuses(file: StaticString = #file, line: UInt = #line, _ operation: () throws -> Void) {
  do { try operation(); assertionFailure("Expected transport refusal", file: file, line: line) } catch {}
}
refuses { _ = try coldTransport.request(manifest, updateURL: identity) }
refuses { _ = try transport.request(URLRequest(url: URL(string: "https://other.example" + assetPath)!), updateURL: identity) }
refuses { _ = try transport.request(URLRequest(url: URL(string: "https://gateway.example/not-an-update")!), updateURL: identity) }
refuses { try transport.setToken("expired", expiresAtUnixMs: 0) }
refuses { _ = try transport.request(asset, updateURL: identity) }
refuses { try transport.setToken("too-long", expiresAtUnixMs: Date().timeIntervalSince1970 * 1000 + 16 * 60 * 1000) }
refuses { try transport.setToken("header\r\ninjection", expiresAtUnixMs: expiry) }
try transport.setToken("Y", expiresAtUnixMs: expiry)
try transport.setGateway("https://replacement.example")
refuses { _ = try transport.request(manifest, updateURL: identity) }
refuses { try transport.setGateway("https://gateway.example/prefix") }
let otherAppRequest = try transport.request(asset, updateURL: URL(string: "https://other-app.example")!)
assert(otherAppRequest.url == asset.url)
let session = URLSession(configuration: .ephemeral)
let task = session.dataTask(with: wireX)
let redirect = HTTPURLResponse(url: endpoint, statusCode: 302, httpVersion: nil, headerFields: nil)!
var redirectAccepted = true
transport.urlSession(session, task: task, willPerformHTTPRedirection: redirect, newRequest: asset) { redirectAccepted = $0 != nil }
assert(!redirectAccepted)
session.invalidateAndCancel()
print("Native transport and unmodified SDK 57 launch-filter regression passed")
