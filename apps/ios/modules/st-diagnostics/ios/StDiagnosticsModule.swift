import ExpoModulesCore

public final class StDiagnosticsModule: Module {
  public func definition() -> ModuleDefinition {
    Name("StDiagnostics")

    Function("launchContext") { () throws -> [String: Any] in
      do {
        try DiagnosticStore.shared.start()
        return DiagnosticStore.shared.context.dictionary
      } catch {
        throw Exception(name: "DiagnosticsStorageFailed", description: "Native diagnostics storage is unavailable")
      }
    }

    AsyncFunction("getPendingReports") { () throws -> [[String: Any]] in
      do {
        return try DiagnosticStore.shared.pending()
      } catch {
        throw Exception(name: "DiagnosticsStorageFailed", description: "Native diagnostics storage is unavailable")
      }
    }

    AsyncFunction("acknowledge") { (ids: [String]) throws in
      do {
        try DiagnosticStore.shared.acknowledge(ids)
      } catch {
        throw Exception(name: "DiagnosticsStorageFailed", description: "Native diagnostics storage is unavailable")
      }
    }
  }
}
