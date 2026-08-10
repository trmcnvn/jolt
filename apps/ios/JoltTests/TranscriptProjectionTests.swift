import XCTest
@testable import Jolt

final class TranscriptProjectionTests: XCTestCase {
    private let pageJSON = #"""
    {
      "id":"message-1",
      "revision":"base",
      "firstOrdinal":0,
      "messages":[{
        "id":"message-1",
        "role":"assistant",
        "parts":[{"id":"text-1","kind":"text","text":"hello"}],
        "createdAt":1,
        "deviceId":"host"
      }]
    }
    """#

    func testAppliesUtf8TextAppendDelta() throws {
        let page = try JSONDecoder().decode(MobileTranscriptPage.self, from: Data(pageJSON.utf8))
        let delta = try JSONDecoder().decode(MobileTranscriptDelta.self, from: Data(#"""
        {
          "pageId":"message-1",
          "pageRevision":"next",
          "frame":{
            "upsert":[],
            "append":[{"entry":"message-1","part":"text-1","text":" 🌍","len":10}],
            "remove":[],
            "count":1
          }
        }
        """#.utf8))

        let updated = try XCTUnwrap(page.applying(delta))
        XCTAssertEqual(updated.revision, "next")
        guard case .text(_, let text) = updated.messages[0].parts[0] else {
            return XCTFail("expected text part")
        }
        XCTAssertEqual(text, "hello 🌍")
    }

    func testDecodesToolResolutionAndInputRequestIdentity() throws {
        let page = try JSONDecoder().decode(MobileTranscriptPage.self, from: Data(#"""
        {
          "id":"message-1",
          "revision":"base",
          "firstOrdinal":0,
          "messages":[{
            "id":"message-1",
            "role":"assistant",
            "parts":[
              {
                "id":"tool-1",
                "kind":"tool",
                "call":{"kind":"exec","command":"pwd"},
                "isError":false,
                "resolved":false
              },
              {
                "id":"in-request-1",
                "kind":"input",
                "requestId":"request-1",
                "questions":[{
                  "id":"question-1",
                  "header":"Choice",
                  "question":"Continue?",
                  "options":["Yes","No"],
                  "multiSelect":false
                }],
                "resolved":false
              }
            ],
            "createdAt":1,
            "deviceId":"host"
          }]
        }
        """#.utf8))

        guard case .tool(_, _, _, let resolved) = page.messages[0].parts[0] else {
            return XCTFail("expected tool part")
        }
        XCTAssertFalse(resolved)
        guard case .input(_, let requestId, _, _) = page.messages[0].parts[1] else {
            return XCTFail("expected input part")
        }
        XCTAssertEqual(requestId, "request-1")
    }

    func testMaterializesSequencedReconnectDeltas() throws {
        let page = try JSONSerialization.jsonObject(with: Data(pageJSON.utf8))
        let bootstrapObject: [String: Any] = [
            "sequence": 4,
            "manifest": [
                "pages": [[
                    "id": "message-1",
                    "revision": "base",
                    "firstOrdinal": 0,
                    "messageCount": 1,
                    "estimatedBytes": 100,
                    "previousPageId": NSNull(),
                ]],
            ],
            "pages": [page],
            "deltas": [[
                "sequence": 5,
                "delta": [
                    "pageId": "message-1",
                    "pageRevision": "next",
                    "frame": [
                        "upsert": [],
                        "append": [[
                            "entry": "message-1",
                            "part": "text-1",
                            "text": "!",
                            "len": 6,
                        ]],
                        "remove": [],
                        "count": 1,
                    ],
                ],
            ]],
        ]
        let data = try JSONSerialization.data(withJSONObject: bootstrapObject)
        let wire = try JSONDecoder().decode(MobileTranscriptBootstrap.self, from: data)
        let bootstrap = try XCTUnwrap(wire.materialized())

        XCTAssertEqual(bootstrap.sequence, 5)
        XCTAssertEqual(bootstrap.pages[0].revision, "next")
        guard case .text(_, let text) = bootstrap.pages[0].messages[0].parts[0] else {
            return XCTFail("expected text part")
        }
        XCTAssertEqual(text, "hello!")
    }
}
