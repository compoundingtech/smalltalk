import Darwin
import Foundation
import MetricKit

private struct DiagnosticLaunchMarker: Codable {
  let context: DiagnosticLaunchContext
  var next_sequence: UInt32
}

private struct DiagnosticSeenReport: Codable {
  let event_id: String
  let captured_at_unix_ms: UInt64
}

private struct DiagnosticState: Codable {
  var version = 1
  var reports: [DiagnosticReport] = []
  var marker: DiagnosticLaunchMarker?
  // Bounded acknowledgement tombstones prevent pastDiagnosticPayloads replaying reports
  // immediately after JavaScript has durably taken ownership of them.
  var seen: [DiagnosticSeenReport] = []
}

enum DiagnosticStoreError: Error {
  case unsupportedVersion
  case oversizedState
  case unavailableDirectory
  case diskWrite
}

final class DiagnosticStore: @unchecked Sendable {
  static let shared = DiagnosticStore()
  private static let maxReports = 128
  private static let maxBytes = 512 * 1_024
  private static let retentionMs: UInt64 = 7 * 24 * 60 * 60 * 1_000

  let context = DiagnosticLaunchContext.current()
  private let queue = DispatchQueue(label: "smalltalk.diagnostics.disk")
  private var cached: DiagnosticState?
  private var cachedDirectory: URL?
  private var started = false

  // All readers/writers, including MetricKit's background callback, run on this serial
  // queue. The new snapshot becomes visible only after the atomic disk commit succeeds.
  func start() throws {
    try queue.sync { try startLocked() }
  }

  func pending() throws -> [[String: Any]] {
    try queue.sync {
      try startLocked()
      let state = try load()
      let oldest = cutoff(diagnosticMilliseconds(Date()))
      if state.reports.contains(where: { $0.captured_at_unix_ms < oldest }) ||
         state.seen.contains(where: { $0.captured_at_unix_ms < oldest }) {
        try transaction { _ in }
      }
      return try load().reports.map { $0.dictionary }
    }
  }

  func acknowledge(_ ids: [String]) throws {
    try queue.sync {
      try startLocked()
      let acknowledged = Set(ids.prefix(Self.maxReports).compactMap { UUID(uuidString: $0)?.uuidString.lowercased() })
      try transaction { state in state.reports.removeAll { acknowledged.contains($0.event_id) } }
    }
  }

  func capture(_ payloads: [MXDiagnosticPayload]) throws {
    try queue.sync {
      try startLocked()
      let capturedAt = diagnosticMilliseconds(Date())
      let reports = try MetricKitReports.reports(payloads, capturedAt: capturedAt)
      try transaction { state in
        var known = Set(state.seen.map { $0.event_id })
        known.formUnion(state.reports.map { $0.event_id })
        for report in reports where known.insert(report.event_id).inserted {
          state.reports.append(report)
          state.seen.append(DiagnosticSeenReport(event_id: report.event_id, captured_at_unix_ms: capturedAt))
        }
      }
    }
  }

  func terminateCleanly() throws {
    try queue.sync {
      try startLocked()
      try transaction { state in
        if state.marker?.context.launch_id == context.launch_id { state.marker = nil }
      }
    }
  }

  private func startLocked() throws {
    guard !started else { return }
    let now = diagnosticMilliseconds(Date())
    try transaction { state in
      // If the rename committed but directory sync reported an error, a retry must
      // recognize this process marker rather than infer a failure or duplicate a start.
      guard state.marker?.context.launch_id != context.launch_id else { return }
      if let previous = state.marker,
         previous.context.started_at_unix_ms >= cutoff(now), previous.next_sequence < 0x8000_0000 {
        // The start is the last exact time we know. Missing termination is an inference,
        // not a crash: suspension, force quit and OS reclaim often omit willTerminate.
        state.reports.append(DiagnosticReport(
          eventID: diagnosticStableID(Data("st-diagnostics-unclean-v1:\(previous.context.launch_id)".utf8)),
          context: previous.context, sequence: previous.next_sequence,
          occurredAt: previous.context.started_at_unix_ms, capturedAt: now, severity: "warning",
          source: "native-marker", payload: .launch(breadcrumb: "previous-launch-unclean", inferred: true)
        ))
      }
      state.reports.append(DiagnosticReport(
        eventID: diagnosticStableID(Data("st-diagnostics-start-v1:\(context.launch_id)".utf8)),
        context: context, sequence: 0, occurredAt: context.started_at_unix_ms, capturedAt: now,
        severity: "info", source: "native-marker", payload: .launch(breadcrumb: "native-start", inferred: false)
      ))
      state.marker = DiagnosticLaunchMarker(context: context, next_sequence: 1)
    }
    started = true
  }

  private func cutoff(_ now: UInt64) -> UInt64 {
    now > Self.retentionMs ? now - Self.retentionMs : 0
  }

  private func transaction(_ mutate: (inout DiagnosticState) -> Void) throws {
    do {
      var next = try load()
      let now = diagnosticMilliseconds(Date())
      let oldest = cutoff(now)
      next.reports.removeAll { $0.captured_at_unix_ms < oldest }
      next.seen.removeAll { $0.captured_at_unix_ms < oldest }
      mutate(&next)
      // Capture time gives oldest-first queue eviction even for delayed OS reports.
      next.reports.sort { $0.captured_at_unix_ms < $1.captured_at_unix_ms }
      if next.reports.count > Self.maxReports {
        next.reports.removeFirst(next.reports.count - Self.maxReports)
      }
      if next.seen.count > 256 { next.seen.removeFirst(next.seen.count - 256) }
      let encoder = JSONEncoder()
      var data = try encoder.encode(next)
      while data.count > Self.maxBytes, !next.reports.isEmpty {
        next.reports.removeFirst()
        data = try encoder.encode(next)
      }
      guard data.count <= Self.maxBytes else { throw DiagnosticStoreError.oversizedState }
      try persist(data)
      cached = next
    } catch {
      // A failed fsync/rename may have reached disk; re-read instead of trusting stale
      // memory. Acknowledgements reject and JavaScript retains its copy on failure.
      cached = nil
      throw error
    }
  }

  private func directory() throws -> URL {
    if let cachedDirectory { return cachedDirectory }
    let manager = FileManager.default
    guard let root = manager.urls(for: .applicationSupportDirectory, in: .userDomainMask).first else {
      throw DiagnosticStoreError.unavailableDirectory
    }
    var directory = root.appendingPathComponent("st-diagnostics-v1", isDirectory: true)
    try manager.createDirectory(at: directory, withIntermediateDirectories: true,
                                attributes: [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication])
    var values = URLResourceValues()
    values.isExcludedFromBackup = true
    try directory.setResourceValues(values)
    // Make the directory entry (and a newly created Application Support root) durable
    // before writing the first marker. This setup is paid only once per process.
    for parent in [root, root.deletingLastPathComponent()] {
      let folder = parent.path.withCString { Darwin.open($0, O_RDONLY) }
      guard folder >= 0 else { throw DiagnosticStoreError.diskWrite }
      let synchronized = Darwin.fsync(folder) == 0
      Darwin.close(folder)
      guard synchronized else { throw DiagnosticStoreError.diskWrite }
    }
    cachedDirectory = directory
    return directory
  }

  private func load() throws -> DiagnosticState {
    if let cached { return cached }
    let url = try directory().appendingPathComponent("state.json")
    guard FileManager.default.fileExists(atPath: url.path) else {
      let state = DiagnosticState()
      cached = state
      return state
    }
    let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
    if let size = attributes[.size] as? NSNumber, size.intValue > Self.maxBytes {
      throw DiagnosticStoreError.oversizedState
    }
    let state = try JSONDecoder().decode(DiagnosticState.self, from: Data(contentsOf: url))
    guard state.version == 1 else { throw DiagnosticStoreError.unsupportedVersion }
    cached = state
    return state
  }

  private func persist(_ data: Data) throws {
    let directory = try directory()
    let temporary = directory.appendingPathComponent("state.tmp")
    let destination = directory.appendingPathComponent("state.json")
    defer { try? FileManager.default.removeItem(at: temporary) }
    // A fixed staging name is safe because this process owns the directory and every
    // access is serialized. Only complete, synchronized snapshots replace state.json.
    try data.write(to: temporary, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
    let file = temporary.path.withCString { Darwin.open($0, O_RDONLY) }
    guard file >= 0 else { throw DiagnosticStoreError.diskWrite }
    let synchronized = Darwin.fsync(file) == 0
    Darwin.close(file)
    guard synchronized else { throw DiagnosticStoreError.diskWrite }
    let renamed = temporary.path.withCString { source in
      destination.path.withCString { target in Darwin.rename(source, target) == 0 }
    }
    guard renamed else { throw DiagnosticStoreError.diskWrite }
    let folder = directory.path.withCString { Darwin.open($0, O_RDONLY) }
    guard folder >= 0 else { throw DiagnosticStoreError.diskWrite }
    let directorySynchronized = Darwin.fsync(folder) == 0
    Darwin.close(folder)
    guard directorySynchronized else { throw DiagnosticStoreError.diskWrite }
  }
}
