//! One bounded exchange between the original control grant and its native UI
//! cursor owner. This is presentation state, never input or authority.
//!
//! Requests replace one pending image. Native work holds no exchange lock;
//! its completion names the original request and decoder configuration. A
//! stopped grant or reset cannot adopt a delayed native completion.
#![forbid(unsafe_code)]

use std::sync::{
    Arc, Mutex, TryLockError,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};

const MAX_DIMENSION: u16 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Busy,
    Invalid,
    Stopped,
}

/// Straight-alpha RGBA8, validated before its bounded copy. No pixels in Debug.
pub struct Shape {
    width: u16,
    height: u16,
    hotspot_x: u16,
    hotspot_y: u16,
    rgba: Vec<u8>,
}

impl Shape {
    pub fn new(
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
        rgba: &[u8],
    ) -> Result<Self, Error> {
        if width == 0
            || height == 0
            || width > MAX_DIMENSION
            || height > MAX_DIMENSION
            || hotspot_x >= width
            || hotspot_y >= height
        {
            return Err(Error::Invalid);
        }
        let bytes = usize::from(width)
            .checked_mul(usize::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .filter(|bytes| *bytes == rgba.len())
            .ok_or(Error::Invalid)?;
        let mut owned = Vec::new();
        owned.try_reserve_exact(bytes).map_err(|_| Error::Invalid)?;
        owned.extend_from_slice(rgba);
        Ok(Self {
            width,
            height,
            hotspot_x,
            hotspot_y,
            rgba: owned,
        })
    }

    pub const fn geometry(&self) -> (u16, u16, u16, u16) {
        (self.width, self.height, self.hotspot_x, self.hotspot_y)
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }
}

/// The admitted image, owned while a request waits for the native UI.
pub enum Image {
    Default,
    Blank,
    Shape(Shape),
}

/// One original decoder binding. Configuration numbers never wrap or repeat
/// within this application session, including across reconnect attempts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Scope {
    pub attempt: u64,
    pub configuration: u64,
    pub frame_after: u64,
}

/// An unforgeable completion key returned with one accepted request.
#[derive(Debug, Clone)]
pub struct Stamp {
    owner: Arc<()>,
    scope: Scope,
    generation: u64,
}

impl PartialEq for Stamp {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
            && self.scope == other.scope
            && self.generation == other.generation
    }
}
impl Eq for Stamp {}

impl Stamp {
    pub const fn scope(&self) -> Scope {
        self.scope
    }
}

pub struct Request {
    pub stamp: Stamp,
    pub image: Image,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pointer {
    Unknown,
    Inside,
    Outside,
}

struct Queue {
    scope: Scope,
    generation: u64,
    pending: Option<Request>,
}

/// The exact original grant. Native handles never cross this boundary.
pub struct Owner {
    identity: Arc<()>,
    original_stop: Arc<AtomicBool>,
    display: (u32, u32),
    stop: AtomicBool,
    completed: AtomicBool,
    pointer: AtomicU8,
    applied: AtomicU64,
    queue: Mutex<Queue>,
}

impl Owner {
    fn new(scope: Scope, display: (u32, u32), original_stop: Arc<AtomicBool>) -> Self {
        Self {
            identity: Arc::new(()),
            original_stop,
            display,
            stop: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            pointer: AtomicU8::new(0),
            applied: AtomicU64::new(0),
            queue: Mutex::new(Queue {
                scope,
                generation: 0,
                pending: None,
            }),
        }
    }

    pub const fn display(&self) -> (u32, u32) {
        self.display
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.original_stop.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn completed(&self) -> bool {
        self.completed.load(Ordering::Acquire)
    }

    /// Only the native UI calls this, after restoring its pointer and releasing
    /// all native resources for this exact owner. A stop request is not cleanup.
    pub fn complete(&self) {
        self.stop();
        self.completed.store(true, Ordering::Release);
    }

    pub fn pointer(&self) -> Pointer {
        match self.pointer.load(Ordering::Acquire) {
            1 => Pointer::Inside,
            2 => Pointer::Outside,
            _ => Pointer::Unknown,
        }
    }

    pub fn observe(&self, pointer: Pointer) {
        let value = match pointer {
            Pointer::Unknown => 0,
            Pointer::Inside => 1,
            Pointer::Outside => 2,
        };
        self.pointer.store(value, Ordering::Release);
    }

    pub fn applied(&self) -> u64 {
        self.applied.load(Ordering::Acquire)
    }

    pub fn scope(&self) -> Result<Scope, Error> {
        Ok(self.queue.try_lock().map_err(lock_error)?.scope)
    }

    /// Copy outside the short exchange lock, retaining the ORIGINAL scope
    /// across allocation. Configuration changes refuse rather than relabel it.
    pub(crate) fn request(
        &self,
        make: impl FnOnce() -> Result<Image, Error>,
    ) -> Result<u64, Error> {
        if self.stopped() {
            return Err(Error::Stopped);
        }
        let original = self.scope()?;
        let image = make()?;
        let mut queue = self.queue.try_lock().map_err(lock_error)?;
        if self.stopped() {
            return Err(Error::Stopped);
        }
        if queue.scope != original {
            return Err(Error::Busy);
        }
        let generation = queue.generation.checked_add(1).ok_or(Error::Stopped)?;
        queue.generation = generation;
        queue.pending = Some(Request {
            stamp: Stamp {
                owner: Arc::clone(&self.identity),
                scope: original,
                generation,
            },
            image,
        });
        Ok(generation)
    }

    pub fn take(&self) -> Result<Option<Request>, Error> {
        let mut queue = self.queue.try_lock().map_err(lock_error)?;
        if self.stopped() {
            queue.pending = None;
            return Err(Error::Stopped);
        }
        Ok(queue.pending.take())
    }

    /// Check both before native work and when acknowledging it. A superseded
    /// image is discarded, never credited as a newer request or configuration.
    pub fn current(&self, stamp: &Stamp) -> Result<bool, Error> {
        let queue = self.queue.try_lock().map_err(lock_error)?;
        Ok(!self.stopped()
            && Arc::ptr_eq(&self.identity, &stamp.owner)
            && queue.scope == stamp.scope
            && queue.generation == stamp.generation)
    }

    pub fn acknowledge(&self, stamp: &Stamp) -> Result<bool, Error> {
        let queue = self.queue.try_lock().map_err(lock_error)?;
        if self.stopped()
            || !Arc::ptr_eq(&self.identity, &stamp.owner)
            || queue.scope != stamp.scope
            || queue.generation != stamp.generation
        {
            return Ok(false);
        }
        self.applied.fetch_max(stamp.generation, Ordering::Release);
        Ok(true)
    }

    fn configure(&self, scope: Scope) -> Result<(), Error> {
        let mut queue = self.queue.lock().map_err(|_| Error::Stopped)?;
        queue.scope = scope;
        queue.pending = None;
        Ok(())
    }
}

fn lock_error<T>(error: TryLockError<T>) -> Error {
    match error {
        TryLockError::WouldBlock => Error::Busy,
        TryLockError::Poisoned(error) => {
            drop(error.into_inner());
            Error::Stopped
        }
    }
}

#[derive(Default)]
struct Binding {
    scope: Scope,
    owner: Option<Arc<Owner>>,
}

/// Opted into by a native UI before the session starts. Each new attempt and
/// grant waits for the preceding native owner to complete its original cleanup.
#[derive(Default)]
pub struct Hub {
    binding: Mutex<Binding>,
}

impl Hub {
    pub fn owner(&self) -> Result<Option<Arc<Owner>>, Error> {
        let binding = self.binding.try_lock().map_err(lock_error)?;
        Ok(binding.owner.clone())
    }

    pub(crate) fn begin_attempt(&self, attempt: u64) -> Result<(), Error> {
        let mut binding = self.binding.lock().map_err(|_| Error::Stopped)?;
        if attempt == 0 || attempt <= binding.scope.attempt {
            return Err(Error::Invalid);
        }
        if binding
            .owner
            .as_ref()
            .is_some_and(|owner| !owner.completed())
        {
            return Err(Error::Busy);
        }
        binding.scope.attempt = attempt;
        binding.scope.frame_after = 0;
        binding.owner = None;
        Ok(())
    }

    /// Called before a replacement decoder can publish any picture. Pending
    /// images and native completions keep their earlier scope and are fenced.
    pub(crate) fn configure(&self, attempt: u64, frame_after: u64) -> Result<(), Error> {
        let mut binding = self.binding.lock().map_err(|_| Error::Stopped)?;
        if attempt == 0
            || binding.scope.attempt != attempt
            || frame_after < binding.scope.frame_after
        {
            return Err(Error::Invalid);
        }
        let configuration = binding
            .scope
            .configuration
            .checked_add(1)
            .ok_or(Error::Stopped)?;
        let scope = Scope {
            attempt,
            configuration,
            frame_after,
        };
        if let Some(owner) = &binding.owner {
            owner.configure(scope)?;
        }
        binding.scope = scope;
        Ok(())
    }

    pub(crate) fn bind(
        &self,
        attempt: u64,
        display: (u32, u32),
        original_stop: Arc<AtomicBool>,
    ) -> Result<Arc<Owner>, Error> {
        let mut binding = self.binding.try_lock().map_err(lock_error)?;
        if attempt == 0
            || binding.scope.attempt != attempt
            || binding.scope.configuration == 0
            || display.0 == 0
            || display.1 == 0
        {
            return Err(Error::Invalid);
        }
        if original_stop.load(Ordering::SeqCst)
            || binding
                .owner
                .as_ref()
                .is_some_and(|owner| !owner.completed())
        {
            return Err(Error::Stopped);
        }
        let owner = Arc::new(Owner::new(binding.scope, display, original_stop));
        binding.owner = Some(Arc::clone(&owner));
        Ok(owner)
    }
}

#[cfg(test)]
#[path = "local_cursor/tests.rs"]
mod tests;
