//! libc++ deque: map pointers and element types come from DWARF, not fixed offsets.

use std::ops::Range;

use anyhow::{Context, Result, ensure};

use crate::debug::formatters::default::StructureFormatter;
use crate::debug::formatters::{ChildCounts, VariableFormatter};
use crate::debug::{Type, Variable};

pub struct StdDequeFormatter;

struct DequeData {
    begin: Variable,
    element_type: Type,
    element_size: usize,
    block_size: usize,
    start: usize,
    count: usize,
}

impl DequeData {
    fn new(value: &Variable) -> Result<Self> {
        let mut map = value
            .child_with_name("__map_")
            .context("std::deque __map_ is unavailable")?;
        // libc++ 22 moved these members into a base class. Accept its DWARF
        // layout only when it is the expected non-virtual base at offset zero.
        if map.child_with_name("__begin_").is_none() {
            let ty = map.ty().skip_modifiers();
            let base = ty
                .die()?
                .find_children(|die| {
                    if die.tag() != gimli::DW_TAG_inheritance
                        || die.attr_value(gimli::DW_AT_data_member_location)
                            != Some(gimli::AttributeValue::Udata(0))
                    {
                        return None;
                    }
                    let base = ty.child(die.type_ref()?);
                    base.matches("^std::__split_buffer_pointer_layout<.+>$")
                        .then_some(base)
                })
                .context("Unsupported std::deque map layout")?;
            map = map.with_type(&base);
        }
        let begin = map
            .child_with_name("__begin_")
            .context("std::deque map begin is unavailable")?;
        let end = map
            .child_with_name("__end_")
            .and_then(|v| v.pointer_value())
            .context("std::deque map end is unavailable")?;
        let begin_address = begin
            .pointer_value()
            .context("std::deque map begin is unavailable")?;
        let pointer_type = begin
            .ty()
            .pointee()
            .context("std::deque block type is unavailable")?;
        let pointer_size = pointer_type
            .byte_size()
            .context("std::deque pointer size is unavailable")? as usize;
        let element_type = pointer_type
            .pointee()
            .context("std::deque element type is unavailable")?;
        let element_size = element_type
            .byte_size()
            .context("std::deque element size is unavailable")? as usize;
        ensure!(
            element_size > 0 && pointer_size == 4,
            "Unsupported std::deque element layout"
        );
        let block_size = if element_size < 256 {
            4096 / element_size
        } else {
            16
        };
        let map_bytes = u64::from(end)
            .checked_sub(u64::from(begin_address))
            .context("std::deque map end precedes begin")? as usize;
        let memory_size = value
            .debugger()
            .context("no debugger")?
            .memory()
            .byte_size();
        ensure!(
            map_bytes % pointer_size == 0 && u64::from(end) <= memory_size as u64,
            "Invalid std::deque map range"
        );
        ensure!(
            map_bytes == 0 || !begin_address.is_null(),
            "std::deque map is null"
        );
        let start = value
            .child_with_name("__start_")
            .and_then(|v| v.unsigned_value())
            .context("std::deque start is unavailable")? as usize;
        let count = value
            .child_with_name("__size_")
            .and_then(|v| v.unsigned_value())
            .context("std::deque size is unavailable")? as usize;
        let capacity = (map_bytes / pointer_size)
            .checked_mul(block_size)
            .context("std::deque capacity overflow")?;
        ensure!(
            start.checked_add(count).is_some_and(|end| end <= capacity)
                && count <= memory_size / element_size
                && (count == 0 || !begin_address.is_null()),
            "Invalid std::deque size or start"
        );
        Ok(Self {
            begin,
            element_type,
            element_size,
            block_size,
            start,
            count,
        })
    }
}

impl VariableFormatter for StdDequeFormatter {
    fn matches(&self, ty: &Type) -> bool {
        ty.matches("^std::deque<.+>$")
    }

    fn display(&self, value: &Variable) -> Result<String> {
        StructureFormatter.display(value)
    }

    fn num_children(&self, value: &Variable) -> Result<ChildCounts> {
        Ok(ChildCounts::indexed(DequeData::new(value)?.count))
    }

    fn indexed_children(&self, value: &Variable, range: Range<usize>) -> Result<Vec<Variable>> {
        let data = DequeData::new(value)?;
        let start = range.start.min(data.count);
        let end = range.end.min(data.count);
        ensure!(
            end.saturating_sub(start) <= 10_000,
            "std::deque page exceeds 10000 elements"
        );
        let pointer_type = data
            .begin
            .ty()
            .pointee()
            .context("std::deque block type is unavailable")?;
        let memory_size = value
            .debugger()
            .context("no debugger")?
            .memory()
            .byte_size() as u64;
        (start..end)
            .map(|index| {
                let position = data.start + index;
                let block = data
                    .begin
                    .child_at_offset((position / data.block_size * 4) as isize)
                    .with_type(&pointer_type);
                let address = block
                    .pointer_value()
                    .context("std::deque block is unavailable")?;
                let offset = position % data.block_size * data.element_size;
                ensure!(
                    !address.is_null()
                        && u64::from(address) + offset as u64 + data.element_size as u64
                            <= memory_size,
                    "std::deque element is outside memory"
                );
                Ok(block
                    .child_at_offset(offset as isize)
                    .with_type(&data.element_type)
                    .with_name(&format!("[{index}]")))
            })
            .collect()
    }

    fn named_children(&self, _: &Variable, _: Range<usize>) -> Result<Vec<Variable>> {
        Ok(Vec::new())
    }
}
