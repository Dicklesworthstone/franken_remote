//! Final local-session gate on the canonical `InputSession` sink.
//! Revoke first, then allow only that owner's release-only cleanup. No renewal,
//! application action retry or replacement input authority is provided here.
use super::{Control, Status};
use fr_core::{
    input::KeyTransition,
    input_submission::{InputMonitor, InputSink, Operation, PlatformError, Submission},
};

pub(crate) struct Gate<S> {
    pub(crate) inner: S,
    control: Control,
    monitor: InputMonitor,
}
impl<S> Gate<S> {
    /// The monitor MUST belong to the `InputSession` which exclusively owns this
    /// sink. Its normal dispatch rechecks authority after every preparation;
    /// its cleanup revokes first and emits releases only for its own held state.
    /// A monitor from another session would violate this association contract.
    pub fn new(inner: S, control: Control, monitor: InputMonitor) -> Self {
        Self {
            inner,
            control,
            monitor,
        }
    }
    fn check(&self, operation: Operation) -> Result<(), PlatformError> {
        if self.monitor.is_revoked() {
            // This is NOT a remote-input exception: canonical dispatch refuses
            // ALL actions after revocation before calling submit. Only canonical
            // cleanup reaches here, for keys/buttons that same owner still holds.
            return if matches!(
                operation,
                Operation::Key {
                    transition: KeyTransition::Release,
                    ..
                } | Operation::Button { pressed: false, .. }
                    | Operation::Wheel { pressed: false, .. }
            ) {
                Ok(())
            } else {
                Err(PlatformError::Permission)
            };
        }
        if self.control.status() == Status::Active {
            Ok(())
        } else {
            self.monitor.revoke();
            Err(PlatformError::Permission)
        }
    }
}
impl<S: InputSink> InputSink for Gate<S> {
    fn prepare(&mut self, operation: Operation) -> Result<(), PlatformError> {
        self.check(operation)?;
        self.inner.prepare(operation)?;
        // A blocking native prepare may have consumed the remaining validity.
        if let Err(error) = self.check(operation) {
            self.inner.cancel_prepared();
            return Err(error);
        }
        Ok(())
    }
    fn submit(&mut self, operation: Operation) -> Submission {
        if let Err(error) = self.check(operation) {
            self.inner.cancel_prepared();
            return Submission::NotSubmitted(error);
        }
        // Preserve Submitted/Unknown exactly: a signal racing AFTER the call
        // cannot erase an external effect which might already have happened.
        self.inner.submit(operation)
    }
    fn cancel_prepared(&mut self) {
        self.inner.cancel_prepared();
    }
    fn repeat_requires_pair(&self) -> bool {
        self.inner.repeat_requires_pair()
    }
    fn line_scroll_requires_pairs(&self) -> bool {
        self.inner.line_scroll_requires_pairs()
    }
}
