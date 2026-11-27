import XCTest
@testable import FrankenRemote

final class SessionInterruptionTests: XCTestCase {
    let wifi = SessionNetworkPath(online: true, media: [.wifi, .other])
    let cellular = SessionNetworkPath(online: true, media: [.cellular, .other])
    let offline = SessionNetworkPath(online: false, media: [.wifi])

    func testReachableInitialAndRepeatedPathsKeepTheSession() {
        XCTAssertFalse(wifi.interrupts(nil))
        XCTAssertFalse(wifi.interrupts(wifi))
        XCTAssertTrue(offline.interrupts(nil))
        XCTAssertTrue(offline.interrupts(wifi))
        XCTAssertTrue(cellular.interrupts(wifi))
        // A route can change even if the friendly primary label stays "Wi-Fi".
        let wifiOnly = SessionNetworkPath(online: true, media: [.wifi])
        XCTAssertEqual(wifiOnly.description, wifi.description)
        XCTAssertTrue(wifiOnly.interrupts(wifi))
    }

    @MainActor
    func testPendingReceiveHandoffRequiresAnOnlineGestureAndKeepsOriginalDestination() throws {
        let defaults = try XCTUnwrap(UserDefaults(suiteName: UUID().uuidString))
        let store = HostStore(defaults: defaults, isValidName: { _ in true })
        let host = try XCTUnwrap(store.add("work.tailtest.ts.net", port: 8443))
        let model = AppModel(store: store, settings: AppSettings(defaults: defaults), monitor: nil)
        model.pathChanged(wifi)
        model.connect(host, control: true, receiveFiles: true)
        XCTAssertTrue(model.starting?.receiveFiles == true)

        // All calls stay on the same main-actor turn: preparation never dials.
        model.pathChanged(offline)
        let original = try XCTUnwrap(model.reconnectOffer)
        XCTAssertNil(model.starting)
        XCTAssertNil(model.session)
        XCTAssertNil(original.retirement)
        model.reconnect(original)
        XCTAssertNil(model.starting, "offline taps cannot consume an offer")

        var edited = host
        edited.port = 9443
        store.update(edited)
        model.pathChanged(cellular)
        XCTAssertNil(model.starting, "the new route alone never opens a session")
        model.reconnect(original)
        XCTAssertEqual(model.starting?.host, host)
        XCTAssertTrue(model.starting?.control == true)
        XCTAssertTrue(model.starting?.receiveFiles == false)
        XCTAssertNil(model.reconnectOffer)

        // A second handoff cancels the same wait, issues a new gesture token
        // and cannot turn the original file destination back on.
        model.pathChanged(wifi)
        let again = try XCTUnwrap(model.reconnectOffer)
        XCTAssertNotEqual(again.id, original.id)
        model.reconnect(original)
        XCTAssertNil(model.starting)
        model.reconnect(again)
        XCTAssertTrue(model.starting?.receiveFiles == false)
        model.disconnect()
    }

    @MainActor
    func testInactiveAndBackgroundCannotCompleteAnInterruptedStart() throws {
        let defaults = try XCTUnwrap(UserDefaults(suiteName: UUID().uuidString))
        let model = AppModel(store: HostStore(defaults: defaults, isValidName: { _ in true }),
                             settings: AppSettings(defaults: defaults), monitor: nil)
        let host = SavedHost(id: UUID(), name: "work.tailtest.ts.net", port: 8443)
        model.pathChanged(wifi)
        model.connect(host, control: true, receiveFiles: true)
        model.scenePhaseChanged(.background)
        let offer = try XCTUnwrap(model.reconnectOffer)
        model.reconnect(offer)
        XCTAssertNil(model.starting)
        model.scenePhaseChanged(.active)
        model.reconnect(offer)
        XCTAssertNotNil(model.starting)
        model.scenePhaseChanged(.inactive)
        XCTAssertNil(model.starting)
        let next = try XCTUnwrap(model.reconnectOffer)
        XCTAssertNotEqual(next.id, offer.id)
        model.reconnect(next)
        XCTAssertNil(model.starting)
        model.scenePhaseChanged(.active)
        model.reconnect(offer)
        XCTAssertNil(model.starting)
        model.reconnect(next)
        XCTAssertTrue(model.starting?.receiveFiles == false)
        model.disconnect()
    }

    @MainActor
    func testResumeStartsRealHandleWithoutStoredControlOrTransferChoices() async throws {
        let defaults = try XCTUnwrap(UserDefaults(suiteName: UUID().uuidString))
        let settings = AppSettings(defaults: defaults)
        settings.controlOnConnect = true
        let model = AppModel(store: HostStore(defaults: defaults, isValidName: { _ in true }),
                             settings: settings, monitor: nil)
        let name = "fr-ios-interruption.tail00000.ts.net"
        let host = SavedHost(id: UUID(), name: name, port: 18443,
                             pin: "fr-pin-v1 \(name) 100.64.0.9")
        model.pathChanged(wifi)
        model.connect(host, control: true, receiveFiles: true)
        model.pathChanged(cellular)
        model.reconnect(try XCTUnwrap(model.reconnectOffer))
        let deadline = ContinuousClock.now + .seconds(2)
        while model.starting != nil, ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        let session = try XCTUnwrap(model.session, model.startError ?? "session did not start")
        defer { model.disconnect() }
        XCTAssertTrue(session.controlCapable)
        XCTAssertFalse(session.wantsControl)
        XCTAssertFalse(session.receivesFiles)
        XCTAssertFalse(session.clipboardRequested)

        model.pathChanged(offline)
        XCTAssertNil(model.session)
        let offer = try XCTUnwrap(model.reconnectOffer)
        XCTAssertTrue(offer.retirement?.original === session)
        XCTAssertEqual(offer.host, host)
        XCTAssertEqual(session.phase, .closing)
        model.pathChanged(wifi)
        XCTAssertNil(model.session, "route restoration cannot open a second handle")
        model.reconnect(offer)
        // The first handle never established a desktop. Its terminal event
        // cannot supply the successful service-cleanup proof.
        let cleanup = ContinuousClock.now + .seconds(12)
        while model.starting != nil, ContinuousClock.now < cleanup {
            try await Task.sleep(for: .milliseconds(20))
        }
        XCTAssertNil(model.session)
        XCTAssertNotNil(model.startError)

        // The picker remains usable after an unsuccessful dial terminates.
        // It still waits on the original service, then performs cold admission.
        XCTAssertNil(offer.retirement?.original, "the receipt must release the original resources")
        model.connect(host, control: false)
        XCTAssertNil(model.session)
        XCTAssertNotNil(model.starting)
        let fresh = ContinuousClock.now + .seconds(2)
        while model.starting != nil, ContinuousClock.now < fresh {
            try await Task.sleep(for: .milliseconds(20))
        }
        let next = try XCTUnwrap(model.session)
        XCTAssertFalse(session === next)
        XCTAssertFalse(next.controlCapable)
        model.retryEndedSession(session)
        model.retryEndedSession(session, viewOnly: true)
        model.disconnect(session)
        XCTAssertTrue(model.session === next, "retired card gestures cannot change the new session")
    }

    @MainActor
    func testDismissingOfferDoesNotBypassOriginalRetirementForPickerStart() throws {
        let defaults = try XCTUnwrap(UserDefaults(suiteName: UUID().uuidString))
        let model = AppModel(store: HostStore(defaults: defaults, isValidName: { _ in true }),
                             settings: AppSettings(defaults: defaults), monitor: nil)
        let name = "fr-ios-interruption.tail00000.ts.net"
        let host = SavedHost(id: UUID(), name: name, port: 18443,
                             pin: "fr-pin-v1 \(name) 100.64.0.9")
        model.pathChanged(wifi)
        model.connect(host, control: false)
        let original = try XCTUnwrap(model.session)
        model.pathChanged(cellular)
        XCTAssertTrue(model.reconnectOffer?.retirement?.original === original)
        model.reconnectOffer = nil
        model.connect(host, control: false)
        XCTAssertNil(model.session)
        XCTAssertNotNil(model.starting)
        model.disconnect()
    }
}
