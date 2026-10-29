//! `std::vector` formatter.

use std::ops::Range;

use anyhow::{Context, Result, ensure};

use crate::debug::formatters::default::StructureFormatter;
use crate::debug::formatters::{ChildCounts, VariableFormatter};
use crate::debug::{Type, Variable};

pub struct StdVectorFormatter;

struct VectorData {
    begin: Variable,
    count: usize,
    elem_ty: Type,
    elem_size: usize,
}

impl StdVectorFormatter {
    fn data(value: &Variable) -> Result<VectorData> {
        let begin = value
            .child_with_name("__begin_")
            .context("No child named '__begin_'")?;
        let end = value
            .child_with_name("__end_")
            .context("No child named '__end_'")?;

        let begin_addr = begin
            .pointer_value()
            .context("std::vector __begin_ is unavailable")?;
        let end_addr = end
            .pointer_value()
            .context("std::vector __end_ is unavailable")?;

        let elem_ty = begin
            .ty()
            .pointee()
            .context("std::vector element type is unavailable")?;
        let elem_size = elem_ty
            .byte_size()
            .context("std::vector element size is unavailable")? as usize;

        let bytes = u64::from(end_addr)
            .checked_sub(u64::from(begin_addr))
            .context("std::vector end precedes begin")?;
        ensure!(elem_size > 0 && bytes % elem_size as u64 == 0, "Invalid std::vector element range");
        ensure!(u64::from(end_addr) <= value.debugger().context("no debugger")?.memory().byte_size() as u64,
            "std::vector storage is outside memory");
        ensure!(bytes == 0 || !begin_addr.is_null(), "std::vector storage is null");
        Ok(VectorData {
            begin: begin.clone(),
            count: bytes as usize / elem_size,
            elem_ty,
            elem_size,
        })
    }
}

impl VariableFormatter for StdVectorFormatter {
    fn matches(&self, ty: &Type) -> bool {
        ty.matches("^std::vector<.+>$") && !ty.matches("^std::vector<bool[,>]")
    }

    fn display(&self, value: &Variable) -> Result<String> {
        StructureFormatter.display(value)
    }

    fn num_children(&self, value: &Variable) -> Result<ChildCounts> {
        let data = Self::data(value)?;
        Ok(ChildCounts::indexed(data.count))
    }

    fn indexed_children(&self, value: &Variable, range: Range<usize>) -> Result<Vec<Variable>> {
        let data = Self::data(value)?;
        let start = range.start.min(data.count);
        let end = range.end.min(data.count);
        ensure!(end.saturating_sub(start) <= 10_000, "std::vector page exceeds 10000 elements");

        let mut result = Vec::with_capacity(end.saturating_sub(start));
        for i in start..end {
            result.push(
                data.begin
                    .child_at_offset((i * data.elem_size) as isize)
                    .with_name(&format!("[{i}]"))
                    .with_type(&data.elem_ty),
            );
        }
        Ok(result)
    }

    fn named_children(&self, _value: &Variable, _range: Range<usize>) -> Result<Vec<Variable>> {
        Ok(Vec::new())
    }
}
