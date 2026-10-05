import XCTest
@testable import St3Client

import CryptoKit
final class NativeSubjectTests: XCTestCase {
    private func messageDescriptor() throws -> [String: Any] {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("docs/st3/client-v0/schemas/subject-projection.schema.json"))
        let artifact = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let families = try XCTUnwrap(artifact["families"] as? [[String: Any]])
        return try XCTUnwrap(families.first { $0["family"] as? String == "message" })
    }

    private func claim() throws -> [String: Any] {
        let descriptor = try messageDescriptor()
        let ids = try XCTUnwrap(descriptor["claim_schema_ids"] as? [String: String])
        return ["id": "claim/message-1", "ref": "message/example", "kind": "message.sent",
            "schema_id": try XCTUnwrap(ids["message.sent"]), "retention": "durable",
            "provenance": ["source": "replicated", "claim_id": "claim/message-1",
                "origin": "host/example", "accepted_at": "2026-10-05T00:00:00Z", "store_index": 1],
            "payload_availability": "available", "omitted_fields": [String](),
            "fields": ["from": "person/ada", "to": "agent/helper", "content": "Hello", "status": "sent"]]
    }

    private func subject(_ claim: [String: Any]) throws -> [String: Any] {
        ["kind": "subject", "id": "message/example", "ref": "message/example", "family": "message",
            "schema_id": try XCTUnwrap(messageDescriptor()["schema_id"] as? String), "heads": [claim],
            "heads_complete": true, "local_fence": ["node": "host/example", "position": 1]]
    }

    private func roundTrip<T: Codable>(_ type: T.Type, _ value: [String: Any]) throws -> [String: Any] {
        let decoded = try JSONDecoder().decode(type, from: JSONSerialization.data(withJSONObject: value))
        return try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(decoded)) as? [String: Any])
    }

    func testOptionalNativeFieldsPreserveAbsentNullAndValue() throws {
        let titles: [Any?] = [nil, NSNull(), "Another title"]
        for title in titles {
            var value = try claim()
            var fields = try XCTUnwrap(value["fields"] as? [String: Any])
            fields["title"] = title
            value["fields"] = fields
            let encoded = try roundTrip(SubjectClaim.self, value)
            XCTAssertEqual(encoded as NSDictionary, value as NSDictionary)
            XCTAssertEqual(try roundTrip(SubjectProjection.self, subject(value)) as NSDictionary,
                try subject(value) as NSDictionary)
        }
    }

    func testKnownNativeDescriptorsRejectWrongTypesAndReferences() throws {
        let invalidValues: [Any] = [42, "person/has whitespace", "unregistered/example",
            "person/has\u{0001}control", "person/" + String(repeating: "a", count: 512),
            "person/" + String(repeating: "é", count: 256), "custom/team//example", "file/not-absolute"]
        for invalid in invalidValues {
            var value = try claim()
            var fields = try XCTUnwrap(value["fields"] as? [String: Any])
            fields["from"] = invalid
            value["fields"] = fields
            XCTAssertThrowsError(try roundTrip(SubjectClaim.self, value))
            XCTAssertThrowsError(try roundTrip(NativeMessageMessageSentClaim.self, value))
        }
    }

    func testConcreteNativeModelsRejectWrongDescriptorsLiteralsAndReferences() throws {
        let valid = try claim()
        XCTAssertEqual(try roundTrip(NativeMessageMessageSentClaim.self, valid) as NSDictionary, valid as NSDictionary)
        for (field, invalid) in [("schema_id", "future-claim-descriptor"), ("kind", "message.received"),
            ("ref", "person/ada"), ("ref", "message/")] {
            var value = valid
            value[field] = invalid
            XCTAssertThrowsError(try roundTrip(NativeMessageMessageSentClaim.self, value), "\(field): \(invalid)")
        }
        let projected = try subject(valid)
        XCTAssertEqual(try roundTrip(NativeMessageSubject.self, projected) as NSDictionary, projected as NSDictionary)
        for (field, invalid) in [("schema_id", "future-family-descriptor"), ("kind", "claim"),
            ("ref", "person/ada"), ("id", "message/different")] {
            var value = projected
            value[field] = invalid
            XCTAssertThrowsError(try roundTrip(NativeMessageSubject.self, value), "\(field): \(invalid)")
        }
    }

    func testConcreteCustomClaimBindsCanonicalKindHashAndReference() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let artifact = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf:
            root.appendingPathComponent("docs/st3/client-v0/schemas/subject-projection.schema.json"))) as? [String: Any])
        let families = try XCTUnwrap(artifact["families"] as? [[String: Any]])
        let entry = try XCTUnwrap(families.first { $0["family"] as? String == "custom" })
        let descriptor = try XCTUnwrap(entry["descriptor"] as? [String: Any])
        let claims = try XCTUnwrap(descriptor["claims"] as? [String: Any])
        let kind = "custom.team.record"
        let effective: [String: Any] = ["wire_version": try XCTUnwrap(artifact["wire_version"]),
            "family": "custom", "kind": kind, "identity": try XCTUnwrap(descriptor["identity"]),
            "claim": try XCTUnwrap(claims["custom.*"]), "value_semantics": try XCTUnwrap(descriptor["value_semantics"]),
            "custom_payload": try XCTUnwrap(descriptor["custom_payload"])]
        let bytes = try JSONSerialization.data(withJSONObject: effective, options: [.sortedKeys, .withoutEscapingSlashes])
        let hash = SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
        var value = try claim()
        value["ref"] = "custom/team/example"
        value["kind"] = kind
        value["schema_id"] = "subject-claim-schema/\(hash)/custom/\(kind)"
        value["fields"] = ["nested": ["value": [NSNull(), true, 3] as [Any]]]
        XCTAssertEqual(try roundTrip(NativeCustomCustomClaim.self, value) as NSDictionary, value as NSDictionary)
        for (field, invalid) in [("schema_id", "subject-claim-schema/\(String(repeating: "0", count: 64))/custom/\(kind)"),
            ("kind", "custom.team.other"), ("ref", "custom/client/example"),
            ("ref", "custom/team//example"), ("ref", "message/example")] {
            var malformed = value
            malformed[field] = invalid
            XCTAssertThrowsError(try roundTrip(NativeCustomCustomClaim.self, malformed), "\(field): \(invalid)")
        }
        value["kind"] = "custom.team.other"
        let unavailable = try roundTrip(SubjectClaim.self, value)
        XCTAssertEqual(unavailable["payload_availability"] as? String, "unsupported-schema")
        XCTAssertNil(unavailable["fields"])
        XCTAssertNil(unavailable["provenance"])
    }

    func testDescriptorMismatchReturnsOnlyValidatedUnavailableHeaders() throws {
        var value = try claim()
        value["schema_id"] = "future-claim-descriptor"
        value["fields"] = ["secret": ["unvalidated": true]]
        let unavailable = try roundTrip(SubjectClaim.self, value)
        XCTAssertEqual(unavailable as NSDictionary, ["kind": "unsupported-subject-schema",
            "id": "claim/message-1", "ref": "message/example", "schema_id": "future-claim-descriptor",
            "payload_availability": "unsupported-schema"] as NSDictionary)
        value.removeValue(forKey: "id")
        XCTAssertThrowsError(try roundTrip(SubjectClaim.self, value))
        value.removeValue(forKey: "fields")
        value["id"] = "claim/message-1"
        XCTAssertEqual(try roundTrip(SubjectClaim.self, value) as NSDictionary, unavailable as NSDictionary)
        var projected = try subject(claim())
        projected["schema_id"] = "future-family-descriptor"
        projected["id"] = "message/another"
        XCTAssertThrowsError(try roundTrip(SubjectProjection.self, projected))
    }

    func testNativeFrameBoundaryKeepsUnknownRowsWithoutTheirPayload() throws {
        var projected = try subject(claim())
        projected["schema_id"] = "future-family-descriptor"
        var header = try claim()
        header["fields"] = ["private": ["unvalidated": true]]
        projected["heads"] = [header]
        for (kind, rows) in [("snapshot", "items"), ("changes", "upserts")] {
            var frame: [String: Any] = ["kind": kind, "id": "native-window", "collection": "subjects",
                "snapshot": ["id": "snapshot/example/1", "host_id": "host/example", "store_index": 1,
                    "projection_version": "client-projection.v0", "created_at": "2026-10-05T00:00:00Z"],
                "order": ["message/example"], "has_more": false, rows: [projected]]
            if kind == "changes" { frame["removes"] = [String]() }
            let encoded = try roundTrip(SubjectCollectionFrame.self, frame)
            let encodedRows = try XCTUnwrap(encoded[rows] as? [[String: Any]])
            XCTAssertEqual(encodedRows[0] as NSDictionary, ["kind": "unsupported-subject-schema",
                "id": "message/example", "ref": "message/example", "schema_id": "future-family-descriptor",
                "payload_availability": "unsupported-schema"] as NSDictionary)
            var malformed = projected
            malformed.removeValue(forKey: "id")
            frame[rows] = [malformed]
            XCTAssertThrowsError(try roundTrip(SubjectCollectionFrame.self, frame))
        }
    }
}
