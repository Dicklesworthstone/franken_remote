//! Keep an incoming connection scope alive while the hub owns its original Host.
use super::{Admission, Error, State, Ticket, wake};
use crate::session_startup::{Approval, Host, Role};
use asupersync::time::{TimerDriverHandle, TimerHandle};
use asupersync::types::Time;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

impl Admission {
    /// Admit the original authenticated Host and follow it through termination.
    /// Unlike `admit_host`, this operation must remain alive for the entire viewer
    /// session: use it inside the native connection's guarded application callback.
    /// Returning only a Ticket from that callback would end its connection scope.
    ///
    /// Hub/capture/local consent still need their ORIGINAL independent service.
    /// This future never polls the Host, renews authority or creates a task. It
    /// holds only the original receipt and one timer registration, with a 10ms
    /// maximum completion-check interval. A dropped (even unpolled) service fences
    /// only its viewer; keeping a cloned ticket cannot keep that viewer alive.
    pub fn serve_host<F>(&self, mut host: Host, notify: F) -> Result<HostService, Error>
    where
        F: FnMut(Approval, Role) -> Result<(), ()> + Send + 'static,
    {
        let (cx, _, _) = host.shared_open_context().map_err(Error::Session)?;
        let driver = cx
            .timer_driver()
            .ok_or(Error::Session(crate::session_startup::Error::Clock))?;
        let previous = driver.now();
        let ticket = self.admit_host(host, notify)?;
        Ok(HostService {
            ticket,
            driver,
            timer: None,
            previous,
            result: None,
        })
    }
}

/// Connection-scoped ownership of a hub-served incoming observer. Completion is
/// terminal service status, never evidence of decode, presentation or control.
/// The cancellation ticket always refers to this exact original session.
#[must_use = "dropping a host service cancels its viewer"]
pub struct HostService {
    ticket: Ticket,
    driver: TimerDriverHandle,
    timer: Option<TimerHandle>,
    previous: Time,
    result: Option<Result<(), Error>>,
}
impl HostService {
    pub fn ticket(&self) -> Ticket {
        self.ticket.clone()
    }
    fn finish(&mut self, result: Result<(), Error>) -> Result<(), Error> {
        if let Some(result) = self.result {
            return result;
        }
        self.ticket.receipt.finish(result);
        // Another owner may have finished first; never replace its result.
        let State::Finished(result) = self.ticket.state() else {
            unreachable!()
        };
        self.result = Some(result);
        if let Some(timer) = self.timer.take() {
            let _ = self.driver.cancel(&timer);
        }
        wake(&self.ticket.registry);
        result
    }
}
impl HostService {
    fn turn(&mut self, task: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let this = self;
        if let Some(result) = this.result {
            return Poll::Ready(result);
        }
        if let State::Finished(result) = this.ticket.state() {
            return Poll::Ready(this.finish(result));
        }
        if let Err(error) = this.ticket.receipt.check() {
            return Poll::Ready(this.finish(Err(error)));
        }
        let now = this.driver.now();
        let until = now.as_nanos().checked_add(10_000_000);
        if now < this.previous || until.is_none() {
            return Poll::Ready(
                this.finish(Err(Error::Session(crate::session_startup::Error::Clock))),
            );
        }
        this.previous = now;
        let until = Time::from_nanos(until.expect("checked above"));
        // A fired timer handle cannot be updated into a new registration. Retire
        // the old handle (including a spent one) before arming the next pulse.
        if let Some(timer) = this.timer.take() {
            let _ = this.driver.cancel(&timer);
        }
        this.timer = Some(this.driver.register(until, task.waker().clone()));
        Poll::Pending
    }
}
impl Drop for HostService {
    fn drop(&mut self) {
        let _ = self.finish(Err(Error::Closed));
    }
}

impl Future for HostService {
    type Output = Result<(), Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let mut turn = PollGuard {
            service: self.get_mut(),
            complete: false,
        };
        let result = turn.service.turn(task);
        turn.complete = true;
        result
    }
}
struct PollGuard<'a> {
    service: &'a mut HostService,
    complete: bool,
}
impl Drop for PollGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self.service.finish(Err(Error::Closed));
        }
    }
}
