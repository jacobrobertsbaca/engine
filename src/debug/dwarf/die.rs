use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::util::weak_error;
use crate::{debug::NamespaceHierarchy, types::CodeOffset};

use super::unit::Language;
use super::{Dwarf, R, Unit};
use gimli::Reader;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct DieReference {
    unit_index: usize,
    #[serde(with = "crate::debug::dwarf::serde::unit_offset")]
    unit_ofs: gimli::UnitOffset,
}

impl DieReference {
    pub fn deref<'a>(&self, dwarf: &'a Dwarf) -> Result<Die<'a>> {
        let Some(unit) = dwarf.units.get(self.unit_index) else {
            return Err(anyhow!("Unit index out of bounds"));
        };

        let die = unit.unit().entry(self.unit_ofs)?;

        Ok(Die::new(DerefContext::new(dwarf, unit), die))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DerefContext<'a> {
    pub dwarf: &'a Dwarf,
    pub unit: &'a Unit,
}

impl<'a> DerefContext<'a> {
    #[inline]
    pub fn new(dwarf: &'a Dwarf, unit: &'a Unit) -> Self {
        Self { dwarf, unit }
    }

    #[inline]
    pub(crate) fn unit_ref(self) -> gimli::UnitRef<'a, R> {
        gimli::UnitRef::new(&self.dwarf.inner, self.unit.unit())
    }
}

pub type DieRange = (CodeOffset, CodeOffset);

#[derive(Debug, Clone)]
pub struct DieRanges(Vec<DieRange>);

impl DieRanges {
    pub fn is_empty(&self) -> bool {
        if self.0.is_empty() {
            return true;
        }

        !self.0.iter().any(|(begin, end)| end > begin)
    }

    /// Returns the sole range, or [`None`] if there are zero or more than one.
    pub fn single(&self) -> Option<DieRange> {
        match self.0.as_slice() {
            &[range] => Some(range),
            _ => None,
        }
    }

    /// Returns whether `pc` falls in any of these ranges.
    pub fn contains(&self, pc: CodeOffset) -> bool {
        let addr = u64::from(pc);
        self.0
            .iter()
            .any(|(begin, end)| addr >= u64::from(*begin) && addr < u64::from(*end))
    }
}

pub struct Die<'a> {
    ctx: DerefContext<'a>,
    die: gimli::DebuggingInformationEntry<R>,
}

impl<'a> std::ops::Deref for Die<'a> {
    type Target = gimli::DebuggingInformationEntry<R>;

    fn deref(&self) -> &Self::Target {
        &self.die
    }
}

impl<'a> Die<'a> {
    pub(crate) fn new(ctx: DerefContext<'a>, die: gimli::DebuggingInformationEntry<R>) -> Self {
        Self { ctx, die }
    }

    #[inline]
    pub fn ctx(&self) -> &DerefContext<'_> {
        &self.ctx
    }

    pub fn name(&self) -> Option<String> {
        self.attr_to_string(gimli::DW_AT_name)
    }

    pub fn decl_file(&self) -> Option<&PathBuf> {
        let attr = self.attr(gimli::DW_AT_decl_file)?;
        let index = match attr.value() {
            gimli::AttributeValue::FileIndex(index) => index as usize,
            gimli::AttributeValue::Udata(index) => index as usize,
            _ => return None,
        };

        self.ctx.unit.file_at(index)
    }

    /// Returns whether this entry belongs to the standard library.
    pub fn is_builtin(&self) -> bool {
        if self
            .decl_file()
            .is_some_and(|path| is_builtin_file(self.ctx.unit.language(), path))
        {
            return true;
        }
        let Some(spec) = self.die_ref_attr(gimli::DW_AT_specification) else {
            return false;
        };
        let Ok(spec) = spec.deref(self.ctx.dwarf) else {
            return false;
        };
        spec.decl_file()
            .is_some_and(|path| is_builtin_file(self.ctx.unit.language(), path))
    }

    /// Returns the qualified name of this entry which includes
    /// the namespace chain (e.g. std::copy).
    pub fn qualified_name(&self) -> Option<String> {
        Some(if self.has_attr(gimli::DW_AT_specification) {
            // We will use the specification name if this DIE has a specification.
            // When generating debugging info for member functions, DWARF will include
            // two DIEs for each member function: one for the specification and one for
            // the definition of the member function.
            let spec = self.die_ref_attr(gimli::DW_AT_specification)?;
            let spec_die = weak_error!(spec.deref(self.ctx.dwarf))?;
            let name = spec_die.name()?;
            spec_die.namespace().qualify(&name)
        } else {
            let name = self.name()?;
            self.namespace().qualify(&name)
        })
    }

    pub fn namespace(&self) -> NamespaceHierarchy {
        let mut components = Vec::with_capacity(self.die.depth.max(0) as usize);
        let mut parent = self.parent();

        while let Some(current) = parent {
            if let Some(component) = parse_namespace_component(&current) {
                components.push(component);
            }
            parent = current.parent()
        }

        components.reverse();
        NamespaceHierarchy(components)
    }

    pub fn parent(&self) -> Option<Die<'a>> {
        let parent_ofs = self.ctx.unit.parents.get(&self.die.offset)?;
        let mut die_ref = self.die_ref();
        die_ref.unit_ofs = *parent_ofs;
        weak_error!(die_ref.deref(self.ctx.dwarf))
    }

    pub fn die(&self) -> &gimli::DebuggingInformationEntry<R> {
        &self.die
    }

    pub fn die_ref(&self) -> DieReference {
        DieReference {
            unit_index: self.ctx.unit.index(),
            unit_ofs: self.die.offset,
        }
    }

    /// Returns the code address ranges of this DIE, if any.
    pub fn ranges(&self) -> Option<DieRanges> {
        let unit = self.ctx().unit_ref();
        let mut iter = weak_error!(unit.die_ranges(self.die()))?;
        let mut ranges = Vec::new();
        while let Some(range) = weak_error!(iter.next())? {
            if range.end > range.begin {
                ranges.push((range.begin.into(), range.end.into()));
            }
        }
        if ranges.is_empty() {
            None
        } else {
            Some(DieRanges(ranges))
        }
    }

    /// Gets an attribute as a [DieReference], if possible.
    pub fn die_ref_attr(&self, attr: gimli::DwAt) -> Option<DieReference> {
        let attr = self.attr(attr)?;
        match attr.value() {
            gimli::AttributeValue::UnitRef(offset) => Some(DieReference {
                unit_index: self.ctx.unit.index(),
                unit_ofs: offset,
            }),
            _ => None,
        }
    }

    pub fn type_ref(&self) -> Option<DieReference> {
        self.die_ref_attr(gimli::DW_AT_type)
    }

    /// `true` if the DIE has this flag, `false` otherwise.
    pub fn has_flag(&self, attr: gimli::DwAt) -> bool {
        matches!(
            self.attr_value(attr),
            Some(gimli::AttributeValue::Flag(true))
        )
    }

    pub fn expression(&self, attr: gimli::DwAt, pc: CodeOffset) -> Option<gimli::Expression<R>> {
        let Some(attr) = self.attr_value(attr) else {
            return None;
        };

        let unit = self.ctx().unit_ref();
        let addr: u64 = pc.into();

        match attr {
            gimli::AttributeValue::Exprloc(expr) => Some(expr),
            other => {
                let Some(it) = weak_error!(unit.attr_locations(other))? else {
                    return None;
                };
                for res in it {
                    let entry = weak_error!(res)?;
                    if addr >= entry.range.begin && addr < entry.range.end {
                        return Some(entry.data);
                    }
                }
                None
            }
        }
    }

    pub fn attr_to_string(&self, attr: gimli::DwAt) -> Option<String> {
        weak_error!(
            self.attr(attr)
                .and_then(|attr| self.ctx.unit_ref().attr_string(attr.value()).ok())
                .map(|l| l.to_string_lossy().map(|s| s.to_string()))
                .transpose()
        )
        .unwrap_or(None)
    }

    /// Loop through child DIEs until a value is found, returning it (if any)
    pub fn find_children<T>(&self, mut f: impl FnMut(Die<'a>) -> Option<T>) -> Option<T> {
        let mut tree = weak_error!(self.ctx.unit_ref().entries_tree(Some(self.offset())))?;

        let root = weak_error!(tree.root())?;
        let mut children = root.children();
        while let Some(c) = weak_error!(children.next())? {
            let die = Die::new(self.ctx, c.entry().clone());

            if let Some(r) = f(die) {
                return Some(r);
            }
        }

        None
    }

    /// Loop through all child DIEs, performing an action on each
    pub fn for_each_child(&self, mut f: impl FnMut(Die)) {
        self.find_children::<()>(|die| {
            f(die);
            None
        });
    }

    /// Loop through all children DIEs, collecting a value for each (if any)
    pub fn collect_children<T>(&self, mut f: impl FnMut(Die) -> Option<T>) -> Vec<T> {
        let mut result = vec![];
        self.for_each_child(|die| {
            if let Some(r) = f(die) {
                result.push(r);
            }
        });

        result
    }

    /// Recursively traverses the tree, including this node.
    ///
    /// Accepts a callback `f` whose return value will control traversal.
    /// Each DIE is passed to `f` exactly once: returning [`Visit::Continue`]
    /// queues the node's children for traversal, [`Visit::SkipChildren`]
    /// drops them, and [`Visit::Break`] aborts the walk entirely.
    pub fn traverse(&self, mut f: impl FnMut(Die<'a>) -> Visit) {
        let mut queue = VecDeque::from([self.die.offset()]);

        while let Some(offset) = queue.pop_front() {
            let Some(mut tree) = weak_error!(self.ctx.unit.unit().entries_tree(Some(offset)))
            else {
                return;
            };

            let Some(root) = weak_error!(tree.root()) else {
                return;
            };

            match f(Die::new(self.ctx.clone(), root.entry().clone())) {
                Visit::Continue => {}
                Visit::SkipChildren => continue,
                Visit::Break => return,
            }

            let mut children = root.children();
            while let Some(Some(child)) = weak_error!(children.next()) {
                queue.push_back(child.entry().offset());
            }
        }
    }
}

pub enum Visit {
    /// Traverse the next node, including children
    Continue,
    /// Skip the children of this node
    SkipChildren,
    /// Stop traversal immediately
    Break,
}

fn is_builtin_file(language: Language, path: &PathBuf) -> bool {
    match language {
        Language::C => path.starts_with("/include/wasm32-wasip1"),
        Language::Cpp => {
            path.starts_with("/include/c++/v1") || path.starts_with("/include/wasm32-wasip1")
        }
        Language::Rust => path.starts_with("/rustc") || path.starts_with("/lib/rustlib"),
        Language::Unknown => false,
    }
}

fn parse_namespace_component(die: &Die<'_>) -> Option<String> {
    if matches!(
        die.tag(),
        gimli::DW_TAG_namespace | gimli::DW_TAG_structure_type | gimli::DW_TAG_class_type
    ) {
        // If DW_AT_export_symbols is set to `true`, this represents
        // an inline namespace such as the `__2` in `std::__2`.
        // These shouldn't be included in the namespace chain since they are
        // semantically ignored.
        if die.has_flag(gimli::DW_AT_export_symbols) {
            None
        } else {
            die.name()
        }
    } else {
        None
    }
}
