//! Fixed-size private UI IPC. One original read budget covers every fragment.
use fr_core::indicator_process::{FRAME_BYTES, Frame, Kind};
use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

const CHANNEL: i32 = 67;
const PROTOCOL: i32 = 65;
const WRITE_BUDGET: Duration = Duration::from_secs(1);

pub(crate) fn read(command: &mut UnixStream, budget: Duration) -> Result<Frame, i32> {
    let until = Instant::now().checked_add(budget).ok_or(CHANNEL)?;
    let mut bytes = [0; FRAME_BYTES];
    let mut filled = 0;
    while filled < bytes.len() {
        let left = until
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or(CHANNEL)?;
        command.set_read_timeout(Some(left)).map_err(|_| CHANNEL)?;
        match command.read(&mut bytes[filled..]) {
            Ok(0) => return Err(CHANNEL),
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(CHANNEL),
        }
    }
    if Instant::now() >= until {
        return Err(CHANNEL);
    }
    Frame::decode(&bytes).map_err(|_| PROTOCOL)
}

pub(crate) fn send(command: &mut UnixStream, request: Frame, kind: Kind) -> Result<(), i32> {
    command
        .set_write_timeout(Some(WRITE_BUDGET))
        .map_err(|_| CHANNEL)?;
    command
        .write_all(&request.reply(kind).encode().map_err(|_| PROTOCOL)?)
        .map_err(|_| CHANNEL)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> Frame {
        Frame {
            kind: Kind::Open,
            sequence: 1,
            epoch: 7,
        }
    }

    #[test]
    fn fragmented_and_back_to_back_frames_keep_their_boundaries() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let original = open();
        let check = Frame {
            kind: Kind::Check,
            sequence: 2,
            ..original
        };
        for frame in [original, check] {
            for fragment in frame.encode().unwrap().chunks(3) {
                writer.write_all(fragment).unwrap();
            }
        }
        assert_eq!(read(&mut reader, WRITE_BUDGET), Ok(original));
        assert_eq!(read(&mut reader, WRITE_BUDGET), Ok(check));
        send(&mut reader, check, Kind::Ready).unwrap();
        let response = read(&mut writer, WRITE_BUDGET).unwrap();
        assert_eq!(response.response_to(check), Ok(Kind::Ready));
    }

    #[test]
    fn corrupt_or_truncated_commands_do_not_become_readiness() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(&[0; FRAME_BYTES]).unwrap();
        assert_eq!(read(&mut reader, WRITE_BUDGET), Err(PROTOCOL));
        writer.write_all(&open().encode().unwrap()[..8]).unwrap();
        drop(writer);
        assert_eq!(read(&mut reader, WRITE_BUDGET), Err(CHANNEL));
    }

    #[test]
    fn a_buffered_frame_cannot_override_an_expired_transaction() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(&open().encode().unwrap()).unwrap();
        assert_eq!(read(&mut reader, Duration::ZERO), Err(CHANNEL));
    }
}
