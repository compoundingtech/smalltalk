import ExpoModulesCore
import Foundation
import Network
import Security
import UIKit
import Darwin
import StFabricRust

public class StFabricModule: Module {
  private let bridge = FabricBridge()
  private let queue = DispatchQueue(label: "smalltalk.fabric-proof")

  public func definition() -> ModuleDefinition {
    Name("StFabric")
    OnCreate { self.bridge.observePaths(on: self.queue) }
    AsyncFunction("identity") { () -> [String: Any] in
      try self.bridge.identity()
    }.runOnQueue(queue)
    AsyncFunction("dial") { (node: String, service: String, address: String?, mode: String) -> [String: Any] in
      try self.bridge.dial(node: node, service: service, address: address, mode: mode)
    }.runOnQueue(queue)
    AsyncFunction("stats") { () -> [String: Any] in
      try self.bridge.stats()
    }.runOnQueue(queue)
    AsyncFunction("stop") { () in
      self.bridge.stop()
    }.runOnQueue(queue)
    OnAppEntersBackground { self.queue.async { self.bridge.stop() } }
    OnDestroy { self.queue.async { self.bridge.destroy() } }
  }
}

private final class FabricBridge {
  // This key is separate from the device bearer and P-256 action-signing key.
  private let service = "com.compoundingtech.smalltalk.fabric-proof"
  private let account = "iroh-secret-v1"
  private let monitor = NWPathMonitor()
  private var networkType = "unknown"
  private var networkSatisfied = false

  func observePaths(on queue: DispatchQueue) {
    #if DEBUG
    monitor.pathUpdateHandler = { path in
      self.networkType = path.usesInterfaceType(.cellular) ? "cellular" : path.usesInterfaceType(.wifi) ? "wifi" : path.usesInterfaceType(.wiredEthernet) ? "wired" : "other"
      self.networkSatisfied = path.status == .satisfied
      let result = st_fabric_network_change()
      st_fabric_string_free(result)
    }
    monitor.start(queue: queue)
    #endif
  }

  func destroy() { monitor.cancel(); stop() }

  func identity() throws -> [String: Any] {
    try requireDebug()
    let secret = try loadOrCreateKey()
    return try secret.withUnsafeBytes { bytes in
      try response(st_fabric_identity(bytes.bindMemory(to: UInt8.self).baseAddress, secret.count))
    }
  }

  func dial(node: String, service: String, address: String?, mode: String) throws -> [String: Any] {
    try requireDebug()
    let secret = try loadOrCreateKey()
    var target: [String: Any] = ["node": node, "service": service, "mode": mode]
    if let address {
      target["addr"] = try JSONSerialization.jsonObject(with: Data(address.utf8))
    }
    let encoded = try JSONSerialization.data(withJSONObject: target)
    guard let json = String(data: encoded, encoding: .utf8) else {
      throw Exception(name: "FabricTarget", description: "Invalid fabric target")
    }
    return try secret.withUnsafeBytes { bytes in
      try json.withCString { request in
        try response(st_fabric_dial(bytes.bindMemory(to: UInt8.self).baseAddress, secret.count, request))
      }
    }
  }

  func stats() throws -> [String: Any] {
    try requireDebug()
    var result = try response(st_fabric_stats())
    var usage = rusage()
    getrusage(RUSAGE_SELF, &usage)
    result["cpuSeconds"] = Double(usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) + Double(usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) / 1_000_000
    result["maxResidentBytes"] = Int64(usage.ru_maxrss)
    result["networkType"] = networkType
    result["networkSatisfied"] = networkSatisfied
    result["thermalState"] = ProcessInfo.processInfo.thermalState.rawValue
    // UIKit device properties are sampled on main, without opening any window.
    DispatchQueue.main.sync {
      UIDevice.current.isBatteryMonitoringEnabled = true
      let level = UIDevice.current.batteryLevel
      result["batteryFraction"] = level >= 0 ? Double(level) : NSNull()
      result["batteryState"] = UIDevice.current.batteryState.rawValue
    }
    return result
  }

  func stop() {
    #if DEBUG
    let result = st_fabric_stop()
    st_fabric_string_free(result)
    #endif
  }

  private func requireDebug() throws {
    #if !DEBUG
    throw Exception(name: "FabricDisabled", description: "Fabric proof is available only in a development build")
    #endif
  }

  private func loadOrCreateKey() throws -> Data {
    let query: [String: Any] = [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: service,
      kSecAttrAccount as String: account,
      kSecReturnData as String: true,
      kSecMatchLimit as String: kSecMatchLimitOne,
    ]
    var found: CFTypeRef?
    let status = SecItemCopyMatching(query as CFDictionary, &found)
    if status == errSecSuccess, let secret = found as? Data, secret.count == 32 { return secret }
    guard status == errSecItemNotFound else {
      throw Exception(name: "FabricKeychain", description: "The fabric identity could not be read (\(status))")
    }
    var secret = Data(count: 32)
    let randomStatus = secret.withUnsafeMutableBytes { bytes in
      SecRandomCopyBytes(kSecRandomDefault, bytes.count, bytes.baseAddress!)
    }
    guard randomStatus == errSecSuccess else {
      throw Exception(name: "FabricIdentity", description: "The fabric identity could not be generated")
    }
    let store: [String: Any] = [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: service,
      kSecAttrAccount as String: account,
      kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
      kSecValueData as String: secret,
    ]
    let saved = SecItemAdd(store as CFDictionary, nil)
    guard saved == errSecSuccess else {
      throw Exception(name: "FabricKeychain", description: "The fabric identity could not be kept (\(saved))")
    }
    return secret
  }

  private func response(_ pointer: UnsafeMutablePointer<CChar>?) throws -> [String: Any] {
    guard let pointer else { throw Exception(name: "FabricNative", description: "Fabric did not return a result") }
    defer { st_fabric_string_free(pointer) }
    let data = Data(String(cString: pointer).utf8)
    guard let result = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
      throw Exception(name: "FabricNative", description: "Invalid fabric native result")
    }
    guard result["ok"] as? Bool == true else {
      throw Exception(name: "FabricNative", description: result["error"] as? String ?? "Fabric failed")
    }
    return result["value"] as? [String: Any] ?? [:]
  }
}
