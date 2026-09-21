//! Independent, test-only C decoder oracle; no synthetic payloads.
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
use fr_media::audio::AudioPcmFrame;
use std::{
    ffi::{c_int, c_void},
    ptr::NonNull,
};

#[link(name = "libopus.so.0", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn opus_decoder_create(rate: i32, channels: c_int, error: *mut c_int) -> *mut c_void;
    fn opus_decoder_destroy(state: *mut c_void);
    fn opus_decode(
        state: *mut c_void,
        packet: *const u8,
        len: i32,
        pcm: *mut i16,
        samples: c_int,
        fec: c_int,
    ) -> c_int;
    fn opus_packet_get_nb_samples(packet: *const u8, len: i32, rate: i32) -> c_int;
}
pub struct Oracle {
    ptr: NonNull<c_void>,
    channels: usize,
}
impl Oracle {
    pub fn new(channels: AudioChannels) -> Self {
        let mut error = 0;
        // SAFETY: valid rate/channel values, writable error pointer, checked result.
        let ptr =
            unsafe { opus_decoder_create(48_000, i32::from(channels.count()), &raw mut error) };
        assert_eq!(error, 0);
        Self {
            ptr: NonNull::new(ptr).unwrap(),
            channels: usize::from(channels.count()),
        }
    }
    pub fn decode(&mut self, packet: &[u8], samples: usize) -> Vec<i16> {
        let len = i32::try_from(packet.len()).unwrap();
        let samples_c = i32::try_from(samples).unwrap();
        let mut pcm = vec![0_i16; samples * self.channels];
        let data = if packet.is_empty() {
            std::ptr::null()
        } else {
            packet.as_ptr()
        };
        if !packet.is_empty() {
            // SAFETY: packet is readable for len bytes and retained for the call.
            assert_eq!(
                unsafe { opus_packet_get_nb_samples(data, len, 48_000) },
                samples_c
            );
        }
        // SAFETY: exclusive valid decoder, writable frame-sized PCM array, packet
        // bounded by its actual slice. A null packet explicitly requests PLC.
        assert_eq!(
            unsafe { opus_decode(self.ptr.as_ptr(), data, len, pcm.as_mut_ptr(), samples_c, 0) },
            samples_c
        );
        pcm
    }
}
impl Drop for Oracle {
    fn drop(&mut self) {
        // SAFETY: exactly one destructor for this exclusive C-created state.
        unsafe { opus_decoder_destroy(self.ptr.as_ptr()) };
    }
}
pub fn config(
    channels: AudioChannels,
    direction: AudioDirection,
    millis: u16,
) -> AudioStreamConfig {
    AudioStreamConfig::new(direction, AudioGeneration::INITIAL, channels, millis, 20).unwrap()
}
#[allow(clippy::cast_possible_truncation)]
pub fn tone(config: AudioStreamConfig, timestamp: u64) -> AudioPcmFrame {
    let count = usize::try_from(config.expected_samples_per_frame()).unwrap();
    let channels = usize::from(config.channels().count());
    let mut samples = vec![0_i16; count * channels];
    for (n, frame) in samples.chunks_exact_mut(channels).enumerate() {
        for (channel, sample) in frame.iter_mut().enumerate() {
            // Bounded periodic index avoids precision loss even near u64::MAX.
            let phase = (timestamp % 480) + u64::try_from(n).unwrap();
            let phase = f64::from(u32::try_from(phase).unwrap());
            let harmonic = if channel == 0 { 4.0 } else { 7.0 };
            *sample = (12_000.0 * (phase * std::f64::consts::TAU * harmonic / 480.0).sin()) as i16;
        }
    }
    AudioPcmFrame::from_interleaved(config.generation(), config.channels(), timestamp, &samples)
        .unwrap()
}

#[link(name = "libopus.so.0", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn opus_repacketizer_create() -> *mut c_void;
    fn opus_repacketizer_destroy(state: *mut c_void);
    fn opus_repacketizer_cat(state: *mut c_void, data: *const u8, len: i32) -> c_int;
    fn opus_repacketizer_out(state: *mut c_void, data: *mut u8, len: i32) -> i32;
}
/// Aggregate actual 20 ms encoder packets using the independent native framing API.
#[allow(dead_code)] // Shared helper also compiled by the encoder-only target.
pub fn aggregate(packets: &[fr_media::audio::AudioAccessUnit]) -> Vec<u8> {
    struct Repacketizer(NonNull<c_void>);
    impl Drop for Repacketizer {
        fn drop(&mut self) {
            // SAFETY: sole create-returned owner, all referenced packets still live.
            unsafe { opus_repacketizer_destroy(self.0.as_ptr()) };
        }
    }
    // SAFETY: constructor takes no inputs; its allocation is checked and owned.
    let state = Repacketizer(NonNull::new(unsafe { opus_repacketizer_create() }).unwrap());
    for packet in packets {
        let len = i32::try_from(packet.payload().len()).unwrap();
        // SAFETY: real complete packets remain immutably borrowed until out/drop;
        // no buffer is moved or freed while the repacketizer retains its pointer.
        assert_eq!(
            unsafe { opus_repacketizer_cat(state.0.as_ptr(), packet.payload().as_ptr(), len) },
            0
        );
    }
    let mut output = vec![0_u8; 1275];
    // SAFETY: live state/borrowed packets and writable actual output capacity.
    let len = unsafe { opus_repacketizer_out(state.0.as_ptr(), output.as_mut_ptr(), 1275) };
    output.truncate(usize::try_from(len).unwrap());
    output
}
