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
      let handle = UUID().uuidString
      var info = try DeviceKey.create(handle: handle).described
      info["handle"] = handle
      return info
    }

    // The raw r||s ECDSA P-256 SHA-256 signature over the UTF-8 bytes of `text`, base64url.
    AsyncFunction("sign") { (text: String, handle: String?) -> String in
      guard let key = try DeviceKey.load(handle: handle) else {
        throw Exception(name: "NoKey", description: "This phone has no signing key; pair it again")
      }
      return try key.sign(Data(text.utf8))
    }

    AsyncFunction("remove") { (handle: String?) in
      DeviceKey.remove(handle: handle)
    }

    AsyncFunction("verify") { (key: String, text: String, signature: String) -> Bool in
      guard let signed = unbase64url(signature), signed.count == 64 else { return false }
      let bytes = Data(text.utf8)
      do {
        if key.hasPrefix("p256:") {
          guard let raw = unbase64url(String(key.dropFirst(5))) else { return false }
          let publicKey = try P256.Signing.PublicKey(x963Representation: raw)
          return publicKey.isValidSignature(try P256.Signing.ECDSASignature(rawRepresentation: signed), for: bytes)
        }
        guard let raw = unbase64url(key) else { return false }
        return try Curve25519.Signing.PublicKey(rawRepresentation: raw).isValidSignature(signed, for: bytes)
      } catch { return false }
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

  static func create(handle: String) throws -> DeviceKey {
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
      kSecAttrAccount as String: handle,
      kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
      kSecValueData as String: stored,
    ]
    let status = SecItemAdd(query as CFDictionary, nil)
    guard status == errSecSuccess else {
      throw Exception(name: "KeychainFailed", description: "The signing key could not be kept (\(status))")
    }
    return key
  }

  static func load(handle: String? = nil) throws -> DeviceKey? {
    let query: [String: Any] = [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: service,
      kSecAttrAccount as String: handle ?? account,
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

  static func remove(handle: String? = nil) {
    let query: [String: Any] = [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: service,
      kSecAttrAccount as String: handle ?? account,
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

private func unbase64url(_ text: String) -> Data? {
  let raw = text.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
  guard let data = Data(base64Encoded: raw + String(repeating: "=", count: (4 - raw.count % 4) % 4)), base64url(data) == text else { return nil }
  return data
}
