//! Read the complete, matching attribution before opening a native prompt.
//! Every fragment remains under the caller's ORIGINAL opening/authority gate.
use fr_core::indicator_process::{
    Frame,
    approval::{CONTEXT_BYTES, Context},
};
use std::{
    io::{self, Read},
    os::unix::net::UnixStream,
    thread,
    time::Duration,
};

const CHANNEL: i32 = 67;
const PROTOCOL: i32 = 65;

pub(crate) fn read(
    command: &mut UnixStream,
    hello: Frame,
    mut check: impl FnMut() -> Result<(), i32>,
) -> Result<Context, i32> {
    command.set_nonblocking(true).map_err(|_| CHANNEL)?;
    let mut bytes = [0; CONTEXT_BYTES];
    let mut filled = 0;
    loop {
        check()?;
        match command.read(&mut bytes[filled..]) {
            Ok(0) => return Err(CHANNEL),
            Ok(count) => filled += count,
            Err(error) if retry(&error) => {}
            Err(_) => return Err(CHANNEL),
        }
        if filled == bytes.len() {
            check()?;
            let context = Context::decode(hello, &bytes).map_err(|_| PROTOCOL)?;
            // The parent must await initial readiness before issuing a second
            // request. Buffered extra bytes cannot be hidden by a valid prefix.
            let mut extra = [0; 1];
            loop {
                check()?;
                match command.read(&mut extra) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(context),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    _ => return Err(PROTOCOL),
                }
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
}
fn retry(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::indicator_process::{Kind, approval::Features};
    use std::{io::Write, time::Instant};

    fn hello() -> Frame {
        Frame {
            kind: Kind::ApproveViewDetails,
            sequence: 1,
            epoch: 7,
        }
    }
    fn context() -> Context {
        Context::new(
            "nOriginal",
            Some("verified.example."),
            Features::PLAYBACK,
            1,
        )
        .unwrap()
    }
    fn gate(until: Instant) -> impl FnMut() -> Result<(), i32> {
        move || {
            if Instant::now() < until {
                Ok(())
            } else {
                Err(66)
            }
        }
    }
    #[test]
    fn complete_fragmented_context_keeps_the_original_identity_and_deadline() {
        let (mut command, mut parent) = UnixStream::pair().unwrap();
        let data = context().encode(hello()).unwrap();
        let writer = thread::spawn(move || {
            for fragment in data.chunks(13) {
                parent.write_all(fragment).unwrap();
            }
            parent
        });
        let received = read(
            &mut command,
            hello(),
            gate(Instant::now() + Duration::from_secs(1)),
        )
        .unwrap();
        let _parent = writer.join().unwrap();
        assert_eq!(received, context());
        assert!(received.details(hello()).unwrap().contains("nOriginal"));
    }
    #[test]
    fn partial_missing_wrong_owner_or_extended_context_never_reaches_native_open() {
        for mode in 0..5 {
            let (mut command, mut parent) = UnixStream::pair().unwrap();
            let mut data = context().encode(hello()).unwrap().to_vec();
            match mode {
                0 => data.truncate(CONTEXT_BYTES - 1),
                1 => data.clear(),
                2 => data[31] ^= 1,
                3 => data.push(0),
                _ => data[5] = Kind::ApproveControlDetails as u8,
            }
            parent.write_all(&data).unwrap();
            let result = read(
                &mut command,
                hello(),
                gate(Instant::now() + Duration::from_millis(20)),
            );
            assert!(result.is_err(), "mode {mode} reached native open");
        }
    }
    #[test]
    fn complete_buffered_context_cannot_revive_an_ended_opening_gate() {
        let (mut command, mut parent) = UnixStream::pair().unwrap();
        parent
            .write_all(&context().encode(hello()).unwrap())
            .unwrap();
        assert_eq!(read(&mut command, hello(), || Err(66)), Err(66));
        let mut calls = 0;
        assert_eq!(
            read(&mut command, hello(), || {
                calls += 1;
                if calls == 1 { Ok(()) } else { Err(66) }
            }),
            Err(66)
        );
        assert_eq!(calls, 2, "the gate is checked after the final fragment too");
    }
}
