import Foundation
import XCTest
@testable import St3Client

final class ConversationNormalizationTests: XCTestCase {
    func testNormalizedFallbackDecodesWithFrozenOldTimelineEnum() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("docs/st3/client-v0/fixtures/normalized-conversation-legacy.json"))
        let old = try JSONDecoder().decode([LegacyTimelineEntry].self, from: data)
        XCTAssertEqual(old.count, 6)
        guard case .toolCall(let call) = old[1].body else { return XCTFail("missing full tool arguments") }
        guard case .object(let arguments) = call.arguments else { return XCTFail("arguments lost their JSON shape") }
        XCTAssertEqual(arguments["command"], .string("echo invented-token"))
        let current = try JSONDecoder().decode([TimelineEntry].self, from: data)
        XCTAssertEqual(current.count, old.count)

        // A new top-level discriminator actually fails this old decoder. The server must
        // negotiate blocks inside known bodies instead of claiming enums are extensible.
        var entries = try JSONSerialization.jsonObject(with: data) as! [[String: Any]]
        entries[0]["type"] = "reasoning"
        XCTAssertThrowsError(try JSONDecoder().decode([LegacyTimelineEntry].self,
            from: JSONSerialization.data(withJSONObject: entries)))
    }

    func testBlocksRoundTripWithoutChangingTheOldEntryType() throws {
        let data = Data(#"{"id":"timeline-entry/example","sequence":1,"revision":1,"timestamp":"2026-10-06T12:00:00Z","role":"system","type":"content","final":true,"body":{"media_type":"text/plain","text":"future JSON","blocks":[{"id":"block/example","kind":"unknown","source_type":"future","payload":{"raw":{"nested":{"token":"invented-token"}}}}]}}"#.utf8)
        let legacy = try JSONDecoder().decode(LegacyTimelineEntry.self, from: data)
        XCTAssertEqual(legacy.type, .content)
        let current = try JSONDecoder().decode(TimelineEntry.self, from: data)
        guard case .content(let content) = current.body else { return XCTFail("wrong envelope") }
        XCTAssertEqual(content.blocks?.first?.kind, "unknown")
        let roundTrip = try JSONEncoder().encode(current)
        let decoded = try JSONDecoder().decode(TimelineEntry.self, from: roundTrip)
        guard case .content(let copied) = decoded.body else { return XCTFail("wrong round trip") }
        XCTAssertEqual(copied.blocks?.first?.payload, content.blocks?.first?.payload)
    }
}
