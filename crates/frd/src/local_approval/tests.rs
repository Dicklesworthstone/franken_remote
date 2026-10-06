use super::*;
use super::terminal::Presentation;
use fr_core::ids::{HostBootId, OsSessionId, RemoteSessionId};
use std::{cell::Cell, collections::VecDeque, io::{self, Read, Write}, rc::Rc};

fn question() -> Question {
    Question::Observation {
        peer: "100.64.0.2:47200".parse().unwrap(),
        binding: ControlBinding {
            id: 11,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        },
        role: Role::RequestControl,
    }
}
fn handle() -> Handle {
    Handle {
        shared: Arc::new(Shared {
            state: Mutex::new(State { status: Status::Ready, pending: None }),
        }),
    }
}
fn setup(now: Instant, challenge: u128) -> (Handle, Ticket, Presentation) {
    let handle = handle();
    let ticket = handle.ask_at(question(), challenge, now).unwrap();
    let presentation = Presentation::new(ticket.pending.clone()).unwrap();
    (handle, ticket, presentation)
}
struct Tty {
    input: VecDeque<u8>,
    output: Vec<u8>,
    max_write: usize,
    max_read: usize,
    reads: usize,
    eof: bool,
    after_read: Option<(Rc<Cell<Instant>>, Instant)>,
}
impl Tty {
    fn new(text: &str) -> Self {
        Self {
            input: text.bytes().collect(), output: Vec::new(),
            max_write: usize::MAX, max_read: usize::MAX, reads: 0,
            eof: false, after_read: None,
        }
    }
}
impl Write for Tty {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.max_write.min(bytes.len());
        self.output.extend_from_slice(&bytes[..count]);
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}
impl Read for Tty {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        if let Some((clock, next)) = &self.after_read { clock.set(*next); }
        if self.input.is_empty() {
            return if self.eof { Ok(0) } else { Err(io::ErrorKind::WouldBlock.into()) };
        }
        let count = self.input.len().min(bytes.len()).min(self.max_read);
        for byte in &mut bytes[..count] { *byte = self.input.pop_front().unwrap(); }
        Ok(count)
    }
}

#[test]
fn exact_receipt_is_single_use_and_holds_capacity_until_consumed() {
    let now = Instant::now();
    let (handle, mut ticket, mut presentation) = setup(now, 5);
    assert_eq!(ticket.question(), question());
    let mut tty = Tty::new(&format!("approve {:032x}\n", 5));
    presentation.turn(&mut tty, || now).unwrap();
    assert!(matches!(handle.ask_at(question(), 6, now), Err(Error::Busy)));
    assert_eq!(ticket.take_at(now), Ok(Some(true)));
    assert_eq!(ticket.take_at(now), Err(Error::Closed));
    let output = String::from_utf8(tty.output).unwrap();
    assert!(output.contains("100.64.0.2:47200"));
    assert!(output.contains("does NOT grant input control"));
}

#[test]
fn typeahead_old_challenge_and_generic_yes_never_approve() {
    let now = Instant::now();
    for answer in ["y\n".to_owned(), "yes\n".to_owned(), "\n".to_owned(),
        format!("approve {:032x}\n", 8), format!("approve {:032x} \n", 9),
        format!("\x1b[2Japprove {:032x}\n", 9)] {
        let (_, mut ticket, mut presentation) = setup(now, 9);
        presentation.turn(&mut Tty::new(&answer), || now).unwrap();
        assert_eq!(ticket.take_at(now), Ok(Some(false)));
    }
}

#[test]
fn abandoned_and_stopped_tickets_cannot_receive_a_late_yes() {
    let now = Instant::now();
    let (_, ticket, mut presentation) = setup(now, 5);
    let pending = ticket.pending.clone();
    drop(ticket);
    let mut tty = Tty::new(&format!("approve {:032x}\n", 5));
    presentation.turn(&mut tty, || now).unwrap();
    assert_eq!(pending.state_at(now), CANCELLED);
    assert_eq!(tty.reads, 0);
    assert!(tty.output.is_empty());
    let (handle, mut ticket, mut presentation) = setup(now, 7);
    presentation.turn(&mut Tty::new(&format!("approve {:032x}\n", 7)), || now).unwrap();
    handle.shared.stop(Error::Closed);
    assert_eq!(ticket.take_at(now), Err(Error::Closed));
    assert!(handle.ask_at(question(), 8, now).is_err());
}

#[test]
fn expiry_fences_even_a_yes_already_delivered_by_the_worker() {
    let now = Instant::now();
    let end = now + PROMPT_TIMEOUT;
    let (_, mut ticket, mut presentation) = setup(now, 5);
    presentation.turn(&mut Tty::new(&format!("approve {:032x}\n", 5)), || now).unwrap();
    assert_eq!(ticket.take_at(end), Err(Error::Expired));
    let (_, mut ticket, mut presentation) = setup(now, 6);
    let mut tty = Tty::new(&format!("approve {:032x}\n", 6));
    presentation.turn(&mut tty, || end).unwrap();
    assert_eq!(ticket.take_at(end), Err(Error::Expired));
    assert_eq!(tty.reads, 0);
}

#[test]
fn native_read_delay_consumes_original_deadline() {
    let now = Instant::now();
    let clock = Rc::new(Cell::new(now));
    let (_, mut ticket, mut presentation) = setup(now, 5);
    let mut tty = Tty::new(&format!("approve {:032x}\n", 5));
    tty.after_read = Some((clock.clone(), now + PROMPT_TIMEOUT));
    presentation.turn(&mut tty, || clock.get()).unwrap();
    assert_eq!(ticket.take_at(clock.get()), Err(Error::Expired));
}

#[test]
fn partial_output_finishes_before_input_and_fragmented_answer_works() {
    let now = Instant::now();
    let (_, mut ticket, mut presentation) = setup(now, 5);
    let mut tty = Tty::new(&format!("approve {:032x}\n", 5));
    tty.max_write = 1;
    tty.max_read = 1;
    presentation.turn(&mut tty, || now).unwrap();
    assert_eq!(tty.reads, 0);
    for _ in 0..2200 {
        presentation.turn(&mut tty, || now).unwrap();
        if ticket.pending.state_at(now) == ALLOWED { break; }
    }
    assert_eq!(ticket.take_at(now), Ok(Some(true)));
}

#[test]
fn stalled_terminal_and_eof_are_not_approval() {
    let now = Instant::now();
    let (_, mut ticket, mut presentation) = setup(now, 5);
    let mut tty = Tty::new("");
    presentation.turn(&mut tty, || now).unwrap();
    assert_eq!(ticket.take_at(now), Ok(None));
    tty.eof = true;
    assert_eq!(presentation.turn(&mut tty, || now), Err(Error::Unavailable));
    assert_ne!(ticket.pending.state_at(now), ALLOWED);
    let (_, mut ticket, mut presentation) = setup(now, 6);
    let mut tty = Tty::new(&format!("approve {:032x}\n", 6));
    tty.max_write = 0;
    assert_eq!(presentation.turn(&mut tty, || now), Err(Error::Unavailable));
    assert_eq!(ticket.take_at(now), Ok(None));
}

#[test]
fn oversized_lines_and_pasted_following_lines_cannot_supply_a_yes() {
    let now = Instant::now();
    for text in ["x".repeat(512), format!("deny\napprove {:032x}\n", 7)] {
        let (_, mut ticket, mut presentation) = setup(now, 7);
        presentation.turn(&mut Tty::new(&text), || now).unwrap();
        assert_eq!(ticket.take_at(now), Ok(Some(false)));
    }
}

#[test]
fn native_failure_is_retained_when_cleanup_stops_again() {
    let handle = handle();
    handle.shared.stop(Error::Unavailable);
    handle.shared.stop(Error::Closed);
    assert_eq!(handle.status(), Status::Stopped(Error::Unavailable));
}
