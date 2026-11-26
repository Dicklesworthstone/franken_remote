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

    func testMachineLinksReviewWithoutSavingAndReuseTheSavedIdentity() throws {
        let s = store()
        let url = try XCTUnwrap(URL(string: "frs://MAC.tailtest.ts.net:18443"))
        let link = try XCTUnwrap(MachineLink(url))
        XCTAssertEqual(link.name, "mac.tailtest.ts.net")
        XCTAssertEqual(link.port, 18443)
        XCTAssertTrue(s.hosts.isEmpty)

        var host = try XCTUnwrap(s.accept(link))
        host.nickname = "Work"
        host.pin = "fr-pin-v1 mac.tailtest.ts.net 100.64.0.1"
        host.direct = true
        s.update(host)
        let shared = try XCTUnwrap(host.shareURL)
        XCTAssertEqual(shared.absoluteString, "fr://mac.tailtest.ts.net:18443")
        XCTAssertEqual(MachineLink(shared), link)
        XCTAssertEqual(s.accept(link), host)

        let different = try XCTUnwrap(MachineLink(try XCTUnwrap(URL(string: "fr://mac.tailtest.ts.net"))))
        XCTAssertEqual(different.port, 8443)
        XCTAssertNil(s.accept(different))
        XCTAssertEqual(s.hosts, [host])
        XCTAssertEqual(store().hosts, [host])
    }

    func testPastingALinkIntoTheEditorUsesItsOwnPortAndDoesNotConnect() {
        let s = store()
        let input = MachineAddressInput("  fr://MAC.tailtest.ts.net:18443\n", portField: "9443")
        XCTAssertEqual(input.name, "mac.tailtest.ts.net")
        XCTAssertEqual(input.port, 18443)
        XCTAssertTrue(input.portFromLink)
        XCTAssertNil(s.problem(with: input.name))
        XCTAssertTrue(s.hosts.isEmpty)
        XCTAssertEqual(MachineAddressInput("frs://mac.tailtest.ts.net", portField: "9443").port, 8443)
        let manual = MachineAddressInput(" Mac.tailtest.ts.net ", portField: "9443")
        XCTAssertEqual(manual.name, input.name)
        XCTAssertEqual(manual.port, 9443)
        XCTAssertFalse(manual.portFromLink)
        XCTAssertNil(MachineAddressInput(manual.name, portField: "0").port)
        let malformed = MachineAddressInput("fr://mac.tailtest.ts.net?token=abc", portField: "8443")
        XCTAssertEqual(s.problem(with: malformed.name), .notTailnetName)
    }

    func testMachineLinkRejectsCredentialsAmbiguousTargetsAndUnusablePorts() {
        for raw in [
            "https://mac.tailtest.ts.net", "fr://100.64.0.1", "fr://mac", "fr://mac.example.com",
            "fr://a.b.c.ts.net", "fr://mac.tailtest.ts.net:0", "fr://mac.tailtest.ts.net:65536",
            "fr://mac.tailtest.ts.net:", "fr://person@mac.tailtest.ts.net",
            "fr://mac.tailtest.ts.net?control=1", "fr://mac.tailtest.ts.net?token=abc",
            "fr://mac.tailtest.ts.net#pin", "fr://mac.tailtest.ts.net/file",
            "fr://mac%2etailtest.ts.net", "fr://mac.tailtest.ts.net/%00"
        ] {
            XCTAssertNil(URL(string: raw).flatMap(MachineLink.init), raw)
        }
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
