//! Fixed-size, original-epoch IPC. The status callback only reads already
//! admitted native evidence; receiving a query never renews its deadline.
use frd::session_monitor::protocol::{self, Reply, State};
use std::io::{ErrorKind, Read, Write};

pub(super) fn serve(
    input: &mut impl Read,
    output: &mut impl Write,
    epoch: u128,
    mut status: impl FnMut() -> Reply,
) -> Result<(), ()> {
    let mut sequence = 1_u64;
    loop {
        let mut bytes = [0; protocol::QUERY_BYTES];
        let mut read = 0;
        while read < bytes.len() {
            match input.read(&mut bytes[read..]) {
                Ok(0) if read == 0 => return Ok(()),
                Ok(0) => return Err(()),
                Ok(n) => read += n,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => return Err(()),
            }
        }
        protocol::check_query(&bytes, epoch, sequence).map_err(|_| ())?;
        let reply = status();
        output
            .write_all(&reply.encode(epoch, sequence).map_err(|_| ())?)
            .map_err(|_| ())?;
        output.flush().map_err(|_| ())?;
        if !matches!(reply.state, State::Opening | State::Active) {
            return Ok(());
        }
        sequence = sequence.checked_add(1).ok_or(())?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn queries(epoch: u128, count: u64) -> Cursor<Vec<u8>> {
        Cursor::new(
            (1..=count)
                .flat_map(|sequence| protocol::query(epoch, sequence).unwrap())
                .collect(),
        )
    }

    fn decoded(bytes: &[u8], epoch: u128, sequence: u64) -> Reply {
        let offset = usize::try_from(sequence - 1).unwrap() * protocol::REPLY_BYTES;
        Reply::decode(
            bytes[offset..offset + protocol::REPLY_BYTES]
                .try_into()
                .unwrap(),
            epoch,
            sequence,
        )
        .unwrap()
    }

    #[test]
    fn opening_is_reported_without_native_progress_and_reads_keep_original_deadlines() {
        let mut input = queries(7, 3);
        let mut output = Vec::new();
        let mut states = [
            Reply {
                state: State::Opening,
                until_ns: 0,
            },
            Reply {
                state: State::Active,
                until_ns: 43,
            },
            Reply {
                state: State::Active,
                until_ns: 43,
            },
        ]
        .into_iter();
        assert_eq!(
            serve(&mut input, &mut output, 7, || states.next().unwrap()),
            Ok(())
        );
        assert_eq!(output.len(), 3 * protocol::REPLY_BYTES);
        assert_eq!(decoded(&output, 7, 1).state, State::Opening);
        assert_eq!(decoded(&output, 7, 2).until_ns, 43);
        assert_eq!(decoded(&output, 7, 3).until_ns, 43);
    }

    #[test]
    fn a_terminal_result_is_written_once_even_with_buffered_queries() {
        let mut input = queries(7, 3);
        let mut output = Vec::new();
        let mut calls = 0;
        assert_eq!(
            serve(&mut input, &mut output, 7, || {
                calls += 1;
                Reply {
                    state: State::Inactive,
                    until_ns: 0,
                }
            }),
            Ok(())
        );
        assert_eq!(calls, 1);
        assert_eq!(output.len(), protocol::REPLY_BYTES);
        assert_eq!(
            input.position(),
            u64::try_from(protocol::QUERY_BYTES).unwrap()
        );
        assert_eq!(decoded(&output, 7, 1).state, State::Inactive);
    }

    #[test]
    fn wrong_epoch_sequence_or_truncated_query_cannot_request_evidence() {
        for bytes in [
            protocol::query(9, 1).unwrap().to_vec(),
            protocol::query(7, 2).unwrap().to_vec(),
            protocol::query(7, 1).unwrap()[..protocol::QUERY_BYTES - 1].to_vec(),
        ] {
            let mut input = Cursor::new(bytes);
            let mut output = Vec::new();
            assert_eq!(
                serve(&mut input, &mut output, 7, || panic!("invalid query")),
                Err(())
            );
            assert!(output.is_empty());
        }
    }

    #[test]
    fn active_without_original_deadline_refuses_before_writing() {
        let mut input = queries(7, 1);
        let mut output = Vec::new();
        assert_eq!(
            serve(&mut input, &mut output, 7, || Reply {
                state: State::Active,
                until_ns: 0,
            }),
            Err(())
        );
        assert!(output.is_empty());
    }
}
