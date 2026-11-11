import XCTest
@testable import FrankenRemote

final class HostStoreTests: XCTestCase {
    private var defaults: UserDefaults!
    private let suite = "fr.tests.hoststore"

    override func setUp() {
        defaults = UserDefaults(suiteName: suite)
        defaults.removePersistentDomain(forName: suite)
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: suite)
    }

    /// The app's real name check: the Rust core through the C ABI.
    private func store() -> HostStore {
        HostStore(defaults: defaults, isValidName: { $0.withCString { fr_host_name_valid($0) } })
    }

    func testNamesAreNormalizedAndOnlyTailnetNamesAreAccepted() {
        XCTAssertEqual(HostStore.normalize("  https://Mac-Studio.tail1f21e.TS.net.:8443/x "),
                       "mac-studio.tail1f21e.ts.net")
        let s = store()
        XCTAssertNil(s.problem(with: "mac-studio.tail1f21e.ts.net"))
        XCTAssertEqual(s.problem(with: ""), .empty)
        for bad in ["100.89.198.71", "mac-studio", "mac.example.com", "a.b.c.ts.net"] {
            XCTAssertEqual(s.problem(with: bad), .notTailnetName, bad)
        }
        XCTAssertNotNil(s.add("mac-studio.tail1f21e.ts.net", port: 8443))
        XCTAssertEqual(s.problem(with: "MAC-STUDIO.tail1f21e.ts.net"), .duplicate)
        XCTAssertNil(s.add("mac.example.com", port: 8443))
        XCTAssertNil(s.add("other.tail1f21e.ts.net", port: 0))
    }

    func testHostsPersistAndPinsAreRecordedOnlyForTheirOwnNameAndNeverReplaced() throws {
        let s = store()
        let host = try XCTUnwrap(s.add("mac.tailtest.ts.net", port: 18443, nickname: "Mac"))
        s.recordPin("fr-pin-v1 other.tailtest.ts.net 100.64.0.2", for: host.id)
        XCTAssertNil(s.host(host.id)?.pin, "a pin for another name is ignored")
        s.recordPin("fr-pin-v1 mac.tailtest.ts.net 100.64.0.1", for: host.id)
        XCTAssertEqual(s.host(host.id)?.pin, "fr-pin-v1 mac.tailtest.ts.net 100.64.0.1")
        s.recordPin("fr-pin-v1 mac.tailtest.ts.net 100.64.0.9", for: host.id)
        XCTAssertEqual(s.host(host.id)?.pin, "fr-pin-v1 mac.tailtest.ts.net 100.64.0.1",
                       "only the user's explicit forget replaces a pin")
        // A pin that keeps every saved address and adds a new family is
        // recorded (the core admitted the family and pinned it)...
        s.recordPin("fr-pin-v1 mac.tailtest.ts.net 100.64.0.1 fd7a:115c:a1e0::1", for: host.id)
        XCTAssertEqual(s.host(host.id)?.pin, "fr-pin-v1 mac.tailtest.ts.net 100.64.0.1 fd7a:115c:a1e0::1")
        // ...but one that drops a saved address never replaces it.
        s.recordPin("fr-pin-v1 mac.tailtest.ts.net fd7a:115c:a1e0::1 100.64.0.9", for: host.id)
        XCTAssertEqual(s.host(host.id)?.pin, "fr-pin-v1 mac.tailtest.ts.net 100.64.0.1 fd7a:115c:a1e0::1")
        let reloaded = store()
        XCTAssertEqual(reloaded.hosts.map(\.name), ["mac.tailtest.ts.net"])
        XCTAssertEqual(reloaded.hosts.first?.title, "Mac")
        reloaded.forgetPin(host.id)
        XCTAssertNil(reloaded.host(host.id)?.pin)
        // Renaming drops the pin: it belonged to the old name.
        reloaded.recordPin("fr-pin-v1 mac.tailtest.ts.net 100.64.0.1", for: host.id)
        var renamed = try XCTUnwrap(reloaded.host(host.id))
        renamed.name = "mac2.tailtest.ts.net"
        reloaded.update(renamed)
        XCTAssertNil(reloaded.host(host.id)?.pin)
    }

    func testTheRustCoreRefusesASessionForAnInvalidNameWithoutStartingOne() {
        var out: FrHandle = 0
        let status: Int32 = "not-a-tailnet-name.example".withCString { name in
            var config = FrConfig(host: name, port: 8443, control: false, direct: false, pin: nil,
                                  picture_width: 0, picture_height: 0)
            return fr_session_start(&config, &out)
        }
        XCTAssertEqual(status, FR_INVALID)
        XCTAssertEqual(out, 0)
        XCTAssertEqual(fr_session_free(12345), FR_STALE)
    }
}
