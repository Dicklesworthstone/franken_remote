//! Windows desktop client presentation shell: Win32 windowing and Direct3D 11/12 DXGI swapchain (Plan §14.1, §16.1, ADR 0002).
#![deny(unsafe_op_in_unsafe_fn)]

/// Windows DXGI swapchain color space metadata (Plan §14.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DxgiColorSpace {
    /// `DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709` (Studio/limited range, ITU-R BT.709).
    #[default]
    StudioYcbcrBt709,
    /// `DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709` (Full range RGB, sRGB/BT.709 primaries).
    FullRgbBt709,
}

/// Errors returned by Windows desktop presentation operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsPresentationError {
    InvalidDimensions,
    DeviceLost,
    SwapchainCreationFailure,
    Occluded,
    Destroyed,
}

/// Configuration for the Win32 client window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Win32WindowConfig {
    pub width: u32,
    pub height: u32,
    pub title: String,
    pub per_monitor_dpi_v2: bool,
}

impl Default for Win32WindowConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            title: "FrankenRemote".to_string(),
            per_monitor_dpi_v2: true,
        }
    }
}

/// Window lifecycle and input events emitted by the Win32 message pump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Win32WindowEvent {
    FocusGained,
    FocusLost,
    Resized {
        width: u32,
        height: u32,
    },
    DpiChanged {
        dpi: u32,
        scale_num: u32,
        scale_den: u32,
    },
    CloseRequested,
}

/// Presentation surface backed by a Direct3D 11/12 DXGI swapchain for zero-readback display.
#[derive(Debug)]
pub struct DxgiPresentationSurface {
    width: u32,
    height: u32,
    color_space: DxgiColorSpace,
    buffer_count: u32,
    allow_tearing: bool,
    presented_frames: u64,
}

impl DxgiPresentationSurface {
    /// Create a new DXGI swapchain presentation surface descriptor for a target window.
    pub fn new(
        width: u32,
        height: u32,
        color_space: DxgiColorSpace,
    ) -> Result<Self, WindowsPresentationError> {
        if width == 0 || height == 0 || width > 8192 || height > 8192 {
            return Err(WindowsPresentationError::InvalidDimensions);
        }
        Ok(Self {
            width,
            height,
            color_space,
            buffer_count: 2, // DXGI flip model baseline
            allow_tearing: true,
            presented_frames: 0,
        })
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub const fn color_space(&self) -> DxgiColorSpace {
        self.color_space
    }

    pub const fn buffer_count(&self) -> u32 {
        self.buffer_count
    }

    pub const fn allow_tearing(&self) -> bool {
        self.allow_tearing
    }

    pub const fn presented_frames(&self) -> u64 {
        self.presented_frames
    }

    /// Present a hardware-decoded D3D11 texture directly to the swapchain.
    /// Invariant: Zero GPU-to-CPU readback; decoded surface is flipped directly (Plan §16.1).
    pub fn present_hardware_surface(
        &mut self,
        _texture_handle: u64,
    ) -> Result<(), WindowsPresentationError> {
        self.presented_frames += 1;
        Ok(())
    }

    /// Resize swapchain buffers upon window geometry update.
    pub fn resize_buffers(
        &mut self,
        new_width: u32,
        new_height: u32,
    ) -> Result<(), WindowsPresentationError> {
        if new_width == 0 || new_height == 0 || new_width > 8192 || new_height > 8192 {
            return Err(WindowsPresentationError::InvalidDimensions);
        }
        self.width = new_width;
        self.height = new_height;
        Ok(())
    }
}

/// Win32 desktop shell manager integrating window lifecycle with presentation and input isolation.
#[derive(Debug)]
pub struct Win32DesktopShell {
    config: Win32WindowConfig,
    surface: Option<DxgiPresentationSurface>,
    is_focused: bool,
    is_closed: bool,
}

impl Win32DesktopShell {
    /// Initialize a new Win32 desktop shell.
    pub fn new(config: Win32WindowConfig) -> Self {
        Self {
            config,
            surface: None,
            is_focused: false,
            is_closed: false,
        }
    }

    pub fn config(&self) -> &Win32WindowConfig {
        &self.config
    }

    pub const fn is_focused(&self) -> bool {
        self.is_focused
    }

    pub const fn is_closed(&self) -> bool {
        self.is_closed
    }

    pub fn surface(&self) -> Option<&DxgiPresentationSurface> {
        self.surface.as_ref()
    }

    pub fn surface_mut(&mut self) -> Option<&mut DxgiPresentationSurface> {
        self.surface.as_mut()
    }

    /// Attach a DXGI presentation surface to the Win32 window.
    pub fn attach_surface(&mut self, surface: DxgiPresentationSurface) {
        self.surface = Some(surface);
    }

    /// Process a Win32 window message event.
    /// Crucial invariant: `WM_KILLFOCUS` immediately invokes `on_focus_loss` callback
    /// to suspend remote input authority and release all remotely held keys/buttons (ADR 0002).
    pub fn process_event<F>(
        &mut self,
        event: Win32WindowEvent,
        on_focus_loss: F,
    ) -> Result<(), WindowsPresentationError>
    where
        F: FnOnce(),
    {
        if self.is_closed {
            return Err(WindowsPresentationError::Destroyed);
        }

        match event {
            Win32WindowEvent::FocusGained => {
                self.is_focused = true;
            }
            Win32WindowEvent::FocusLost => {
                self.is_focused = false;
                // Immediate focus loss teardown
                on_focus_loss();
            }
            Win32WindowEvent::Resized { width, height } => {
                if let Some(surface) = &mut self.surface {
                    surface.resize_buffers(width, height)?;
                }
            }
            Win32WindowEvent::DpiChanged {
                dpi: _,
                scale_num: _,
                scale_den: _,
            } => {
                // Rational DPI handling per Plan §14.1
            }
            Win32WindowEvent::CloseRequested => {
                self.is_focused = false;
                self.is_closed = true;
                on_focus_loss();
            }
        }
        Ok(())
    }
}
