//! The `audio-down` attachment: positive selection, observer-only, and every
//! audio kind confined to its own lane and direction. Route/length checks only;
//! Opus and `AudioConfiguration` semantics belong to the session owners.
use super::delivery::{attach, enable, record, transfer};
use super::*;
use fr_wire::attachment::MediaRole;

fn offer_audio(l: &mut Link, cx: &Cx, selection: &Selection) -> Result<MediaChannel, Error> {
    l.h.offer_media_role(
        cx,
        ChannelScope {
            control: l.hr,
            parent: parent(),
            selection,
        },
        ChannelRequest {
            binding: binding(9),
            ticket: Ticket(91),
            timeout: Duration::from_secs(2),
        },
        MediaRole::AudioDown,
        || true,
    )
}

#[test]
fn audio_down_needs_its_capability_an_observer_and_confines_every_audio_kind() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        // Without positive selection no audio route or reservation exists.
        let before = l.h.usage();
        let selection = l.selection.clone();
        assert!(matches!(
            offer_audio(&mut l, &cx, &selection),
            Err(Error::WrongRoute)
        ));
        let audio = Capability {
            name: fr_wire::audio::CAPABILITY.into(),
            version: fr_wire::audio::VERSION,
            required: false,
        };
        // A controller's selection never receives the audio-down channel.
        let mut control = l.selection.clone();
        control.capabilities.push(audio.clone());
        control.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
        control.role = fr_wire::negotiation::Role::RequestControl;
        assert!(matches!(
            offer_audio(&mut l, &cx, &control),
            Err(Error::WrongRoute)
        ));
        assert_eq!(l.h.usage(), before);
        assert!(!l.h.is_closed());
        l.selection.capabilities.push(audio);
        l.selection.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
        let (_, _, hv, _) = attach(&mut l, &cx, MediaRole::Video, 8).await;
        let (mut channel, _, ha, ca) = attach(&mut l, &cx, MediaRole::AudioDown, 9).await;
        assert_eq!(ha.outbound.messages, Messages::AudioControl);
        assert_eq!(ha.inbound.messages, Messages::AudioReplies);
        assert_eq!(ca.inbound.messages, Messages::AudioControl);
        assert_eq!(ca.outbound.messages, Messages::AudioReplies);
        assert_eq!(ha.outbound.priority, Priority::Critical);
        assert_eq!(ha.byte_allowance, 1150);
        let hd = ha.datagram.unwrap();
        let cd = ca.datagram.unwrap();
        assert_eq!((hd.kind, hd.outbound, cd.outbound), (0x62, true, false));
        // Configuration and stop on the host's reliable lane.
        for (kind, len) in [(0x60, 52), (0x63, 40)] {
            transfer(
                &mut l,
                &cx,
                true,
                Route::Stream(ha.outbound),
                Route::Stream(ca.inbound),
                &record(kind, 9, len),
            )
            .await;
        }
        // Acknowledgement and receiver stop on the viewer's reply lane.
        for (kind, len) in [(0x61, 44), (0x63, 40)] {
            transfer(
                &mut l,
                &cx,
                false,
                Route::Stream(ca.outbound),
                Route::Stream(ha.inbound),
                &record(kind, 9, len),
            )
            .await;
        }
        // Packets only on the audio datagram route.
        transfer(
            &mut l,
            &cx,
            true,
            Route::Datagram(hd),
            Route::Datagram(cd),
            &record(0x62, 9, 300),
        )
        .await;
        let before = l.h.usage();
        for (route, bytes) in [
            // No packet on the reliable lane and no configuration as a datagram.
            (Route::Stream(ha.outbound), record(0x62, 9, 300)),
            (Route::Datagram(hd), record(0x60, 9, 52)),
            // The host never sends the viewer's acknowledgement.
            (Route::Stream(ha.outbound), record(0x61, 9, 44)),
            // Audio never rides the video lanes, and video never rides audio.
            (Route::Datagram(hv.datagram.unwrap()), record(0x62, 8, 300)),
            (Route::Stream(hv.outbound), record(0x60, 8, 52)),
            (Route::Datagram(hd), record(0x34, 9, 300)),
            // The negotiated 1150-byte datagram cap binds audio as well.
            (Route::Datagram(hd), record(0x62, 9, 1151)),
        ] {
            assert!(
                matches!(
                    l.h.send(&cx, route, &bytes, clock(&cx) + 1_000_000, || true),
                    Err(Error::WrongRoute | Error::TooLarge)
                ),
                "{route:?}"
            );
        }
        assert_eq!(l.h.usage(), before);
        // Audio is not part of the video media set's replacement retirement.
        assert!(matches!(
            channel.retire_media(&mut l.h, &cx),
            Err(Error::WrongRoute)
        ));
        // Exactly one audio-down channel per connection.
        let selection = l.selection.clone();
        assert!(matches!(
            offer_audio(&mut l, &cx, &selection),
            Err(Error::WrongRoute)
        ));
    });
}
