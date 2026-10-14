//! Real sender/receiver ownership with valid tiny PNG byte fixtures. The sink
//! records native-boundary calls; these tests do not qualify an OS clipboard.
#[allow(dead_code)]
#[path = "clipboard_controller/mod.rs"]
mod support;
use fr_client::clipboard::Error;
use fr_core::{
    clipboard::{
        self, Endpoint, PlatformError, Publication, Stamp,
        authority::Monitor,
        image::{ImageClipboardSession, ImageClipboardSink, ImageMetadata},
    },
    limits::{LimitOverrides, ProtocolLimits},
};
use support::*;

// PNGs generated with zlib, real IHDR/IDAT/IEND lengths and CRCs, RGBA8.
fn png(width: u32) -> Vec<u8> {
    let hex = match width {
        2 => {
            "89504e470d0a1a0a0000000d494844520000000200000002080600000072b60d240000001149444154789c63601253fc0fc20c30060025ee04e1e44f07e60000000049454e44ae426082"
        }
        3 => {
            "89504e470d0a1a0a0000000d49484452000000030000000208060000009d74661a0000001149444154789c63601653fc0fc30cc81c00567b07574d2c1c630000000049454e44ae426082"
        }
        4 => {
            "89504e470d0a1a0a0000000d49484452000000040000000208060000007fa87d630000001149444154789c63601153fc8f8c19d005009b0209d1d69d0fc60000000049454e44ae426082"
        }
        8 => {
            "89504e470d0a1a0a0000000d494844520000000800000002080600000065949ded0000001249444154789c63e01053fc8f0f331052000078c013e1e789b2ef0000000049454e44ae426082"
        }
        128 => {
            "89504e470d0a1a0a0000000d4948445200000080000000800806000000c33e61cb0000012f49444154789cedd22101002010c0c04f81a729cd21066227ce4f6cceda97aef91d8001300006c00018000360000c8001300006c00018000360000c8001300006c00018000360000c8001300006c00018000360000c8001300006c00018000360000c80013000068833409c01e20c1067803803c41920ce0071068833409c01e20c1067803803c41920ce0071068833409c01e20c1067803803c41920ce0071068833409c01e20c1067803803c41920ce0071068833409c01e20c1067803803c41920ce0071068833409c01e20c1067803803c41920ce0071068833409c01e20c1067803803c41920ce0071068833409c01e20c1067803803c41920ce0071068833409c01e20c1067803803c41920ce0071068833409c01e20c1067803803c41920ce0071068833409c01e20c10f70008898664b11d66bb0000000049454e44ae426082"
        }
        _ => panic!("fixture width"),
    };
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}
#[derive(Default)]
struct Sink {
    prepared: Vec<Vec<u8>>,
    stamps: Vec<Stamp>,
}
impl ImageClipboardSink for Sink {
    fn prepare_image(
        &mut self,
        bytes: &[u8],
        _: &ImageMetadata,
        _: Stamp,
    ) -> Result<(), PlatformError> {
        self.prepared.push(bytes.to_vec());
        Ok(())
    }
    fn publish_image(&mut self, stamp: Stamp) -> Publication {
        self.stamps.push(stamp);
        Publication::SubmittedToOs
    }
}
#[test]
fn consecutive_images_publish_on_the_same_original_receiver_without_replay() {
    let mut input = accepted();
    ready(&mut input, 0, 0);
    let mut sender = input.attach_image_clipboard(77, true, client(0)).unwrap();
    let native_owner = host_owner();
    let mut receiver = ImageClipboardSession::new(
        Monitor::from_input(&native_owner),
        Endpoint::Host,
        L,
        true,
        host_at(0),
    )
    .unwrap();
    let mut sink = Sink::default();
    let mut first = None;
    for (sequence, width) in (1_u64..).zip([2, 3, 4, 8]) {
        let bytes = png(width);
        let (begin, metadata) = sender
            .offer(u128::from(sequence), &bytes, None, client(sequence))
            .unwrap();
        assert_eq!(begin.stamp.sequence, sequence);
        assert_eq!(begin.metadata, metadata);
        assert_eq!(begin.chunks, 1);
        receiver.begin(begin, host_at(sequence)).unwrap();
        receiver
            .chunk(begin.stamp, 0, 0, &bytes, host_at(sequence))
            .unwrap();
        let receipt = receiver
            .commit(begin.stamp, metadata.total_bytes, &mut sink, || {
                host_at(sequence)
            })
            .unwrap();
        assert_eq!(receipt.publication, Publication::SubmittedToOs);
        first.get_or_insert(begin);
    }
    assert_eq!(sink.prepared, [png(2), png(3), png(4), png(8)]);
    assert_eq!(sink.stamps.len(), 4);
    assert_eq!(
        receiver.begin(first.unwrap(), host_at(5)),
        Err(clipboard::Error::Replay)
    );
    assert!(!sender.is_closed());
}
#[test]
fn rejected_zero_id_does_not_poison_echo_state_or_consume_a_sequence() {
    let mut input = accepted();
    ready(&mut input, 0, 0);
    let mut sender = input.attach_image_clipboard(77, true, client(0)).unwrap();
    assert!(sender.offer(0, &png(2), None, client(1)).is_err());
    let (begin, _) = sender.offer(1, &png(2), None, client(2)).unwrap();
    assert_eq!(begin.stamp.sequence, 1);
    assert!(!sender.is_closed());
}
#[test]
fn dimensions_and_pixels_use_the_negotiated_limits_not_absolute_maxima() {
    for limits in [
        ProtocolLimits::with_overrides(LimitOverrides {
            max_dimension_pixels: Some(100),
            ..Default::default()
        })
        .unwrap(),
        ProtocolLimits::with_overrides(LimitOverrides {
            max_coded_pixels: Some(10_000),
            ..Default::default()
        })
        .unwrap(),
    ] {
        let mut input = accepted_with_limits(limits);
        ready(&mut input, 0, 0);
        let mut sender = input.attach_image_clipboard(77, true, client(0)).unwrap();
        assert_eq!(
            sender.offer(1, &png(128), None, client(1)),
            Err(Error::InvalidLimits)
        );
        let (begin, _) = sender.offer(2, &png(4), None, client(2)).unwrap();
        begin.validate(&limits).unwrap();
        assert_eq!(begin.stamp.sequence, 1);
    }
}
#[test]
fn suppression_and_invalid_bytes_do_not_advance_the_transfer_sequence() {
    let mut input = accepted();
    ready(&mut input, 0, 0);
    let mut sender = input.attach_image_clipboard(77, true, client(0)).unwrap();
    let (first, _) = sender.offer(1, &png(2), None, client(1)).unwrap();
    assert!(
        sender
            .offer(2, &png(2), Some(first.stamp), client(2))
            .is_err()
    );
    assert!(sender.offer(3, &[0; 40], None, client(3)).is_err());
    let (next, _) = sender.offer(4, &png(3), None, client(4)).unwrap();
    assert_eq!(next.stamp.sequence, 2);
}
#[test]
fn cancelling_a_declared_image_does_not_reuse_its_sequence() {
    let mut input = accepted();
    ready(&mut input, 0, 0);
    let mut sender = input.attach_image_clipboard(77, true, client(0)).unwrap();
    let native_owner = host_owner();
    let mut receiver = ImageClipboardSession::new(
        Monitor::from_input(&native_owner),
        Endpoint::Host,
        L,
        true,
        host_at(0),
    )
    .unwrap();
    let (first, _) = sender.offer(1, &png(2), None, client(1)).unwrap();
    receiver.begin(first, host_at(1)).unwrap();
    receiver.cancel(first.stamp).unwrap();
    let (next, _) = sender.offer(2, &png(3), None, client(2)).unwrap();
    receiver.begin(next, host_at(2)).unwrap();
    assert_eq!(next.stamp.sequence, 2);
}
#[test]
fn closed_disabled_and_replaced_owners_cannot_emit_another_offer() {
    for mode in 0..3 {
        let mut input = accepted();
        ready(&mut input, 0, 0);
        let mut sender = input.attach_image_clipboard(77, true, client(0)).unwrap();
        sender.offer(1, &png(2), None, client(1)).unwrap();
        match mode {
            0 => sender.close(),
            1 => {
                sender.local_switch().set_enabled(false);
            }
            _ => {
                input.stop(fr_client::input::StopReason::Disconnected);
            }
        }
        assert!(sender.offer(2, &png(3), None, client(2)).is_err());
    }
}
