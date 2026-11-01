#![cfg(all(target_os = "linux", feature = "linux-opus-process"))]
//! Real sandboxed codec ownership and private socket framing are separate tests.
//! Synthetic PCM uses the actual encoder. No physical mic or network is claimed.
use fr_client::audio::playout::decoder::PolledDecoder;
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
use fr_media::audio::{AudioAccessUnit, AudioEncoder, AudioMediaError, AudioPcmFrame};
use fr_native::opus::{CodecLimits, Encoder, process::socket::SocketDecoder};
use std::{
    io::{Read, Write},
    os::{
        fd::OwnedFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
fn config() -> AudioStreamConfig {
    AudioStreamConfig::new(
        AudioDirection::Uplink,
        AudioGeneration::from_raw(7),
        AudioChannels::Mono,
        10,
        20,
    )
    .unwrap()
}
fn limits() -> CodecLimits {
    CodecLimits::new(1000, 480).unwrap()
}
struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn codec() -> (SocketDecoder, Owned) {
    let (socket, child_socket) = UnixStream::pair().unwrap();
    let output: OwnedFd = child_socket.try_clone().unwrap().into();
    let input: OwnedFd = child_socket.into();
    let child = Command::new(env!("CARGO_BIN_EXE_fr-opus-worker"))
        .env_clear()
        .args(["--parent-pid", &std::process::id().to_string()])
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(output))
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .unwrap();
    let mut decoder = SocketDecoder::new(socket, limits()).unwrap();
    decoder.configure(config()).unwrap();
    (decoder, Owned(child))
}
fn ready(decoder: &mut SocketDecoder) {
    let until = Instant::now() + Duration::from_secs(2);
    while !decoder.poll_configured().unwrap() {
        assert!(
            Instant::now() < until,
            "original codec configuration deadline"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn pcm(decoder: &mut SocketDecoder) -> AudioPcmFrame {
    let until = Instant::now() + Duration::from_millis(100);
    loop {
        if let Some(pcm) = decoder.poll_pcm().unwrap() {
            return pcm;
        }
        assert!(Instant::now() < until, "original decode deadline");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn reaped(child: &mut Child) {
    let until = Instant::now() + Duration::from_secs(2);
    while child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < until,
            "parent must observe actual child exit"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(child.wait().is_ok(), "cached actual OS exit receipt");
}
fn packets() -> Vec<AudioAccessUnit> {
    let mut encoder = Encoder::new();
    encoder.configure(config()).unwrap();
    (0..8_u16)
        .map(|sequence| {
            let samples: Vec<_> = (0..480_u16)
                .map(|index| {
                    #[allow(clippy::cast_possible_truncation)]
                    let sample = (8_000.0
                        * (2.0 * std::f64::consts::PI * f64::from(index % 48) / 48.0).sin())
                    .round() as i16;
                    sample
                })
                .collect();
            let frame = AudioPcmFrame::from_interleaved(
                config().generation(),
                config().channels(),
                u64::from(sequence) * 480,
                &samples,
            )
            .unwrap();
            encoder.submit_pcm(&frame).unwrap();
            encoder.poll_packet().unwrap().unwrap()
        })
        .collect()
}
fn power(samples: &[i16], frequency: f64) -> f64 {
    let mut sine = 0.0;
    let mut cosine = 0.0;
    for (index, value) in samples.iter().enumerate() {
        let index = f64::from(u32::try_from(index).unwrap());
        let phase = 2.0 * std::f64::consts::PI * frequency * index / 48_000.0;
        sine += f64::from(*value) * phase.sin();
        cosine += f64::from(*value) * phase.cos();
    }
    sine * sine + cosine * cosine
}
#[test]
#[ignore = "actual isolated libopus child and independent tone oracle; explicit native CI"]
fn original_parent_owned_codec_decodes_tone_and_plc_without_a_nested_child() {
    let (mut decoder, mut child) = codec();
    ready(&mut decoder);
    let fence = decoder.fence();
    let mut checked = 0;
    for packet in packets() {
        decoder.submit_packet(&packet).unwrap();
        assert!(matches!(
            decoder.submit_packet(&packet),
            Err(AudioMediaError::Backpressure)
        ));
        let output = pcm(&mut decoder);
        assert_eq!(output.generation(), config().generation());
        assert_eq!(output.timestamp_samples(), packet.timestamp_samples());
        assert_eq!(output.samples_per_channel(), 480);
        if packet.sequence() >= 2 {
            let tone = power(output.samples(), 1000.0);
            let other = power(output.samples(), 700.0);
            assert!(
                tone > 1_000_000.0 && tone > other * 20.0,
                "independent tone oracle"
            );
            checked += 1;
        }
    }
    assert!(decoder.submit_plc(480).unwrap().is_none());
    let concealed = pcm(&mut decoder);
    assert_eq!(concealed.timestamp_samples(), 8 * 480);
    assert_eq!(concealed.samples_per_channel(), 480);
    assert_eq!(checked, 6);
    fence.stop();
    assert!(fence.is_stopped());
    assert!(decoder.poll_pcm().is_err());
    reaped(&mut child.0);
    println!(
        "actual_parent_owned_codec: 6/6 tone frames matched; PLC timeline matched; parent observed OS exit"
    );
}
#[test]
#[ignore = "actual isolated libopus child and SIGSTOP; explicit native CI"]
fn frozen_codec_channel_expiry_never_claims_reaping_and_cannot_affect_replacement() {
    let (mut old, mut old_child) = codec();
    ready(&mut old);
    assert!(
        Command::new("/bin/kill")
            .args(["-STOP", &old_child.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let until = Instant::now() + Duration::from_secs(1);
    loop {
        let status = std::fs::read_to_string(format!("/proc/{}/status", old_child.0.id())).unwrap();
        if status
            .lines()
            .any(|line| line.starts_with("State:") && line.contains('T'))
        {
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    }
    let packet = packets().remove(0);
    let started = Instant::now();
    old.submit_packet(&packet).unwrap();
    assert!(old.poll_pcm().unwrap().is_none());
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "frozen peer must not block a poll"
    );
    let fence = old.fence();
    std::thread::sleep(Duration::from_millis(110));
    assert!(old.poll_pcm().is_err());
    assert!(fence.is_stopped());
    assert!(
        old_child.0.try_wait().unwrap().is_none(),
        "socket closure is not child reaping"
    );
    let (mut replacement, mut next_child) = codec();
    ready(&mut replacement);
    fence.stop();
    assert!(!replacement.fence().is_stopped());
    replacement.submit_packet(&packet).unwrap();
    assert_eq!(
        pcm(&mut replacement).timestamp_samples(),
        packet.timestamp_samples()
    );
    old_child.0.kill().unwrap();
    reaped(&mut old_child.0);
    replacement.fence().stop();
    reaped(&mut next_child.0);
}
fn peer() -> (SocketDecoder, UnixStream) {
    let (socket, peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    peer.set_write_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut decoder = SocketDecoder::new(socket, limits()).unwrap();
    decoder.configure(config()).unwrap();
    assert!(!decoder.poll_configured().unwrap());
    (decoder, peer)
}
fn request(peer: &mut UnixStream) -> [u8; 48] {
    let mut header = [0; 48];
    peer.read_exact(&mut header).unwrap();
    let bytes = u32::from_be_bytes(header[40..44].try_into().unwrap());
    assert!(bytes <= 1000);
    let mut body = vec![0; usize::try_from(bytes).unwrap()];
    peer.read_exact(&mut body).unwrap();
    header
}
fn response(mut request: [u8; 48], bytes: u32) -> [u8; 48] {
    request[..4].copy_from_slice(b"FROR");
    request[40..44].copy_from_slice(&bytes.to_be_bytes());
    request
}
#[test]
fn private_partial_configuration_reply_keeps_one_original_request() {
    let (mut decoder, mut peer) = peer();
    let header = response(request(&mut peer), 0);
    peer.write_all(&header[..7]).unwrap();
    assert!(!decoder.poll_configured().unwrap());
    peer.write_all(&header[7..]).unwrap();
    assert!(decoder.poll_configured().unwrap());
    let packet = packets().remove(0);
    decoder.submit_packet(&packet).unwrap();
    assert!(decoder.poll_pcm().unwrap().is_none());
    let mut malformed = response(request(&mut peer), u32::MAX);
    malformed[8..16].copy_from_slice(&99_u64.to_be_bytes());
    peer.write_all(&malformed).unwrap();
    assert!(matches!(
        decoder.poll_pcm(),
        Err(AudioMediaError::InvalidPayload)
    ));
    assert!(decoder.fence().is_stopped());
}
#[test]
fn a_partial_pcm_reply_cannot_restart_the_original_decode_deadline() {
    let (mut decoder, mut peer) = peer();
    let configuration = response(request(&mut peer), 0);
    peer.write_all(&configuration).unwrap();
    assert!(decoder.poll_configured().unwrap());
    let packet = packets().remove(0);
    decoder.submit_packet(&packet).unwrap();
    assert!(decoder.poll_pcm().unwrap().is_none());
    let header = response(request(&mut peer), 960);
    peer.write_all(&header).unwrap();
    peer.write_all(&[0; 16]).unwrap();
    assert!(decoder.poll_pcm().unwrap().is_none());
    std::thread::sleep(Duration::from_millis(110));
    assert!(matches!(decoder.poll_pcm(), Err(AudioMediaError::Fatal)));
    assert!(decoder.fence().is_stopped());
}
