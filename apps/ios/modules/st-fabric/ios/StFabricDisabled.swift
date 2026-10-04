import ExpoModulesCore

// Default app builds do not link the experimental carrier or require a Rust toolchain.
public final class StFabricModule: Module {
  public func definition() -> ModuleDefinition {
    Name("StFabric")
    AsyncFunction("identity") { () -> String in
      throw Exception(name: "FabricDisabled", description: "Reinstall pods with ST3_FABRIC_PROOF=1 to enable the development proof")
    }
    AsyncFunction("dial") { (_: String, _: String, _: String?) -> String in
      throw Exception(name: "FabricDisabled", description: "The fabric development proof is disabled")
    }
    AsyncFunction("stop") {}
  }
}
