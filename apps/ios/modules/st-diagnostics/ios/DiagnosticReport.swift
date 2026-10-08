import CryptoKit
import Foundation

struct DiagnosticLaunchContext: Codable {
  let launch_id: String
  let started_at_unix_ms: UInt64
  let app_version: String
  let native_build: String
  let os_version: String

  static func current() -> DiagnosticLaunchContext {
    let os = ProcessInfo.processInfo.operatingSystemVersion
    return DiagnosticLaunchContext(
      launch_id: UUID().uuidString.lowercased(),
      started_at_unix_ms: diagnosticMilliseconds(Date()),
      app_version: diagnosticVersion(Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String),
      native_build: diagnosticVersion(Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String),
      os_version: "\(os.majorVersion).\(os.minorVersion).\(os.patchVersion)"
    )
  }

  var dictionary: [String: Any] {
    ["launch_id": launch_id, "started_at_unix_ms": started_at_unix_ms,
     "app_version": app_version, "native_build": native_build, "os_version": os_version]
  }
}

struct DiagnosticNativeFrame: Codable {
  let binary: String
  let offset: String

  var dictionary: [String: Any] { ["binary": binary, "offset": offset] }
}

enum DiagnosticPayload: Codable {
  case launch(breadcrumb: String, inferred: Bool)
  case crash(exceptionType: Int?, signal: Int?, frames: [DiagnosticNativeFrame])
  case hang(durationMs: UInt64, frames: [DiagnosticNativeFrame])

  private enum Keys: String, CodingKey {
    case kind, breadcrumb, inferred, exception_type, signal, frames, duration_ms
  }

  init(from decoder: Decoder) throws {
    let values = try decoder.container(keyedBy: Keys.self)
    switch try values.decode(String.self, forKey: .kind) {
    case "launch":
      self = .launch(breadcrumb: try values.decode(String.self, forKey: .breadcrumb),
                     inferred: try values.decode(Bool.self, forKey: .inferred))
    case "native-crash":
      self = .crash(exceptionType: try values.decodeIfPresent(Int.self, forKey: .exception_type),
                    signal: try values.decodeIfPresent(Int.self, forKey: .signal),
                    frames: try values.decode([DiagnosticNativeFrame].self, forKey: .frames))
    case "hang":
      self = .hang(durationMs: try values.decode(UInt64.self, forKey: .duration_ms),
                   frames: try values.decode([DiagnosticNativeFrame].self, forKey: .frames))
    default:
      throw DecodingError.dataCorruptedError(forKey: .kind, in: values, debugDescription: "Unsupported diagnostic kind")
    }
  }

  func encode(to encoder: Encoder) throws {
    var values = encoder.container(keyedBy: Keys.self)
    switch self {
    case let .launch(breadcrumb, inferred):
      try values.encode("launch", forKey: .kind)
      try values.encode(breadcrumb, forKey: .breadcrumb)
      try values.encode(inferred, forKey: .inferred)
    case let .crash(exceptionType, signal, frames):
      try values.encode("native-crash", forKey: .kind)
      try values.encode(exceptionType, forKey: .exception_type)
      try values.encode(signal, forKey: .signal)
      try values.encode(frames, forKey: .frames)
    case let .hang(durationMs, frames):
      try values.encode("hang", forKey: .kind)
      try values.encode(durationMs, forKey: .duration_ms)
      try values.encode(frames, forKey: .frames)
    }
  }

  var dictionary: [String: Any] {
    switch self {
    case let .launch(breadcrumb, inferred):
      return ["kind": "launch", "breadcrumb": breadcrumb, "inferred": inferred]
    case let .crash(exceptionType, signal, frames):
      return ["kind": "native-crash", "exception_type": exceptionType.map { $0 as Any } ?? NSNull(),
              "signal": signal.map { $0 as Any } ?? NSNull(), "frames": frames.map { $0.dictionary }]
    case let .hang(durationMs, frames):
      return ["kind": "hang", "duration_ms": durationMs, "frames": frames.map { $0.dictionary }]
    }
  }
}

struct DiagnosticReport: Codable {
  let event_id: String
  let launch_id: String
  let sequence: UInt32
  let occurred_at_unix_ms: UInt64
  let captured_at_unix_ms: UInt64
  let occurrence_time_basis: String
  let launch_id_basis: String
  let app_version: String
  let native_build: String
  let runtime_version: String
  let update_id: String
  let platform: String
  let os_version: String
  let severity: String
  let capture_source: String
  let payload: DiagnosticPayload

  init(eventID: String, context: DiagnosticLaunchContext, sequence: UInt32, occurredAt: UInt64,
       capturedAt: UInt64, severity: String, source: String, payload: DiagnosticPayload,
       occurrenceBasis: String = "exact", launchBasis: String = "process") {
    self.event_id = eventID
    self.launch_id = context.launch_id
    self.sequence = sequence
    self.occurred_at_unix_ms = min(occurredAt, capturedAt)
    self.captured_at_unix_ms = capturedAt
    self.occurrence_time_basis = occurrenceBasis
    self.launch_id_basis = launchBasis
    self.app_version = context.app_version
    self.native_build = context.native_build
    // Match Expo's nativeVersion policy without an Expo Updates dependency.
    // Native/MetricKit reports cannot identify a JS update.
    let runtime = context.app_version + "(" + context.native_build + ")"
    self.runtime_version = runtime.utf8.count <= 128 ? runtime : "unknown"
    self.update_id = "embedded"
    self.platform = "ios"
    self.os_version = context.os_version
    self.severity = severity
    self.capture_source = source
    self.payload = payload
  }

  var dictionary: [String: Any] {
    ["event_id": event_id, "launch_id": launch_id, "sequence": sequence,
     "occurred_at_unix_ms": occurred_at_unix_ms, "captured_at_unix_ms": captured_at_unix_ms,
     "occurrence_time_basis": occurrence_time_basis, "launch_id_basis": launch_id_basis,
     "app_version": app_version, "native_build": native_build, "runtime_version": runtime_version,
     "update_id": update_id, "platform": platform, "os_version": os_version,
     "severity": severity, "capture_source": capture_source, "payload": payload.dictionary]
  }
}

func diagnosticMilliseconds(_ date: Date) -> UInt64 {
  let milliseconds = date.timeIntervalSince1970 * 1_000
  guard milliseconds.isFinite, milliseconds > 0 else { return 0 }
  // Keep timestamps exactly representable across the JavaScript bridge.
  return UInt64(min(milliseconds, 9_007_199_254_740_991))
}

func diagnosticVersion(_ value: String?, limit: Int = 64) -> String {
  guard let value, !value.isEmpty, value.utf8.count <= limit,
        let first = value.utf8.first,
        ((first >= 48 && first <= 57) || (first >= 65 && first <= 90) || (first >= 97 && first <= 122)),
        value.utf8.allSatisfy({ byte in
          (byte >= 48 && byte <= 57) || (byte >= 65 && byte <= 90) ||
          (byte >= 97 && byte <= 122) || byte == 46 || byte == 95 || byte == 45
        }) else { return "unknown" }
  return value
}

func diagnosticOSVersion(_ value: String) -> String {
  // MetricKit may return "iPhone OS 17.1 (21B74)". Retain only the numeric version,
  // never arbitrary OS description/build/device strings.
  for token in value.split(whereSeparator: { !$0.isASCII || (!$0.isNumber && $0 != ".") }) {
    guard token.utf8.count <= 32, token.first?.isNumber == true,
          token.last?.isNumber == true, !token.contains("..") else { continue }
    return String(token)
  }
  return "unknown"
}

func diagnosticStableID(_ bytes: Data) -> String {
  var digest = Array(SHA256.hash(data: bytes).prefix(16))
  // RFC 9562 UUIDv8: a namespaced content digest, never a device identifier.
  digest[6] = (digest[6] & 0x0f) | 0x80
  digest[8] = (digest[8] & 0x3f) | 0x80
  return UUID(uuid: (digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
                     digest[8], digest[9], digest[10], digest[11], digest[12], digest[13], digest[14], digest[15]))
    .uuidString.lowercased()
}
