use std::ops::Range;

use crate::{
    debug::{
        Debugger, EvaluationContext, MemberLocation, ReferenceKind, Type, TypeDeclaration,
        dwarf::{Die, R, Visit},
        formatters::ChildCounts,
    },
    types::{CodeOffset, MemoryOffset},
};

use anyhow::Result;

use gimli::Reader;
use gimli::read::Expression;

/// Gets all live variables at the given PC.
///
/// `die` should be a reference to a subprogram,
/// and `pc` should be an offset within the wasm code segment.
pub fn get_variables<'a>(die: &Die<'a>, pc: CodeOffset) -> Vec<Die<'a>> {
    assert!(
        die.tag() == gimli::DW_TAG_subprogram,
        "get_variables requires subprogram die"
    );

    let mut result: Vec<Die<'a>> = vec![];

    die.traverse(|child| {
        let tag = child.tag();

        match tag {
            // `DW_TAG_variable` is what modern DWARF (and clang) emit for
            // locals; `DW_TAG_local_variable` is a legacy alias kept for
            // older producers.
            gimli::DW_TAG_formal_parameter
            | gimli::DW_TAG_variable
            | gimli::DW_TAG_local_variable => {
                // If this subprogram is a member function, then DW_AT_object_pointer will contain
                // the reference to the DW_TAG_formal_parameter which represents the `this` or `self` parameter.
                // Since this attribute is typically marked as artificial, and we exclude artificial members in
                // the traversal below, we will use this as a special case to make sure that `this` is included.
                if child.has_flag(gimli::DW_AT_artificial)
                    && die
                        .die_ref_attr(gimli::DW_AT_object_pointer)
                        .is_none_or(|op| op != child.die_ref())
                {
                    return Visit::Continue;
                }

                result.push(child);
            }

            _ => {
                if let Some((low, high)) = child.addr_range() {
                    if pc < low || pc >= high {
                        return Visit::SkipChildren;
                    }
                }
            }
        }

        Visit::Continue
    });

    result
}

/// Gets the location expression for a variable at the given PC
pub fn get_location(die: &Die<'_>, pc: CodeOffset) -> Option<Expression<R>> {
    die.expression(gimli::DW_AT_location, pc)
}

/// A typed value backed by one or more DWARF location pieces.
///
/// `pieces` describes where the bytes live (memory address, embedded value,
/// register, …); `ty` describes how to interpret them.
#[derive(Clone)]
pub struct Variable {
    context: EvaluationContext,
    name: String,
    pieces: Vec<gimli::Piece<R>>,
    ty: Type,
}

impl Variable {
    pub fn new(
        context: EvaluationContext,
        name: String,
        pieces: Vec<gimli::Piece<R>>,
        ty: Type,
    ) -> Self {
        Variable {
            context,
            name,
            pieces,
            ty,
        }
    }

    pub fn context(&self) -> &EvaluationContext {
        &self.context
    }

    pub fn debugger(&self) -> Option<&Debugger> {
        self.context.debugger()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn ty(&self) -> &Type {
        &self.ty
    }

    /// Changes the name of this variable.
    pub fn with_name(self, name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..self
        }
    }

    /// Changes the type of this variable.
    pub fn with_type(self, ty: &Type) -> Self {
        Self {
            ty: ty.clone(),
            ..self
        }
    }

    /// Returns the address of this variable.
    /// If this variable has no known address, returns [None].
    pub fn address(&self) -> Option<MemoryOffset> {
        let piece = self.pieces.first()?;
        match &piece.location {
            gimli::Location::Address { address } => Some((*address).into()),
            _ => None,
        }
    }

    /// Reads `len` bytes from the start of this variable's value.
    /// If exactly `len` bytes cannot be read, returns [None].
    pub fn read(&self, len: usize) -> Option<Vec<u8>> {
        // TODO: This function is very important, albeit poorly tested/understood.
        // It will fail under optimizing compilers, which may store values as collections of pieces
        // rather than the simple formats that this method assumes.
        //
        // More cases should be added for multi-piece values.
        // These should be rigorously tested under a test harness.

        // TODO: Handling for multi-piece value, not just `first()`
        let piece = self.pieces.first()?;
        let mut bytes = match &piece.location {
            gimli::Location::Address { address } => {
                let memory = self.debugger()?.memory();
                let end = usize::try_from(*address).ok()?.checked_add(len)?;
                if end > memory.byte_size() {
                    return None;
                }
                memory.read((*address).into(), len)
            }
            gimli::Location::Value { value } => value_to_le_bytes(*value, len),
            gimli::Location::Bytes { value } => value.to_slice().ok()?.to_vec(),
            _ => Vec::default(),
        };

        if bytes.len() < len {
            return None;
        }

        bytes.resize(len, 0);
        Some(bytes)
    }

    /// Get the unsigned value of this variable.
    /// If the value cannot be represented as an unsigned value, returns [None].
    ///
    /// * For signed integral types, returns [None] for negative values.
    /// * For character types, returns the unsigned value of the code point.
    /// * For floating point types, returns [None].
    /// * For pointer types, returns the pointer's value.
    pub fn unsigned_value(&self) -> Option<u64> {
        match self.ty.resolved()? {
            TypeDeclaration::Scalar {
                byte_size,
                encoding,
                ..
            } => {
                let size = *byte_size as usize;
                let bytes = self.read(size)?;
                match *encoding {
                    gimli::DW_ATE_signed | gimli::DW_ATE_signed_char => {
                        let signed = match size {
                            1 => i64::from(bytes[0] as i8),
                            2 => i64::from(i16::from_le_bytes(bytes[..2].try_into().ok()?)),
                            4 => i64::from(i32::from_le_bytes(bytes[..4].try_into().ok()?)),
                            8 => i64::from(i64::from_le_bytes(bytes[..8].try_into().ok()?)),
                            _ => return None,
                        };
                        u64::try_from(signed).ok()
                    }
                    gimli::DW_ATE_unsigned | gimli::DW_ATE_unsigned_char => match size {
                        1 => Some(bytes[0] as u64),
                        2 => Some(u16::from_le_bytes(bytes[..2].try_into().ok()?) as u64),
                        4 => Some(u32::from_le_bytes(bytes[..4].try_into().ok()?) as u64),
                        8 => Some(u64::from_le_bytes(bytes[..8].try_into().ok()?)),
                        _ => None,
                    },
                    gimli::DW_ATE_UTF | gimli::DW_ATE_ASCII if size == 1 => Some(bytes[0] as u64),
                    gimli::DW_ATE_float | gimli::DW_ATE_boolean => None,
                    _ => None,
                }
            }
            TypeDeclaration::Referential {
                kind: ReferenceKind::Pointer,
                ..
            } => self.pointer_value().map(u64::from),
            _ => None,
        }
    }

    /// Returns the actual address stored by a pointer.
    ///
    /// For example, in the following snippet:
    ///
    /// ```cpp
    /// int* x = (int*) 0xBA5EBA11;
    /// ```
    ///
    /// [Variable::pointer_value] would return `Some(MemoryOffset::from(0xBA5EBA11))` for
    /// the [Variable] corresponding to `x`.
    pub fn pointer_value(&self) -> Option<MemoryOffset> {
        if let Some(bytes) = self.read(4) {
            Some(u32::from_le_bytes(bytes.try_into().ok()?).into())
        } else {
            None
        }
    }

    /// Gets a raw child named `name` in this variable.
    /// This method uses the raw variable format, ignoring any formatters.
    ///
    /// For example, for structural types, `name` might be the name of a member within this variable.
    ///
    /// Returns [None] if no such child is found.
    pub fn child_with_name(&self, name: &str) -> Option<Variable> {
        let member = match self.ty.resolved() {
            Some(TypeDeclaration::Structure { members, .. }) => members
                .iter()
                .find(|member| member.name.as_deref() == Some(name))?,
            _ => return None,
        };

        // TODO: This code is duplicated by the structure formatter
        // Eventually might want to coalesce this somehow if the logic gets more complicated
        let offset = match &member.location {
            Some(MemberLocation::Constant(value)) => *value,
            Some(MemberLocation::Expr(expr)) => {
                self.context().evaluate_signed(expr.clone()).ok()?
            }
            None => 0,
        };

        Some(
            self.child_at_offset(offset as isize)
                .with_name(name)
                .with_type(&self.ty().child(member.ty)),
        )
    }

    /// Returns a child of this variable with byte offset `offset`.
    /// This method uses the raw variable format, ignoring any formatters.
    ///
    /// The returned variable will have an empty name and the same type as this one.
    /// You may change the resulting name and type using [Variable::with_name] and [Variable::with_type].
    ///
    /// Roughly speaking, the offsets correspond to the same as "space" as the variable's children.
    /// E.g. an `int* ptr`'s `child_at_offset(0)` would return `ptr[0]`, a structured variable
    /// `my_struct_t x`'s `child_at_offset(0)` would return the first member, and so on.
    pub fn child_at_offset(&self, offset: isize) -> Variable {
        // TODO: This function is very important, albeit poorly tested/understood.
        // It will fail under optimizing compilers, which may store values as collections of pieces
        // rather than the simple formats that this method assumes.
        //
        // More cases should be added for multi-piece values.
        // These should be rigorously tested under a test harness.
        //
        // Also need better testing for negative offsets

        // Dereference pointer types
        let mut pieces = match self.ty.resolved() {
            Some(TypeDeclaration::Referential { .. }) => self
                .pointer_value()
                .filter(|a| !a.is_null())
                .map(|a| a.pieces())
                .unwrap_or_default(),
            _ => self.pieces.clone(),
        };

        // Skip `ofs` bytes through the piece vector
        let mut ofs = offset.max(0) as u64;
        while let Some(p) = pieces.first() {
            let Some(bits) = p.size_in_bits else { break };
            let piece_bytes = ((bits) + 7) / 8;
            if ofs < piece_bytes {
                break;
            }
            ofs -= piece_bytes;
            pieces.remove(0);
        }

        // Add `ofs` bytes to the first piece
        if let Some(p) = pieces.first_mut() {
            let adjustment = if offset >= 0 {
                ofs as i64
            } else {
                offset as i64
            };
            match &mut p.location {
                gimli::Location::Address { address } => {
                    *address = address.wrapping_add_signed(adjustment);
                }
                _ => {
                    p.bit_offset = Some(
                        p.bit_offset
                            .unwrap_or(0)
                            .wrapping_add_signed(adjustment.saturating_mul(8)),
                    )
                }
            }
        }

        // Clone the variable with the new contents
        Self {
            context: self.context.clone(),
            name: Default::default(),
            pieces,
            ty: self.ty.clone(),
        }
    }

    pub fn display(&self) -> Result<String> {
        match self.ty.formatter() {
            Some(formatter) => formatter.display(self),
            None => Ok("<unknown>".into()),
        }
    }

    pub fn num_children(&self) -> Result<ChildCounts> {
        match self.ty.formatter() {
            Some(formatter) => formatter.num_children(self),
            None => Ok(Default::default()),
        }
    }

    pub fn indexed_children(&self, range: Range<usize>) -> Result<Vec<Variable>> {
        match self.ty.formatter() {
            Some(formatter) => formatter.indexed_children(self, range),
            None => Ok(Default::default()),
        }
    }

    pub fn named_children(&self, range: Range<usize>) -> Result<Vec<Variable>> {
        match self.ty.formatter() {
            Some(formatter) => formatter.named_children(self, range),
            None => Ok(Default::default()),
        }
    }
}

fn value_to_le_bytes(value: gimli::Value, len: usize) -> Vec<u8> {
    let raw: u64 = match value {
        gimli::Value::Generic(v) => v,
        gimli::Value::I8(v) => v as i64 as u64,
        gimli::Value::U8(v) => v as u64,
        gimli::Value::I16(v) => v as i64 as u64,
        gimli::Value::U16(v) => v as u64,
        gimli::Value::I32(v) => v as i64 as u64,
        gimli::Value::U32(v) => v as u64,
        gimli::Value::I64(v) => v as u64,
        gimli::Value::U64(v) => v,
        gimli::Value::F32(_) | gimli::Value::F64(_) => 0,
    };
    let bytes = raw.to_le_bytes();
    let mut out = vec![0u8; len];
    let copy = len.min(8);
    out[..copy].copy_from_slice(&bytes[..copy]);
    out
}

impl MemoryOffset {
    /// Returns the pieces for a variable located at this address.
    pub fn pieces(&self) -> Vec<gimli::Piece<R>> {
        vec![gimli::Piece {
            size_in_bits: None,
            bit_offset: None,
            location: gimli::Location::Address {
                address: (*self).into(),
            },
        }]
    }
}
