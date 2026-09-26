import XCTest
@testable import St3Client

final class St3ClientTests: XCTestCase {
    func testTimelineRouteIDStripsPrefixAndEncodesOneSegment() {
        XCTAssertEqual(St3Client.routedSessionID("session/release-agent/9"), "release-agent%2F9")
        XCTAssertEqual(St3Client.routedSessionID("release agent/9"), "release%20agent%2F9")
    }

    func testCapabilitiesFixtureDecodes() throws {
        let json = #"{"api_version":"st3.client.v0","request_id":"request/1","snapshot":{"id":"snapshot/host/1/a","host_id":"host/a","store_index":1,"projection_version":"client-projection.v0","created_at":"2026-09-20T12:00:00Z"},"value":{"kind":"capabilities","session_actor":"person/a/session/b","transport":"fabric-loopback","capabilities":[],"limits":{"max_page_items":200,"max_event_items":500,"max_response_bytes":1048576,"max_wait_ms":30000},"event_cursor":"event-cursor/a/1","oldest_event_cursor":"event-cursor/a/0","schemas":["schema.json"]}}"#
        let value = try JSONDecoder().decode(Envelope<Capabilities>.self, from: Data(json.utf8))
        XCTAssertEqual(value.value.limits.maxPageItems, 200)
    }

    func testGeneratedActionCoverage() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("docs/st3/client-v0/schemas/operations.json"))
        let operations = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let actions = try XCTUnwrap(operations["actions"] as? [String: Any])
        let reads = try XCTUnwrap(operations["reads"] as? [[String: Any]])
        XCTAssertEqual(Set(ActionType.allCases.map(\.rawValue)), Set(actions.keys))
        XCTAssertEqual(Set(ReadOperation.allCases.map(\.rawValue)), Set(reads.compactMap { $0["id"] as? String }))
    }

    func testAuthorityErrorCodesAreTypedAndRoundTrip() throws {
        for (raw, expected) in [
            ("runtime-not-local", ErrorCode.runtimeNotLocal),
            ("runtime-authority-indeterminate", ErrorCode.runtimeAuthorityIndeterminate),
        ] {
            let decoded = try JSONDecoder().decode(ErrorCode.self, from: Data("\"\(raw)\"".utf8))
            XCTAssertEqual(decoded, expected)
            XCTAssertEqual(String(decoding: try JSONEncoder().encode(decoded), as: UTF8.self), "\"\(raw)\"")
        }
    }

    func testDiscriminatedResourceFixturePreservesTypedDetailsAndVisualization() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("docs/st3/client-v0/fixtures/resources.json"))
        let resources = try JSONDecoder().decode([Resource].self, from: data)
        XCTAssertEqual(resources.count, 15)
        guard case .attention(let attention) = resources[0] else { return XCTFail("attention discriminator lost") }
        XCTAssertEqual(attention.priority, "high")
        XCTAssertEqual(attention.actions, ["launch.approve", "launch.cancel"])
        guard case .launch(let launch) = resources[2] else { return XCTFail("launch discriminator lost") }
        XCTAssertEqual(launch.variants, ["launch-variant/release/default"])
        XCTAssertEqual(launch.visualization?.version, "st3.visualization.v0")
        XCTAssertEqual(launch.visualization?.nodes.first?.goals, ["Build artifacts"])
        XCTAssertEqual(launch.visualization?.decisions.first?.decisionType, .singleChoice)
        XCTAssertEqual(launch.visualization?.diffs.first?.changes.count, 1)
        XCTAssertEqual(launch.visualization?.swimlanes.first?.nodes, ["step/build"])
        guard case .mission(let mission) = resources[6] else { return XCTFail("mission discriminator lost") }
        XCTAssertEqual(mission.runGenerations["mission-run/release/1"], "run-generation/release/1/g1")
        XCTAssertEqual(mission.visualization?.mission, "mission/release")
        guard case .work(let work) = resources[7] else { return XCTFail("work discriminator lost") }
        XCTAssertEqual(work.readinessEpoch, 3)
        XCTAssertEqual(work.goals, ["Build artifacts"])
        XCTAssertNil(work.blockedReason)
        XCTAssertEqual(work.blockers, [])
        guard case .agent(let agent) = resources[8] else { return XCTFail("agent discriminator lost") }
        XCTAssertEqual(agent.runtimeIDs, ["runtime/release-agent"])
        guard case .runtime(let runtime) = resources[9] else { return XCTFail("runtime discriminator lost") }
        XCTAssertEqual(runtime.ownerHostID, "host/host-a")
        XCTAssertEqual(runtime.incarnationID, "runtime-9:2026-09-20T11:06:30Z")
        guard case .machine(let machine) = resources[10] else { return XCTFail("machine discriminator lost") }
        XCTAssertEqual(machine.hostID, "host/host-a")
        XCTAssertEqual(machine.capacity.state, "unknown")
        XCTAssertEqual(machine.occupancy.runningRuntimes, 1)
        XCTAssertEqual(machine.projects, [])
        guard case .history(let history) = resources[13] else { return XCTFail("history discriminator lost") }
        XCTAssertEqual(history.storeIndex, 1842)
        guard case .session(let session) = resources[14] else { return XCTFail("session discriminator lost") }
        XCTAssertEqual(session.timelineCursor, "timeline-cursor/release-agent/9/8")
    }

    func testTimelineFixturePreservesEveryTypedBody() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("docs/st3/client-v0/fixtures/timeline.json"))
        let timeline = try JSONDecoder().decode(Envelope<TimelinePage>.self, from: data).value
        guard case .status = timeline.items[0].body else { return XCTFail("status body lost") }
        guard case .message = timeline.items[1].body else { return XCTFail("message body lost") }
        guard case .content(let content) = timeline.items[2].body else { return XCTFail("content body lost") }
        XCTAssertEqual(content.text, "Build the release.")
        guard case .toolCall(let call) = timeline.items[4].body else { return XCTFail("tool call lost") }
        XCTAssertEqual(call.name, "shell")
        guard case .toolResult = timeline.items[5].body else { return XCTFail("tool result lost") }
        guard case .usage(let usage) = timeline.items[6].body else { return XCTFail("usage body lost") }
        XCTAssertEqual(usage.totalTokens, 500)
        XCTAssertEqual(usage.attribution.agentID, "agent/release-agent")
        guard case .redaction = timeline.items[7].body else { return XCTFail("redaction lost") }
        guard case .truncation = timeline.items[8].body else { return XCTFail("truncation lost") }
        guard case .error = timeline.items[9].body else { return XCTFail("error body lost") }
    }
}
