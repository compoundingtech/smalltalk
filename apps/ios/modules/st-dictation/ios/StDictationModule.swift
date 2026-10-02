import AVFoundation
import ExpoModulesCore
import Speech

// On-device dictation for the message box: the microphone into SpeechAnalyzer's transcriber,
// reported as the words so far (onText) and the input level for a waveform (onLevel). Nothing
// leaves the phone. iOS 26 and later; earlier, isAvailable says no and the app hides the button.
public class StDictationModule: Module {
  private var session: AnyObject?

  public func definition() -> ModuleDefinition {
    Name("StDictation")
    Events("onText", "onLevel", "onError")

    Function("isAvailable") { () -> Bool in
      if #available(iOS 26.0, *) { return true }
      return false
    }

    AsyncFunction("start") { (localeIdentifier: String?) in
      guard #available(iOS 26.0, *) else {
        throw Exception(name: "Unavailable", description: "Dictation needs iOS 26 or later")
      }
      let dictation = Dictation(
        onText: { [weak self] text, final in self?.sendEvent("onText", ["text": text, "final": final]) },
        onLevel: { [weak self] level in self?.sendEvent("onLevel", ["level": level]) },
        onError: { [weak self] message in self?.sendEvent("onError", ["message": message]) }
      )
      self.session = dictation
      try await dictation.start(locale: Locale(identifier: localeIdentifier ?? Locale.current.identifier))
    }

    AsyncFunction("stop") { () -> String in
      guard #available(iOS 26.0, *), let dictation = self.session as? Dictation else { return "" }
      self.session = nil
      return await dictation.stop()
    }
  }
}

@available(iOS 26.0, *)
final class Dictation {
  private let engine = AVAudioEngine()
  private var analyzer: SpeechAnalyzer?
  private var input: AsyncStream<AnalyzerInput>.Continuation?
  private var results: Task<Void, Never>?
  private var settled = ""
  private var latest = ""
  private let onText: (String, Bool) -> Void
  private let onLevel: (Float) -> Void
  private let onError: (String) -> Void

  init(onText: @escaping (String, Bool) -> Void, onLevel: @escaping (Float) -> Void, onError: @escaping (String) -> Void) {
    self.onText = onText
    self.onLevel = onLevel
    self.onError = onError
  }

  func start(locale wanted: Locale) async throws {
    guard await AVAudioApplication.requestRecordPermission() else {
      throw Exception(name: "Microphone", description: "The microphone is off for Small Talk in Settings")
    }
    let locale = await SpeechTranscriber.supportedLocale(equivalentTo: wanted) ?? Locale(identifier: "en-US")
    let transcriber = SpeechTranscriber(locale: locale, transcriptionOptions: [], reportingOptions: [.volatileResults], attributeOptions: [])
    // The model for the language downloads once; later it is on the phone already.
    if let install = try await AssetInventory.assetInstallationRequest(supporting: [transcriber]) {
      try await install.downloadAndInstall()
    }
    let analyzer = SpeechAnalyzer(modules: [transcriber])
    self.analyzer = analyzer
    guard let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [transcriber]) else {
      throw Exception(name: "Format", description: "No audio format suits the transcriber")
    }
    let (stream, continuation) = AsyncStream<AnalyzerInput>.makeStream()
    self.input = continuation
    results = Task { [weak self] in
      do {
        for try await result in transcriber.results {
          guard let self else { return }
          let text = String(result.text.characters)
          if result.isFinal {
            self.settled += text
            self.latest = self.settled
            self.onText(self.settled, true)
          } else {
            self.latest = self.settled + text
            self.onText(self.latest, false)
          }
        }
      } catch {
        self?.onError(error.localizedDescription)
      }
    }
    try await analyzer.start(inputSequence: stream)

    let audio = AVAudioSession.sharedInstance()
    try audio.setCategory(.record, mode: .measurement, options: .duckOthers)
    try audio.setActive(true, options: .notifyOthersOnDeactivation)
    let node = engine.inputNode
    let micFormat = node.outputFormat(forBus: 0)
    guard let converter = AVAudioConverter(from: micFormat, to: format) else {
      throw Exception(name: "Format", description: "The microphone's audio cannot be converted")
    }
    node.installTap(onBus: 0, bufferSize: 2048, format: micFormat) { [weak self] buffer, _ in
      guard let self else { return }
      self.onLevel(Dictation.level(of: buffer))
      let ratio = format.sampleRate / micFormat.sampleRate
      let capacity = AVAudioFrameCount(Double(buffer.frameLength) * ratio) + 1
      guard let converted = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: capacity) else { return }
      var supplied = false
      var error: NSError?
      converter.convert(to: converted, error: &error) { _, status in
        if supplied { status.pointee = .noDataNow; return nil }
        supplied = true
        status.pointee = .haveData
        return buffer
      }
      if error == nil { self.input?.yield(AnalyzerInput(buffer: converted)) }
    }
    engine.prepare()
    try engine.start()
  }

  /// Stop listening and return everything heard.
  func stop() async -> String {
    engine.stop()
    engine.inputNode.removeTap(onBus: 0)
    input?.finish()
    try? await analyzer?.finalizeAndFinishThroughEndOfInput()
    await results?.value
    try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    return latest
  }

  /// The buffer's loudness, 0 to 1, for the waveform.
  static func level(of buffer: AVAudioPCMBuffer) -> Float {
    guard let samples = buffer.floatChannelData?[0], buffer.frameLength > 0 else { return 0 }
    var sum: Float = 0
    for index in 0..<Int(buffer.frameLength) { sum += samples[index] * samples[index] }
    let rms = (sum / Float(buffer.frameLength)).squareRoot()
    return min(1, max(0, (20 * log10(max(rms, 0.000_01)) + 50) / 50))
  }
}
