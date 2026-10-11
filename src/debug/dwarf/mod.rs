pub mod die;
pub mod serde;
pub mod unit;
pub use die::*;
pub use unit::*;

use anyhow::Result;
use gimli::{DwarfSections, EndianRcSlice, LittleEndian, SectionId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use crate::util::weak_error;

/// The reader type we use any time we interface with `gimli`.
pub type R = EndianRcSlice<LittleEndian>;

#[derive(Debug)]
pub struct Dwarf {
    /// Provides direct access to `gimli`
    inner: gimli::Dwarf<R>,
    /// DWARF sections maintained for serialization.
    /// References same memory as `inner`.
    sections: gimli::DwarfSections<R>,
    /// List of dwarf unit wrappers
    units: Vec<Unit>,
}

impl Clone for Dwarf {
    fn clone(&self) -> Self {
        let sections = gimli::DwarfSections {
            debug_abbrev: self.sections.debug_abbrev.clone(),
            debug_addr: self.sections.debug_addr.clone(),
            debug_aranges: self.sections.debug_aranges.clone(),
            debug_info: self.sections.debug_info.clone(),
            debug_line: self.sections.debug_line.clone(),
            debug_line_str: self.sections.debug_line_str.clone(),
            debug_macinfo: self.sections.debug_macinfo.clone(),
            debug_macro: self.sections.debug_macro.clone(),
            debug_names: self.sections.debug_names.clone(),
            debug_str: self.sections.debug_str.clone(),
            debug_str_offsets: self.sections.debug_str_offsets.clone(),
            debug_types: self.sections.debug_types.clone(),
            debug_loc: self.sections.debug_loc.clone(),
            debug_loclists: self.sections.debug_loclists.clone(),
            debug_ranges: self.sections.debug_ranges.clone(),
            debug_rnglists: self.sections.debug_rnglists.clone(),
        };

        let inner = sections.borrow(|s| s.clone());

        Self {
            units: self.units.iter().map(|u| u.clone(&inner)).collect(),
            inner,
            sections,
        }
    }
}

impl Dwarf {
    /// Load Dwarf from section map
    pub fn from_sections(sections: &HashMap<&str, &[u8]>) -> Result<Self> {
        let sections = DwarfSections::load(|id: SectionId| -> Result<R, gimli::Error> {
            let data = sections.get(id.name()).copied().unwrap_or(&[]);
            Ok(EndianRcSlice::new(Rc::from(data), LittleEndian))
        })?;

        let inner = sections.borrow(|section| section.clone());

        let mut parser = UnitParser::new(&inner);
        let units = inner
            .units()
            .map(|header| parser.parse(weak_error!(header)?))
            .flatten()
            .collect();

        Ok(Self {
            inner,
            sections,
            units,
        })
    }

    pub fn units(&self) -> &[Unit] {
        &self.units
    }

    /// Gets all locations across all compilation units.
    /// This list is retrieved directly from the DWARF and represents all locations known to the compiler.
    /// For the list of locations for which a breakpoint can be set, see [DebugInfo::locations]
    pub fn locations(&self) -> impl Iterator<Item = Location> {
        self.units.iter().flat_map(|u| u.locations())
    }

    /// Gets the path for the file with the given index.
    /// Returns [None] if the index does not resolve to any unit's file table.
    /// This happens when `wasm-ld` deduplicates COMDAT sections: the dropped
    /// translation units' DWARF survives in the output but their file tables
    /// do not, leaving stale indices behind.
    pub fn file_at(&self, index: usize) -> Option<&PathBuf> {
        self.units.iter().find_map(|u| {
            let local_index = index.checked_sub(u.properties().file_offset)?;
            u.file_at(local_index)
        })
    }
}
