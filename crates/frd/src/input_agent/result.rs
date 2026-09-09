//! Project real native receipts using the original request's immutable binding.
//! Missing, evicted or never-started outcomes are explicit, not invented receipts.
use super::{Agent, Dispatch, Error, PlatformError, Refusal, Reply, Response};
use fr_core::input::InputRequest;
use fr_wire::input_result::{InputResult, ResultBinding, SequenceSpace};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputReply {
    Record(InputResult),
    ConsumedWithoutReceipt,
    ObsoletePointer,
    Refused(Refusal),
    CancelledBeforeStart,
    NativePanicWithoutReceipt,
    InitializationFailed(PlatformError),
}

#[derive(Clone, Copy)]
pub(super) struct ResultContext {
    binding: ResultBinding,
    space: SequenceSpace,
}
impl ResultContext {
    pub(super) fn new(channel: u32, request: InputRequest<'_>) -> Self {
        Self {
            binding: ResultBinding {
                channel,
                session: request.credentials.session,
                lease: request.credentials.lease,
            },
            space: if request.event.is_pointer() {
                SequenceSpace::Pointer
            } else {
                SequenceSpace::Action
            },
        }
    }
    fn project(self, reply: Reply) -> Result<InputReply, Error> {
        Ok(match reply {
            Reply::Input(Ok(Dispatch::Completed(receipt)))
            | Reply::NativePanic {
                receipt: Some(receipt),
            } => InputReply::Record(
                InputResult::from_receipt(self.binding, self.space, receipt)
                    .map_err(Error::Wire)?,
            ),
            Reply::Input(Ok(Dispatch::ConsumedWithoutReceipt)) => {
                InputReply::ConsumedWithoutReceipt
            }
            Reply::Input(Ok(Dispatch::ObsoletePointer)) => InputReply::ObsoletePointer,
            Reply::Input(Err(refusal)) => InputReply::Refused(refusal),
            Reply::CancelledBeforeStart => InputReply::CancelledBeforeStart,
            Reply::NativePanic { receipt: None } => InputReply::NativePanicWithoutReceipt,
            Reply::InitializationFailed(error) => InputReply::InitializationFailed(error),
            Reply::Authority(_) | Reply::Reconciliation(_) | Reply::ReconciliationPanic { .. } => {
                return Err(Error::NotInputCommand);
            }
        })
    }
}
impl Agent {
    fn input_context(&self) -> Result<ResultContext, Error> {
        if !self.shared.lock().outstanding {
            return Err(Error::NoPendingCommand);
        }
        self.response_context.ok_or(Error::NotInputCommand)
    }
    /// Collect an input reply suitable for the existing host-to-viewer codec.
    /// Projection never upgrades API submission to observed application effects.
    /// Selecting this API for an authority command does not consume its reply.
    pub fn try_input_result(&mut self) -> Result<Option<InputReply>, Error> {
        let context = self.input_context()?;
        self.try_reply()?.map(|r| context.project(r)).transpose()
    }
    /// Await and project the original input result. Dropping this wait has the
    /// same revoke-without-replay semantics as `response`; the result remains
    /// collectable, including its original binding after controller handoff.
    pub fn input_response(&mut self) -> Result<InputResponse<'_>, Error> {
        let context = self.input_context()?;
        Ok(InputResponse {
            response: self.response(),
            context,
        })
    }
}

#[must_use = "await the reply; abandoning the wait revokes input"]
pub struct InputResponse<'a> {
    response: Response<'a>,
    context: ResultContext,
}
impl Future for InputResponse<'_> {
    type Output = Result<InputReply, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        Pin::new(&mut this.response)
            .poll(task)
            .map(|r| r.and_then(|r| this.context.project(r)))
    }
}
