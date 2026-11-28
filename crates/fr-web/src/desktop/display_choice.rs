//! One immutable approved catalog and one full-display choice. The DOM gets
//! bounded row metadata; aliases, geometry generations and wire bytes stay here.
use super::{ControlPurpose, DesktopSession};
use crate::{BrowserSessionCore, adapter::BrowserAdapterError as Error};
use fr_wire::{
    decoder,
    display::{Catalog, Display, Message as DisplayMessage},
    input::{
        InputDelivery::Reliable,
        InputDirection::{HostToViewer, ViewerToHost},
    },
    negotiation::browser,
};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DisplayOptionSnapshot {
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DisplayCatalogSnapshot {
    /// Metadata for rendering identity, never a JavaScript selection input.
    pub revision: String,
    pub displays: Vec<DisplayOptionSnapshot>,
}

#[derive(Default)]
pub(super) struct DisplayChoice {
    catalog: Option<Catalog>,
    selected: Option<Display>,
    queued: bool,
}
impl DisplayChoice {
    pub(super) fn pending(&self) -> bool {
        self.catalog.is_some() && self.selected.is_none()
    }
    pub(super) fn snapshot(&self) -> Option<DisplayCatalogSnapshot> {
        let catalog = self.catalog.filter(|_| self.pending())?;
        Some(DisplayCatalogSnapshot {
            revision: catalog.revision().to_string(),
            displays: catalog
                .displays()
                .iter()
                .enumerate()
                .map(|(index, display)| DisplayOptionSnapshot {
                    index: u32::try_from(index).expect("catalog has at most eight rows"),
                    width: display.pixel_width,
                    height: display.pixel_height,
                    x: display.x,
                    y: display.y,
                })
                .collect(),
        })
    }
    pub(super) fn queued(&mut self) -> Result<(), Error> {
        if self.selected.is_none() || self.queued {
            return Err(Error::InvalidDisplay);
        }
        self.queued = true;
        Ok(())
    }
    pub(super) fn accepts(&self, binding: decoder::Binding) -> bool {
        self.queued
            && self.selected.is_some_and(|display| {
                display.handle == binding.display && display.geometry == binding.geometry
            })
    }
    pub(super) fn accepts_picture(&self, configuration: decoder::Configuration<'_>) -> bool {
        self.queued
            && self.selected.is_some_and(|display| {
                // Coded padding is allowed, but the visible picture must be the
                // entire native-size display. No scaled-picture capability is offered.
                display.pixel_width == configuration.crop_width
                    && display.pixel_height == configuration.crop_height
            })
    }
}

impl DesktopSession {
    pub fn awaiting_display(&self) -> bool {
        !self.closed && self.display_choice.pending()
    }
    pub fn display_catalog(&self) -> Option<DisplayCatalogSnapshot> {
        if self.closed {
            None
        } else {
            self.display_choice.snapshot()
        }
    }
    pub(super) fn display_selection_enabled(&self) -> bool {
        self.selected
            .as_ref()
            .is_some_and(browser::display_selection_selected)
    }
    pub(super) fn receive_catalog(
        &mut self,
        core: &mut BrowserSessionCore,
        bytes: &[u8],
    ) -> Result<(), Error> {
        if !self.binding_queued
            || !self.display_selection_enabled()
            || self.display_choice.catalog.is_some()
            || self.descriptor.is_some()
        {
            return Err(Error::InvalidDisplay);
        }
        let DisplayMessage::Catalog(catalog) = fr_wire::display::decode(
            bytes,
            self.parent()?,
            &self.limits(),
            HostToViewer,
            Reliable,
        )
        .map_err(|_| Error::InvalidDisplay)?
        else {
            return Err(Error::InvalidDisplay);
        };
        if catalog.is_empty() {
            return Err(Error::NoDisplays);
        }
        core.on_display_catalog(&catalog)
            .map_err(|_| Error::InvalidDisplay)?;
        self.display_choice.catalog = Some(catalog);
        if catalog.len() == 1 {
            self.select_display(core, 0)?;
        }
        Ok(())
    }
    pub fn select_display(
        &mut self,
        core: &mut BrowserSessionCore,
        index: u32,
    ) -> Result<(), Error> {
        self.live()?;
        if !self.binding_queued
            || !self.display_selection_enabled()
            || !self.display_choice.pending()
        {
            return Err(Error::InvalidDisplay);
        }
        let catalog = self.display_choice.catalog.ok_or(Error::InvalidDisplay)?;
        let display = *catalog
            .displays()
            .get(usize::try_from(index).map_err(|_| Error::InvalidDisplay)?)
            .ok_or(Error::InvalidDisplay)?;
        let choice = catalog
            .selection(display.handle)
            .map_err(|_| Error::InvalidDisplay)?;
        let mut bytes = [0; fr_wire::display::SELECT_BYTES];
        let length = fr_wire::display::encode(
            &DisplayMessage::Select(choice),
            self.parent()?,
            &self.limits(),
            &mut bytes,
            ViewerToHost,
            Reliable,
        )
        .map_err(|_| Error::InvalidDisplay)?;
        core.select_display(display.handle)
            .map_err(|_| Error::InvalidDisplay)?;
        self.queue_control(bytes[..length].to_vec(), ControlPurpose::Display, false)?;
        self.display_choice.selected = Some(display);
        Ok(())
    }
}
