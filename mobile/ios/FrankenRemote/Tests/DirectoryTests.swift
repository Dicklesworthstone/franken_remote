import XCTest
@testable import FrankenRemote

/// The tailnet directory as the app reads it: the core's JSON decoded
/// exactly (nothing invented), listed machines opened at their listed
/// addresses, and the real C ABI reporting a failed fetch.
final class DirectoryTests: XCTestCase {
    private let listed = """
    {"status":"ok","via":"mac-mini-old.tail1f21e.ts.net","truncated":false,"machines":[
     {"name":"mac-mini-old.tail1f21e.ts.net","node_id":"nA1CNTRL","os":"macOS","online":true,"serving":true,
      "ipv4":"100.70.0.1","ipv6":"fd7a:115c:a1e0::1","pin":"fr-pin-v1 mac-mini-old.tail1f21e.ts.net 100.70.0.1 fd7a:115c:a1e0::1"},
     {"name":"mac-studio.tail1f21e.ts.net","node_id":"nB2CNTRL","os":"macOS","online":true,"serving":false,
      "ipv4":null,"ipv6":"fd7a:115c:a1e0::2","pin":"fr-pin-v1 mac-studio.tail1f21e.ts.net fd7a:115c:a1e0::2"},
     {"name":"odd.sub.tail1f21e.ts.net","node_id":"nC3CNTRL","os":"","online":false,"serving":false,
      "ipv4":"100.70.0.3","ipv6":null,"pin":null}]}
    """

    func testAListingDecodesExactlyWhatTheHostSent() throws {
        guard case .listed(let via, let machines, let truncated)? = DirectoryResult.parse(listed) else {
            return XCTFail("not a listing")
        }
        XCTAssertEqual(via, "mac-mini-old.tail1f21e.ts.net")
        XCTAssertFalse(truncated)
        XCTAssertEqual(machines.map(\.machine), ["mac-mini-old", "mac-studio", "odd"])
        XCTAssertEqual(machines.map(\.serving), [true, false, false])
        XCTAssertEqual(machines[1].ipv6, "fd7a:115c:a1e0::2")
        XCTAssertNil(machines[1].ipv4)
        XCTAssertEqual(machines[1].nodeID, "nB2CNTRL")
        XCTAssertNil(machines[2].pin)

        // A listed machine opens directly at its listed addresses...
        let studio = try XCTUnwrap(machines[1].host(port: 8443))
        XCTAssertEqual(studio.name, "mac-studio.tail1f21e.ts.net")
        XCTAssertEqual(studio.pin, "fr-pin-v1 mac-studio.tail1f21e.ts.net fd7a:115c:a1e0::2")
        XCTAssertTrue(studio.dialsDirectly)
        // ...and one the app cannot name (no pin) or a zero port never opens.
        XCTAssertNil(machines[2].host(port: 8443))
        XCTAssertNil(machines[1].host(port: 0))
    }

    func testRefusalsAndFailuresStayTypedAndIncompleteDocumentsAreRejected() {
        let disabled = DirectoryResult.parse(#"{"status":"refused","via":"a.tailtest.ts.net","reason":"directory_disabled"}"#)
        XCTAssertEqual(disabled, .refused(via: "a.tailtest.ts.net", .disabled))
        XCTAssertEqual(DirectoryResult.parse(#"{"status":"refused","via":"a.tailtest.ts.net","reason":"directory_unavailable"}"#),
                       .refused(via: "a.tailtest.ts.net", .unavailable))
        XCTAssertEqual(DirectoryResult.parse(#"{"status":"refused","via":"a.tailtest.ts.net","reason":"required_capability"}"#),
                       .refused(via: "a.tailtest.ts.net", .unsupported))
        XCTAssertEqual(DirectoryResult.parse(#"{"status":"refused","via":"a.tailtest.ts.net","reason":"permission_denied"}"#),
                       .refused(via: "a.tailtest.ts.net", .denied))
        XCTAssertEqual(DirectoryResult.parse(#"{"status":"failed","via":"a.tailtest.ts.net","end":2,"detail":"Tailnet(NameUnresolved)"}"#),
                       .failed(via: "a.tailtest.ts.net", end: 2, detail: "Tailnet(NameUnresolved)"))
        XCTAssertTrue(DirectoryRefusal.disabled.message(via: "mini").contains("--directory"))
        // Planted negatives: nothing partial or unknown becomes a listing.
        for bad in ["", "{}", "not json",
                    #"{"status":"ok","via":"a.tailtest.ts.net","truncated":false}"#,
                    #"{"status":"ok","via":"a.tailtest.ts.net","machines":[]}"#,
                    #"{"status":"refused","via":"a.tailtest.ts.net"}"#,
                    #"{"status":"failed","via":"a.tailtest.ts.net","detail":"x"}"#,
                    #"{"status":"maybe","via":"a.tailtest.ts.net"}"#] {
            XCTAssertNil(DirectoryResult.parse(bad), bad)
        }
    }

    func testOnlyRecentTypedMachinesAreAskedNeverDirectoryOnes() {
        let now = Date()
        func host(_ name: String, _ age: TimeInterval?, direct: Bool? = nil) -> SavedHost {
            SavedHost(id: UUID(), name: name, port: 8443, nickname: nil,
                      pin: direct == true ? "fr-pin-v1 \(name) 100.64.0.1" : nil,
                      lastConnected: age.map { now.addingTimeInterval(-$0) }, direct: direct)
        }
        let hosts = [host("old.tailtest.ts.net", 900), host("never.tailtest.ts.net", nil),
                     host("listed.tailtest.ts.net", 1, direct: true),
                     host("new.tailtest.ts.net", 10), host("mid.tailtest.ts.net", 100)]
        XCTAssertEqual(AppModel.directoryCandidates(hosts).map(\.name),
                       ["new.tailtest.ts.net", "mid.tailtest.ts.net", "old.tailtest.ts.net"])
        XCTAssertEqual(AppModel.directoryCandidates([]), [])
    }

    func testHostsSavedBeforeTheDirectoryStillLoadAndDialByName() throws {
        let suite = "fr.tests.directory.compat"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defaults.removePersistentDomain(forName: suite)
        defer { defaults.removePersistentDomain(forName: suite) }
        let old = #"[{"id":"6F9619FF-8B86-D011-B42D-00C04FC964FF","name":"mac.tailtest.ts.net","port":8443,"pin":"fr-pin-v1 mac.tailtest.ts.net 100.64.0.1"}]"#
        defaults.set(Data(old.utf8), forKey: "savedHosts.v1")
        let store = HostStore(defaults: defaults, isValidName: { $0.withCString { fr_host_name_valid($0) } })
        let host = try XCTUnwrap(store.hosts.first)
        XCTAssertNil(host.direct)
        XCTAssertFalse(host.dialsDirectly, "a typed machine keeps resolving its name")
    }

    /// The real C ABI and Swift bridge: an unresolvable name ends the fetch
    /// with exactly that reason, once, and lists nothing.
    func testTheCoreReportsAFailedFetchThroughTheBridge() async {
        let host = SavedHost(id: UUID(), name: "fr-ios-directory.tail00000.ts.net", port: 18443)
        let result = await TailnetDirectory().fetch(from: host)
        guard case .failed(let via, let end, let detail) = result else {
            return XCTFail("\(result)")
        }
        XCTAssertEqual(via, host.name)
        XCTAssertEqual(Int32(bitPattern: end), FR_END_NAME_UNRESOLVED, detail)
    }

    func testTheCoreRefusesABadFetchWithoutCallingBack() {
        // A direct fetch without a pin is invalid; the callback must not run.
        let status: Int32 = "mac.tailtest.ts.net".withCString { name in
            var config = FrConfig(host: name, port: 8443, control: false, direct: true, pin: nil)
            return fr_directory_fetch(&config, nil, { _, _ in fatalError("called back") })
        }
        XCTAssertEqual(status, FR_INVALID)
    }
}
