//! The bounded, content-free event queue the app polls (see `FrEvent` in
//! `include/fr_mobile.h`). Events carry states, codes and geometry only: never
//! names, addresses, pixels, keys or text.
use std::{collections::VecDeque, sync::Mutex};

/// Most events retained between polls. The app polls every display refresh;
/// a stalled app loses the OLDEST non-terminal events first, and an overflow
/// is itself reported so the app knows to re-read session state.
pub const CAPACITY: usize = 64;

pub mod kind {
    pub const STATE: u32 = 1;
    pub const PIN: u32 = 2;
    pub const VIDEO: u32 = 3;
    pub const ENDED: u32 = 4;
    pub const OVERFLOW: u32 = 5;
}
pub mod state {
    pub const CONNECTING: u32 = 1;
    pub const AUTHENTICATED: u32 = 2;
    pub const AWAITING_APPROVAL: u32 = 3;
    pub const VIEWING: u32 = 4;
    pub const REQUESTING_CONTROL: u32 = 5;
    pub const CONTROLLED: u32 = 6;
    pub const RECONNECTING: u32 = 7;
    pub const CLOSING: u32 = 8;
    pub const CONTROL_ENDED: u32 = 9;
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Event {
    pub kind: u32,
    /// `state::*` for STATE; the end code for ENDED (see `code`).
    pub value: u32,
    pub attempt: u32,
    /// VIDEO: the host display's pixel size and the encoded picture size.
    pub display_width: u32,
    pub display_height: u32,
    pub picture_width: u32,
    pub picture_height: u32,
    pub reserved: u32,
}

#[derive(Default)]
pub struct Events {
    queue: Mutex<VecDeque<Event>>,
}
impl Events {
    pub fn push(&self, event: Event) {
        let Ok(mut queue) = self.queue.lock() else {
            return;
        };
        if queue.len() >= CAPACITY {
            // Keep the newest. Never drop the terminal ENDED (it is last) or
            // the one OVERFLOW marker at the front.
            let droppable = |e: &Event| e.kind != kind::ENDED && e.kind != kind::OVERFLOW;
            if let Some(index) = queue.iter().position(droppable) {
                queue.remove(index);
            }
            if !queue.iter().any(|e| e.kind == kind::OVERFLOW) {
                if let Some(index) = queue.iter().position(droppable) {
                    queue.remove(index);
                }
                queue.push_front(Event {
                    kind: kind::OVERFLOW,
                    ..Event::default()
                });
            }
        }
        queue.push_back(event);
    }
    pub fn state(&self, value: u32, attempt: u32) {
        self.push(Event {
            kind: kind::STATE,
            value,
            attempt,
            ..Event::default()
        });
    }
    pub fn poll(&self) -> Option<Event> {
        self.queue.lock().ok()?.pop_front()
    }
    pub fn len(&self) -> usize {
        self.queue.lock().map_or(0, |q| q.len())
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_queue_is_bounded_keeps_order_and_reports_overflow_once() {
        let events = Events::default();
        for n in 0..10 {
            events.state(state::CONNECTING, n);
        }
        assert_eq!(events.poll().map(|e| e.attempt), Some(0));
        assert_eq!(events.len(), 9);
        while events.poll().is_some() {}
        for n in 0..200 {
            events.state(state::VIEWING, n);
        }
        events.push(Event {
            kind: kind::ENDED,
            value: 3,
            ..Event::default()
        });
        assert_eq!(events.len(), CAPACITY);
        let first = events.poll().unwrap();
        assert_eq!(first.kind, kind::OVERFLOW);
        let mut last = None;
        let mut overflow = 0;
        while let Some(event) = events.poll() {
            if event.kind == kind::OVERFLOW {
                overflow += 1;
            }
            last = Some(event);
        }
        assert_eq!(overflow, 0, "overflow is reported once");
        // The terminal event survives any overflow and stays last.
        assert_eq!(last.map(|e| (e.kind, e.value)), Some((kind::ENDED, 3)));
    }
}
