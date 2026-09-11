//! One owned decoder obligation, without a receiver borrow across foreign IPC.
//! The receiver can admit bounded packets and service repair/expiry concurrently.
use super::{Error, MediaOperation, PresentationReceipt, PresentationStage, Presenter, host_now};
use crate::worker::{self, Deadline};
use asupersync::{cx::Cx, time::sleep, types::Time};
use fr_media::{
    access_unit::{FrameId, FrameKind},
    delivery::{ReceivePipeline, ReceivedPicture},
    worker::{Kind, unit_parts},
};
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
    time::Duration,
};

/// One reservation covers queued, executing AND uncollected decode. Dropping
/// any of those stages invalidates the original receiver before freeing bytes.
/// No stage can construct a successful completion without the native reply.
pub(crate) struct DecodeJob {
    picture: Option<ReceivedPicture>,
    stage: Option<PresentationStage>,
}
impl DecodeJob {
    pub(crate) fn take(cx: &Cx, receiver: &mut ReceivePipeline) -> Result<Option<Self>, Error> {
        Ok(receiver
            .take_decodable(host_now(cx)?.as_micros())
            .map_err(Error::Receiver)?
            .map(|picture| Self {
                picture: Some(picture),
                stage: None,
            }))
    }
    fn picture(&self) -> &ReceivedPicture {
        self.picture.as_ref().expect("owned decode picture")
    }
    pub(crate) fn complete(
        mut self,
        cx: &Cx,
        receiver: &mut ReceivePipeline,
    ) -> Result<PresentationReceipt, Error> {
        let stage = self.stage.ok_or(Error::InvalidFrame)?;
        let frame = FrameId::from_raw(self.picture().descriptor().frame);
        let decoded = receiver
            .complete_decode(self.picture(), host_now(cx)?.as_micros())
            .map_err(Error::Receiver)?;
        // Completion validates the original receiver, in-flight identity and
        // ORIGINAL reference deadline. No dequeue/IPC delay starts a new budget.
        self.picture = None;
        Ok(PresentationReceipt {
            frame,
            stage,
            decoded,
        })
    }
}
impl Drop for DecodeJob {
    fn drop(&mut self) {
        if let Some(picture) = &self.picture {
            picture.cancel_decode();
        }
    }
}

// Field order deliberately fences the receiver before aborting native work,
// including an unpolled decode future and errors between separate IPC requests.
struct DecodeCall<'a> {
    job: Option<DecodeJob>,
    operation: MediaOperation<'a>,
}
impl Presenter {
    /// A successful network startup stamps the presenter before handing it to
    /// input-grant/UI setup. A standalone or recovered decoder has no such stamp.
    pub(crate) fn check_stream(
        &self,
        q: &fr_transport::quic::QuicRecords,
        media: &crate::media_quic::NegotiatedMedia,
        receiver: &fr_media::delivery::ReceivePipeline,
    ) -> Result<(), Error> {
        let Some((connection, view)) = &self.stream_binding else {
            return Err(Error::InvalidFrame);
        };
        if !q.is_bound_to(connection) || *view != media.binding() {
            return Err(Error::InvalidFrame);
        }
        let cfg = media
            .receiver_config(q, fr_media::delivery::ReceivePolicy::default())
            .map_err(|_| Error::InvalidFrame)?;
        receiver
            .check_delivery_configuration(cfg.limits, cfg.bindings, cfg.epoch)
            .map_err(Error::Receiver)?;
        self.binding.check(receiver).map_err(Error::Receiver)
    }

    pub(crate) fn check_receiver(&self, receiver: &ReceivePipeline) -> Result<(), Error> {
        self.binding.check(receiver).map_err(Error::Receiver)?;
        if self.worker.state() != worker::State::Running {
            return Err(Error::Worker(worker::Error::Unavailable));
        }
        Ok(())
    }
    /// No native operation is submitted here. One picture is taken only after
    /// validating the actual decoder/receiver ownership; foreign receivers are
    /// not consumed. The queue retains its existing one-in-flight discipline.
    pub(crate) fn take_next(
        &self,
        cx: &Cx,
        receiver: &mut ReceivePipeline,
    ) -> Result<Option<DecodeJob>, Error> {
        self.check_receiver(receiver)?;
        DecodeJob::take(cx, receiver)
    }
    /// Own the exact received bytes until the decoder has finished borrowing
    /// them. The returned job still owns their charge until receiver completion.
    /// Guard creation is synchronous: abandoning an unpolled operation fences.
    pub(crate) fn decode_job<'a>(
        &'a mut self,
        cx: &'a Cx,
        job: DecodeJob,
    ) -> impl Future<Output = Result<DecodeJob, Error>> + 'a {
        let valid = self
            .binding
            .check_picture(job.picture())
            .map_err(Error::Receiver);
        let configuration = self.configuration;
        let call = DecodeCall {
            job: Some(job),
            operation: MediaOperation::new(&mut self.worker),
        };
        async move {
            let mut call = call;
            valid?;
            let job = call.job.as_ref().expect("owned decode job");
            let picture = job.picture();
            if picture.epoch().configuration != configuration.generation || job.stage.is_some() {
                return Err(Error::InvalidFrame);
            }
            let current = host_now(cx)?.as_micros();
            if current >= picture.reference_deadline_us() {
                return Err(Error::Worker(worker::Error::Deadline));
            }
            let display = current < picture.display_deadline_us();
            let d = picture.descriptor();
            let kind = d.reference.map_or(
                FrameKind::Idr {
                    recovery: picture.epoch().recovery,
                },
                |id| FrameKind::Predicted {
                    references: FrameId::from_raw(id),
                },
            );
            // One bounded IPC staging allocation plus the original, charged
            // receiver buffer. No encoded picture or reference is cloned.
            let payload = unit_parts(
                FrameId::from_raw(d.frame),
                d.capture_micros,
                configuration.generation,
                kind,
                picture.bytes(),
            )?;
            let nanos = picture
                .reference_deadline_us()
                .checked_mul(1000)
                .ok_or(Error::Worker(worker::Error::Deadline))?;
            let deadline =
                Deadline::after(cx, Duration::from_millis(200))?.capped_at(Time::from_nanos(nanos));
            {
                let mut native = pin!(run_native(
                    call.operation.worker,
                    cx,
                    display,
                    payload,
                    d.frame,
                    deadline,
                ));
                poll_fn(|task| {
                    if !picture.is_live() {
                        return Poll::Ready(Err(Error::Receiver(
                            fr_media::delivery::DeliveryError::DecodeMismatch,
                        )));
                    }
                    native.as_mut().poll(task)
                })
                .await?;
            }
            if !picture.is_live() {
                return Err(Error::Receiver(
                    fr_media::delivery::DeliveryError::DecodeMismatch,
                ));
            }
            call.operation.completed = true;
            let mut job = call.job.take().expect("owned decode job");
            job.stage = Some(if display {
                PresentationStage::SubmittedToCompositor
            } else {
                PresentationStage::DecodedOnly
            });
            Ok(job)
        }
    }
}

async fn run_native(
    worker: &mut worker::Worker,
    cx: &Cx,
    display: bool,
    payload: Vec<u8>,
    frame: u64,
    deadline: Deadline,
) -> Result<(), Error> {
    let mut reply = worker
        .request(
            cx,
            if display { Kind::Present } else { Kind::Decode },
            payload,
            deadline,
        )
        .await?;
    loop {
        let expected = if display {
            Kind::Presented
        } else {
            Kind::Decoded
        };
        if reply.header.kind == expected && reply.body() == frame.to_be_bytes() {
            return Ok(());
        }
        if reply.header.kind != Kind::NeedInput {
            return Err(Error::InvalidFrame);
        }
        sleep(
            cx.timer_driver()
                .ok_or(worker::Error::MissingRuntime)?
                .now(),
            Duration::from_millis(1),
        )
        .await;
        reply = worker.request(cx, Kind::Poll, vec![], deadline).await?;
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod tests;

#[cfg(all(test, target_os = "linux"))]
mod stream_fixture;
