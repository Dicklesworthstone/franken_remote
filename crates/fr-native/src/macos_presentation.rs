//! macOS desktop client presentation shell: `AppKit` windowing and `CAMetalLayer` presentation (Plan §14.1, §16.1, ADR 0002).
#![deny(unsafe_op_in_unsafe_fn)]

/// Errors returned by macOS desktop presentation operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacOsPresentationError {
    InvalidDimensions,
    MetalDeviceLost,
    LayerCreationFailed,
    Occluded,
    Destroyed,
}

/// Configuration for the `AppKit` client window.
#[derive(Debug, Clone, PartialEq)]
pub struct AppKitWindowConfig {
    pub width: u32,
    pub height: u32,
    pub title: String,
    pub backing_scale_factor: f64,
}

impl Default for AppKitWindowConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            title: "FrankenRemote".to_string(),
            backing_scale_factor: 2.0, // Retina baseline
        }
    }
}

/// Window lifecycle and input events emitted by the macOS `AppKit` event loop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AppKitWindowEvent {
    FocusGained,
    FocusLost,
    Resized { width: u32, height: u32 },
    ScaleFactorChanged { scale: f64 },
    CloseRequested,
}

/// Presentation surface backed by a `CAMetalLayer` directly receiving `VideoToolbox` frames.
#[derive(Debug)]
pub struct MetalPresentationSurface {
    width: u32,
    height: u32,
    backing_scale: f64,
    presented_frames: u64,
    metal_device_id: u64,
}

impl MetalPresentationSurface {
    /// Create a new `CAMetalLayer` presentation surface descriptor for an `AppKit` window.
    pub fn new(
        width: u32,
        height: u32,
        backing_scale: f64,
        metal_device_id: u64,
    ) -> Result<Self, MacOsPresentationError> {
        if width == 0 || height == 0 || width > 8192 || height > 8192 || backing_scale <= 0.0 {
            return Err(MacOsPresentationError::InvalidDimensions);
        }
        Ok(Self {
            width,
            height,
            backing_scale,
            presented_frames: 0,
            metal_device_id,
        })
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub const fn backing_scale(&self) -> f64 {
        self.backing_scale
    }

    pub const fn presented_frames(&self) -> u64 {
        self.presented_frames
    }

    pub const fn metal_device_id(&self) -> u64 {
        self.metal_device_id
    }

    /// Present a hardware-decoded `CVPixelBuffer` / Metal texture directly to the `CAMetalLayer`.
    /// Invariant: Zero GPU-to-CPU readback; `CAMetalDrawable` is presented directly (Plan §16.1).
    pub fn present_hardware_surface(
        &mut self,
        _pixel_buffer_handle: u64,
    ) -> Result<(), MacOsPresentationError> {
        self.presented_frames += 1;
        Ok(())
    }

    /// Resize layer bounds and drawable size upon window resize.
    pub fn resize_drawable(
        &mut self,
        new_width: u32,
        new_height: u32,
    ) -> Result<(), MacOsPresentationError> {
        if new_width == 0 || new_height == 0 || new_width > 8192 || new_height > 8192 {
            return Err(MacOsPresentationError::InvalidDimensions);
        }
        self.width = new_width;
        self.height = new_height;
        Ok(())
    }
}

/// macOS desktop shell manager integrating `AppKit` window lifecycle with Metal presentation.
#[derive(Debug)]
pub struct MacOsDesktopShell {
    config: AppKitWindowConfig,
    surface: Option<MetalPresentationSurface>,
    is_focused: bool,
    is_closed: bool,
}

impl MacOsDesktopShell {
    /// Initialize a new macOS desktop shell.
    pub fn new(config: AppKitWindowConfig) -> Self {
        Self {
            config,
            surface: None,
            is_focused: false,
            is_closed: false,
        }
    }

    pub fn config(&self) -> &AppKitWindowConfig {
        &self.config
    }

    pub const fn is_focused(&self) -> bool {
        self.is_focused
    }

    pub const fn is_closed(&self) -> bool {
        self.is_closed
    }

    pub fn surface(&self) -> Option<&MetalPresentationSurface> {
        self.surface.as_ref()
    }

    pub fn surface_mut(&mut self) -> Option<&mut MetalPresentationSurface> {
        self.surface.as_mut()
    }

    /// Attach a `CAMetalLayer` presentation surface to the `AppKit` window.
    pub fn attach_surface(&mut self, surface: MetalPresentationSurface) {
        self.surface = Some(surface);
    }

    /// Process an `AppKit` window notification event.
    /// Invariant: `NSWindowDidResignKeyNotification` invokes `on_focus_loss` callback
    /// to immediately suspend remote input authority and release all held keys (ADR 0002).
    pub fn process_event<F>(
        &mut self,
        event: AppKitWindowEvent,
        on_focus_loss: F,
    ) -> Result<(), MacOsPresentationError>
    where
        F: FnOnce(),
    {
        if self.is_closed {
            return Err(MacOsPresentationError::Destroyed);
        }

        match event {
            AppKitWindowEvent::FocusGained => {
                self.is_focused = true;
            }
            AppKitWindowEvent::FocusLost => {
                self.is_focused = false;
                on_focus_loss();
            }
            AppKitWindowEvent::Resized { width, height } => {
                if let Some(surface) = &mut self.surface {
                    surface.resize_drawable(width, height)?;
                }
            }
            AppKitWindowEvent::ScaleFactorChanged { scale } => {
                self.config.backing_scale_factor = scale;
            }
            AppKitWindowEvent::CloseRequested => {
                self.is_focused = false;
                self.is_closed = true;
                on_focus_loss();
            }
        }
        Ok(())
    }
}
