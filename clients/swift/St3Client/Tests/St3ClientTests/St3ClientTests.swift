import XCTest
@testable import St3Client

final class St3ClientTests: XCTestCase {
    func testCreationActionFixtures() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        for name in ["agent-create", "terminal-create", "terminal-end"] {
            let data = try Data(contentsOf: root.appendingPathComponent("docs/st3/client-v0/fixtures/\(name).json"))
            let request = try JSONDecoder().decode(ActionRequest.self, from: data)
            let roundTrip = try JSONEncoder().encode(request)
            XCTAssertEqual(try JSONSerialization.jsonObject(with: data) as? NSDictionary,
                try JSONSerialization.jsonObject(with: roundTrip) as? NSDictionary)
        }
        let parameters = AgentCreateParameters(name: "worker", harness: "codex", message: "--literal text")
        let request = try ActionRequest.agentCreate(id: "action/create", idempotencyKey: "creation-test-key",
            fence: Fence(snapshotID: "snapshot/test"), parameters: parameters)
        XCTAssertEqual(request.parameters["message"], .string("--literal text"))
        let checkout = AgentCreateParameters(name: "parser", harness: "codex", repo: "/work/repo",
            base: "origin/main", branch: "parser", removeAtRunEnd: true)
        let creation = try ActionRequest.agentCreate(id: "action/checkout", idempotencyKey: "checkout-test-key",
            fence: Fence(snapshotID: "snapshot/test"), parameters: checkout)
        XCTAssertEqual(creation.parameters["remove_at_run_end"], .bool(true))
        XCTAssertNil(creation.parameters["removeAtRunEnd"])
        let repositories = try JSONDecoder().decode(HostRepositories.self, from: Data(
            #"{"host_id":"host/example","repositories":[{"path":"/work/repo","workspaces":["/work/parser"],"agent_ids":["agent/example.parser"]}]}"#.utf8))
        XCTAssertEqual(repositories.hostID, "host/example")
        XCTAssertEqual(repositories.repositories[0].agentIDs, ["agent/example.parser"])
    }

    func testGlassFixtureAndNullCreationBase() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("docs/st3/client-v0/fixtures/glasses.json"))
        let glass = try JSONDecoder().decode(Envelope<Glass>.self, from: data).value
        XCTAssertEqual(glass.body?.name, "Main workspace")
        guard case .split(.right, let groups, let ratio) = try XCTUnwrap(glass.body).layout,
              case .group(let tabs) = groups[0],
              case .split(.below, let nested, let nestedRatio) = groups[1],
              case .group(let empty) = nested[1] else { return XCTFail("Expected nested groups") }
        XCTAssertEqual(ratio, 0.3)
        XCTAssertNil(nestedRatio)
        XCTAssertEqual(tabs.count, 2)
        XCTAssertEqual(tabs[0].pane, "agent:agent/example/worker")
        XCTAssertEqual(tabs[0].title, "Work")
        XCTAssertTrue(empty.isEmpty)
        XCTAssertNil(glass.baseRevision)
        let encoded = try JSONEncoder().encode(GlassPut(body: try XCTUnwrap(glass.body), baseRevision: nil))
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: encoded) as? [String: Any])
        XCTAssertTrue(object["base_revision"] is NSNull)
        let fixture = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let value = try XCTUnwrap(fixture["value"] as? [String: Any])
        XCTAssertEqual(object["body"] as? NSDictionary, value["body"] as? NSDictionary)
        _ = try JSONDecoder().decode(Resource.self, from: JSONEncoder().encode(glass))
    }

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

    func testSchemaErrorCodesAreKnownAndRoundTrip() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("docs/st3/client-v0/schemas/client-v0.schema.json"))
        let schema = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let definitions = try XCTUnwrap(schema["$defs"] as? [String: Any])
        let envelope = try XCTUnwrap(definitions["ErrorEnvelope"] as? [String: Any])
        let properties = try XCTUnwrap(envelope["properties"] as? [String: Any])
        let code = try XCTUnwrap(properties["code"] as? [String: Any])
        XCTAssertEqual(code["$ref"] as? String, "#/$defs/ErrorCode")
        let errorCode = try XCTUnwrap(definitions["ErrorCode"] as? [String: Any])
        let branches = try XCTUnwrap(errorCode["anyOf"] as? [[String: Any]])
        let codes = try XCTUnwrap(branches.first?["enum"] as? [String])
        for raw in codes {
            let encoded = try JSONEncoder().encode(raw)
            let decoded = try JSONDecoder().decode(ErrorCode.self, from: encoded)
            if case .unknown = decoded { XCTFail("schema error code is unknown: \(raw)") }
            XCTAssertEqual(try JSONEncoder().encode(decoded), encoded)
        }
    }

    func testAuthorityErrorCodesAreTypedAndRoundTrip() throws {
        for (raw, expected) in [
            ("runtime-not-local", ErrorCode.runtimeNotLocal),
            ("runtime-authority-indeterminate", ErrorCode.runtimeAuthorityIndeterminate),
            ("terminal-unavailable", ErrorCode.terminalUnavailable),
            ("terminal-ended", ErrorCode.terminalEnded),
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
        XCTAssertEqual(resources.count, 18)
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
        XCTAssertEqual(agent.workspace, "/srv/example/release")
        XCTAssertEqual(agent.checkout?.branch, "release")
        guard case .runtime(let runtime) = resources[9] else { return XCTFail("runtime discriminator lost") }
        XCTAssertEqual(runtime.ownerHostID, "host/host-a")
        XCTAssertEqual(runtime.incarnationID, "runtime-9:2026-09-20T11:06:30Z")
        guard case .machine(let machine) = resources[13] else { return XCTFail("machine discriminator lost") }
        XCTAssertEqual(machine.hostID, "host/host-a")
        XCTAssertEqual(machine.capacity.state, "unknown")
        XCTAssertEqual(machine.occupancy.runningRuntimes, 1)
        XCTAssertEqual(machine.projects, [])
        guard case .history(let history) = resources[16] else { return XCTFail("history discriminator lost") }
        XCTAssertEqual(history.storeIndex, 1842)
        guard case .session(let session) = resources[17] else { return XCTFail("session discriminator lost") }
        XCTAssertEqual(session.timelineCursor, "timeline-cursor/release-agent/9/8")
    }

    func testSearchFixturePreservesResultTargetsAndIncompleteHistory() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("docs/st3/client-v0/fixtures/conversation-search.json"))
        let search = try JSONDecoder().decode(Envelope<ConversationSearch>.self, from: data).value
        XCTAssertEqual(search.items[0].entryID, "timeline-entry/note")
        XCTAssertEqual(search.items[0].agentID, "agent/scribe")
        XCTAssertEqual(search.incompleteSources, ["session/older: truncation"])
        XCTAssertFalse(search.page.hasMore)
    }

    func testTimelineFixturePreservesEveryTypedBody() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("docs/st3/client-v0/fixtures/timeline.json"))
        let timeline = try JSONDecoder().decode(Envelope<TimelinePage>.self, from: data).value
        guard case .status = timeline.items[0].body else { return XCTFail("status body lost") }
        guard case .message(let message) = timeline.items[1].body else { return XCTFail("message body lost") }
        XCTAssertEqual(message.tags, ["dictated"])
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

    func testRelayOmissionFixtureKeepsKnownBodiesAndOptionalValues() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("docs/st3/client-v0/fixtures/timeline-relay-omission.json"))
        let wire = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let value = try XCTUnwrap(wire["value"] as? [String: Any])
        let items = try XCTUnwrap(value["items"] as? [[String: Any]])
        // Swift's current closed type enum has no future-record case. Preserve that existing
        // contract here; the Rust relay and TypeScript controls cover the unknown raw body.
        let known = items.filter { ($0["type"] as? String) != "future_record" }
        for item in known {
            let entry = try JSONDecoder().decode(TimelineEntry.self, from: JSONSerialization.data(withJSONObject: item))
            let encoded = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(entry)) as? [String: Any])
            let originalBody = try XCTUnwrap(item["body"] as? [String: Any])
            let encodedBody = try XCTUnwrap(encoded["body"] as? [String: Any])
            // The existing Swift usage model does not expose cache_write_tokens or turn_id.
            // Their wire preservation is tested by the Rust relay and TypeScript consumers;
            // this check covers values this installed Swift model actually represents.
            let unmodeled: Set<String>
            if case .usage = entry.body { unmodeled = ["cache_write_tokens", "turn_id"] }
            else { unmodeled = [] }
            // Codable may omit explicitly nullable nils, without inventing absent fields.
            for (key, original) in originalBody where !(original is NSNull) && key != "attribution" && !unmodeled.contains(key) {
                XCTAssertEqual(encodedBody[key] as? NSObject, original as? NSObject, key)
            }
            for key in encodedBody.keys {
                XCTAssertNotNil(originalBody[key], "invented field: \(key)")
            }
            XCTAssertEqual(encoded["id"] as? String, item["id"] as? String)
            XCTAssertEqual(encoded["sequence"] as? Int, item["sequence"] as? Int)
            XCTAssertEqual(encoded["revision"] as? Int, 2)
        }
        let minimal = try JSONDecoder().decode(TimelineEntry.self, from: JSONSerialization.data(withJSONObject: known[0]))
        guard case .message(let body) = minimal.body else { return XCTFail("message body lost") }
        XCTAssertNil(body.replyTo); XCTAssertNil(body.from); XCTAssertNil(body.to); XCTAssertNil(body.title)
    }

    func testAKindThisClientDoesNotKnowReadsAsUnknown() throws {
        let json = #"{"kind":"example-arrangement","id":"example-arrangement/person/avery/1","revision":"r1","updated_at":"2026-10-04T08:00:00Z","name":"Pinned"}"#
        let resource = try JSONDecoder().decode(Resource.self, from: Data(json.utf8))
        guard case .unknown(let unknown) = resource else { return XCTFail("an unknown kind is kept as unknown") }
        XCTAssertEqual(unknown.kind, "example-arrangement")
        XCTAssertEqual(resource.id, "example-arrangement/person/avery/1")
        XCTAssertEqual(unknown.fields["name"], .string("Pinned"))
        let again = try JSONDecoder().decode(Resource.self, from: JSONEncoder().encode(resource))
        XCTAssertEqual(again.id, resource.id)
    }
}
