import XCTest
@testable import Jolt

final class DeviceRegistrationTests: XCTestCase {
    func testRegistrationPreservesSyncedNameAndCreationTime() {
        let existing = RegistryRow(
            kind: "devices",
            id: "ios-phone",
            seq: 1,
            deleted: false,
            delHlc: nil,
            fields: [
                "name": .string("Renamed phone"),
                "createdAt": .int(100),
            ],
            clocks: [:]
        )

        let fields = iosDeviceRegistrationFields(
            id: "ios-phone",
            deviceName: "Default phone",
            existing: existing,
            at: 200,
            version: "1.2.3"
        )

        XCTAssertEqual(fields["id"], .string("ios-phone"))
        XCTAssertEqual(fields["name"], .string("Renamed phone"))
        XCTAssertEqual(fields["platform"], .string("ios"))
        XCTAssertEqual(fields["createdAt"], .int(100))
        XCTAssertEqual(fields["lastSeenAt"], .int(200))
        XCTAssertEqual(fields["version"], .string("1.2.3"))
    }

    func testRegistrationUsesLocalDefaultsForANewDevice() {
        let fields = iosDeviceRegistrationFields(
            id: "ios-phone",
            deviceName: "My iPhone",
            existing: nil,
            at: 200,
            version: nil
        )

        XCTAssertEqual(fields["name"], .string("My iPhone"))
        XCTAssertEqual(fields["createdAt"], .int(200))
        XCTAssertNil(fields["version"])
    }

    func testViewerPlatformsCannotHostEngines() {
        func device(_ platform: String) -> DeviceRow {
            DeviceRow(id: platform, name: platform, platform: platform,
                      lastSeenAt: nil, createdAt: nil)
        }

        XCTAssertFalse(device("ios").isEngineHost)
        XCTAssertFalse(device("android").isEngineHost)
        XCTAssertFalse(device("web").isEngineHost)
        XCTAssertTrue(device("macos").isEngineHost)
        XCTAssertTrue(device("linux").isEngineHost)
        XCTAssertTrue(device("windows").isEngineHost)
    }
}
