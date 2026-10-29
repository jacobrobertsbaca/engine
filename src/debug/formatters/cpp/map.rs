//! `std::map` and `std::set` formatters (`std::__tree`).
//!
//! See the corresponding [LLDB formatter code](https://github.com/llvm/llvm-project/blob/main/lldb/source/Plugins/Language/CPlusPlus/LibCxxMap.cpp)
//! for reference.

// The flattened layout of the std::__tree_iterator::__ptr_ looks
// as follows:
//
// The following shows the contiguous block of memory:
//
//        +-----------------------------+ class __tree_end_node
// __ptr_ | pointer __left_;            |
//        +-----------------------------+ class __tree_node_base
//        | pointer __right_;           |
//        | __parent_pointer __parent_; |
//        | bool __is_black_;           |
//        +-----------------------------+ class __tree_node
//        | __node_value_type __value_; | <<< our key/value pair
//        +-----------------------------+
//
// where __ptr_ has type __iter_pointer.

use std::ops::Range;

use anyhow::{Context, Result};

use crate::debug::formatters::default::StructureFormatter;
use crate::debug::formatters::{ChildCounts, VariableFormatter};
use crate::debug::{Type, Variable};

// ╭──────────────────────────────────────────────────────────────────────────╮
// │ MapEntry                                                                 │
// ╰──────────────────────────────────────────────────────────────────────────╯

/// Wraps a variable representing a pointer to a tree node.
#[derive(Clone)]
struct MapEntry(Variable);

impl MapEntry {
    fn value(&self) -> u64 {
        self.0.unsigned_value().unwrap_or(0)
    }

    fn null(&self) -> bool {
        self.value() == 0
    }

    fn left(&self) -> Option<MapEntry> {
        if self.null() {
            return None;
        };

        Some(MapEntry(self.0.child_at_offset(0)))
    }

    fn right(&self) -> Option<MapEntry> {
        if self.null() {
            return None;
        };

        let addr_size = self.0.debugger()?.address_size();
        Some(MapEntry(self.0.child_at_offset(addr_size as isize)))
    }

    fn parent(&self) -> Option<MapEntry> {
        if self.null() {
            return None;
        };

        let addr_size = self.0.debugger()?.address_size();
        Some(MapEntry(self.0.child_at_offset(2 * addr_size as isize)))
    }

    fn is_left_child(&self) -> bool {
        if self.null() {
            return false;
        }

        let Some(parent) = self.parent() else {
            return false;
        };

        let Some(left) = parent.left() else {
            return false;
        };

        self.value() == left.value()
    }
}

// ╭──────────────────────────────────────────────────────────────────────────╮
// │ TreeIterator                                                             │
// ╰──────────────────────────────────────────────────────────────────────────╯

struct TreeIterator {
    current: MapEntry,
    value_type: Type,
    size: usize,
    remaining_steps: usize,
}

impl TreeIterator {
    fn new(container: &Variable) -> Result<Self> {
        let tree = container
            .child_with_name("__tree_")
            .context("No child named '__tree_'")?;

        let begin_node = tree
            .child_with_name("__begin_node_")
            .context("No child named '__begin_node_'")?;

        let value_type = tree
            .ty()
            .skip_modifiers()
            .direct_nested_type_with_name("value_type")?
            .skip_modifiers();

        let size = tree
            .child_with_name("__size_")
            .context("No child named '__size_'")?
            .unsigned_value()
            .context("Could not read __size_")? as usize;
        anyhow::ensure!(size <= container.debugger().context("no debugger")?.memory().byte_size() / 16,
            "Invalid std::map size");

        Ok(Self {
            current: MapEntry(begin_node),
            value_type,
            size,
            remaining_steps: 10_000,
        })
    }

    fn value(&self) -> Variable {
        // Note: 16 is the offset from the start of the node to
        // the beginning of its stored value
        //
        // TODO: We should not assume this offset, instead it would
        // be better to cast to the node type and do `child_with_name`
        // like LLDB does
        self.current.0.child_at_offset(16)
    }

    fn next(&mut self) -> Result<()> {
        self.remaining_steps = self.remaining_steps.checked_sub(1)
            .context("std::map traversal exceeds 10000 steps")?;
        if self.current.null() {
            return Ok(());
        }

        let right = self.current.right();

        if let Some(right) = right {
            if !right.null() {
                self.current = self.tree_min(&right)?;
                return Ok(());
            }
        }

        let mut steps = 0;
        while !self.current.is_left_child() {
            let Some(parent) = self.current.parent() else {
                return Ok(());
            };
            self.current = parent;
            steps += 1;
            self.remaining_steps = self.remaining_steps.checked_sub(1)
                .context("std::map traversal exceeds 10000 steps")?;
            anyhow::ensure!(steps <= self.size, "Invalid std::map parent cycle");
        }

        if let Some(parent) = self.current.parent() {
            self.current = parent;
        }
        Ok(())
    }

    fn tree_min(&mut self, entry: &MapEntry) -> Result<MapEntry> {
        let mut curr = entry.clone();

        if entry.null() {
            return Ok(curr);
        }

        let mut steps = 0;
        while let Some(left) = curr.left().filter(|e| !e.null()) {
            curr = left;
            steps += 1;
            self.remaining_steps = self.remaining_steps.checked_sub(1)
                .context("std::map traversal exceeds 10000 steps")?;
            anyhow::ensure!(steps <= self.size, "Invalid std::map child cycle");
        }

        Ok(curr)
    }
}

// ╭──────────────────────────────────────────────────────────────────────────╮
// │ StdMapFormatter                                                          │
// ╰──────────────────────────────────────────────────────────────────────────╯

pub struct StdMapFormatter;

impl VariableFormatter for StdMapFormatter {
    fn matches(&self, ty: &Type) -> bool {
        ty.matches("^std::(set|map)<.+>$")
    }

    fn display(&self, value: &Variable) -> Result<String> {
        StructureFormatter.display(value)
    }

    fn num_children(&self, value: &Variable) -> Result<ChildCounts> {
        let tree = TreeIterator::new(value)?;
        Ok(ChildCounts::indexed(tree.size))
    }

    fn indexed_children(&self, value: &Variable, range: Range<usize>) -> Result<Vec<Variable>> {
        let mut iter = TreeIterator::new(value)?;
        let end = range.end.min(iter.size);
        if range.start >= end {
            return Ok(Vec::new());
        }
        anyhow::ensure!(end <= 10_000, "std::map traversal exceeds 10000 entries");
        for _ in 0..range.start {
            iter.next()?;
        }
        (range.start..end)
            .map(|i| {
                let child = iter
                    .value()
                    .with_name(&format!("[{i}]"))
                    .with_type(&iter.value_type);
                iter.next()?;
                Ok(child)
            })
            .collect()
    }

    fn named_children(&self, _: &Variable, _: Range<usize>) -> Result<Vec<Variable>> {
        Ok(Vec::new())
    }
}
