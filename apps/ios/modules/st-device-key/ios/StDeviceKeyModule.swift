import CryptoKit
import ExpoModulesCore
import Foundation
import Security

// The key this phone signs its messages with (docs/st3/device-signing.md). A P-256 key made in the
// Secure Enclave on a device; on the simulator, which has none, a software key. Either way the
// private half stays here: the Keychain keeps the enclave's wrapped handle or the software key,
// readable only on this device while it is unlocked. Only the public key and signatures leave.
public class StDeviceKeyModule: Module {
  public func definition() -> ModuleDefinition {
    Name("StDeviceKey")

    // The key this phone has, or null: {key: "p256:…", storage}.
    AsyncFunction("current") { () -> [String: String]? in
      try DeviceKey.load().map { $0.described }
    }

    // A new key, replacing any earlier one: each pairing enrolls its own key.
    AsyncFunction("create") { () -> [String: String] in
      try DeviceKey.create().described
    }

    // The raw r||s ECDSA P-256 SHA-256 signature over the UTF-8 bytes of `text`, base64url.
    AsyncFunction("sign") { (text: String) -> String in
      guard let key = try DeviceKey.load() else {
        throw Exception(name: "NoKey", description: "This phone has no signing key; pair it again")
      }
      return try key.sign(Data(text.utf8))
    }

    AsyncFunction("remove") { () in
      DeviceKey.remove()
    }
  }
}

enum DeviceKey {
  case enclave(SecureEnclave.P256.Signing.PrivateKey)
  case software(P256.Signing.PrivateKey)

  private static let service = "com.compoundingtech.smalltalk.device-key"
  private static let account = "signing-key"

  var described: [String: String] {
    switch self {
    case .enclave(let key):
      return ["key": "p256:" + base64url(key.publicKey.x963Representation), "storage": "secure-enclave"]
    case .software(let key):
      return ["key": "p256:" + base64url(key.publicKey.x963Representation), "storage": "software"]
    }
  }

  func sign(_ data: Data) throws -> String {
    switch self {
    case .enclave(let key): return base64url(try key.signature(for: data).rawRepresentation)
    case .software(let key): return base64url(try key.signature(for: data).rawRepresentation)
    }
  }

  static func create() throws -> DeviceKey {
    remove()
    let key: DeviceKey
    let stored: Data
    if SecureEnclave.isAvailable {
      let made = try SecureEnclave.P256.Signing.PrivateKey()
      key = .enclave(made)
      stored = Data([1]) + made.dataRepresentation
    } else {
      let made = P256.Signing.PrivateKey()
      key = .software(made)
      stored = Data([0]) + made.rawRepresentation
    }
    let query: [String: Any] = [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: service,
      kSecAttrAccount as String: account,
      kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
      kSecValueData as String: stored,
    ]
    let status = SecItemAdd(query as CFDictionary, nil)
    guard status == errSecSuccess else {
      throw Exception(name: "KeychainFailed", description: "The signing key could not be kept (\(status))")
    }
    return key
  }

  static func load() throws -> DeviceKey? {
    let query: [String: Any] = [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: service,
      kSecAttrAccount as String: account,
      kSecReturnData as String: true,
      kSecMatchLimit as String: kSecMatchLimitOne,
    ]
    var found: CFTypeRef?
    let status = SecItemCopyMatching(query as CFDictionary, &found)
    if status == errSecItemNotFound { return nil }
    guard status == errSecSuccess, let data = found as? Data, let kind = data.first else {
      throw Exception(name: "KeychainFailed", description: "The signing key could not be read (\(status))")
    }
    let body = data.dropFirst()
    if kind == 1 {
      return .enclave(try SecureEnclave.P256.Signing.PrivateKey(dataRepresentation: body))
    }
    return .software(try P256.Signing.PrivateKey(rawRepresentation: body))
  }

  static func remove() {
    let query: [String: Any] = [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: service,
      kSecAttrAccount as String: account,
    ]
    SecItemDelete(query as CFDictionary)
  }
}

private func base64url(_ data: Data) -> String {
  data.base64EncodedString()
    .replacingOccurrences(of: "+", with: "-")
    .replacingOccurrences(of: "/", with: "_")
    .replacingOccurrences(of: "=", with: "")
}
