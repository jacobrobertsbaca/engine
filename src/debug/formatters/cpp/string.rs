//! `std::string` (libc++ `basic_string<char>`) formatter.
//!
//! See the corresponding [LLDB formatter code](https://github.com/llvm/llvm-project/blob/main/lldb/source/Plugins/Language/CPlusPlus/LibCxx.cpp)
//! for reference.

// libc++ stores the string as a union of two reps sharing the same storage
// (12 bytes on wasm32). The high bit of the last byte is `__is_long_`, which
// selects which representation is live.
//
// Short (SSO, characters inline):
//
//        +-------------------------------+ class __short
//   0    | char __data_[__min_cap];      |  inline buffer (11 bytes for char)
//        | char __padding_[...];         |
//  11    | unsigned char __size_    : 7; |  low 7 bits = length
//        | unsigned char __is_long_ : 1; |  high bit  = 0
//        +-------------------------------+
//
// Long (heap-allocated):
//
//        +-------------------------------+ class __long
//   0    | pointer    __data_;           |  pointer to heap buffer
//   4    | size_type  __size_;           |  string length
//   8    | size_type  __cap_    : 31;    |  capacity (low 31 bits)
//        | size_type  __is_long_:  1;    |  high bit = 1
//        +-------------------------------+

use std::fmt::Write;
use std::ops::Range;

use anyhow::{Context, Result};

use crate::debug::formatters::{ChildCounts, VariableFormatter};
use crate::debug::{Type, Variable};

pub struct StdStringFormatter;

// High bit of `__short`'s size byte — set iff the string is in `__long` mode.
const IS_LONG_BIT: u8 = 0x80;

impl StdStringFormatter {
    fn read(value: &Variable) -> Result<Vec<u8>> {
        let dbg = value.debugger().context("no debugger")?;
        let rep = value.child_with_name("__rep_").context("__rep_")?;

        // __is_long_ / __size_ in __short are bitfields,
        let header = rep.read(12).context("read __rep_")?[11];

        if header & IS_LONG_BIT == 0 {
            let len = (header & !IS_LONG_BIT) as usize;
            let inline = rep
                .child_with_name("__s")
                .and_then(|s| s.child_with_name("__data_"))
                .context("__s.__data_")?;
            // Sanity: short size must fit inside the inline buffer; otherwise
            // the string is likely uninitialized and we'd render garbage.
            // LLDB also bails when `byte_size` is unavailable; we don't, since
            // clang-on-wasm always emits the array bound for char[__min_cap].
            if let Some(cap) = inline.ty().byte_size() {
                if len > cap as usize {
                    anyhow::bail!("std::string short size {len} exceeds inline buffer {cap}");
                }
            }
            let buf = inline.address().context("__s.__data_ address")?;
            anyhow::ensure!(u64::from(buf) + len as u64 <= dbg.memory().byte_size() as u64,
                "std::string storage is outside memory");
            return Ok(dbg.memory().read(buf, len));
        }

        let l = rep.child_with_name("__l").context("__l")?;
        let data = l
            .child_with_name("__data_")
            .and_then(|d| d.pointer_value())
            .context("__l.__data_")?;
        let len = l
            .child_with_name("__size_")
            .and_then(|f| f.unsigned_value())
            .context("__l.__size_")? as usize;
        anyhow::ensure!(len <= 65_536, "std::string display exceeds 65536 bytes");
        anyhow::ensure!(len == 0 || !data.is_null(), "std::string storage is null");
        anyhow::ensure!(u64::from(data) + len as u64 <= dbg.memory().byte_size() as u64,
            "std::string storage is outside memory");
        Ok(dbg.memory().read(data, len))
    }
}

fn quote(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() + 2);
    s.push('"');
    for &b in bytes {
        match b {
            b'"' => s.push_str("\\\""),
            b'\\' => s.push_str("\\\\"),
            b'\n' => s.push_str("\\n"),
            b'\r' => s.push_str("\\r"),
            b'\t' => s.push_str("\\t"),
            0x20..=0x7e => s.push(b as char),
            _ => write!(s, "\\x{b:02x}").unwrap(),
        }
    }
    s.push('"');
    s
}

// ╭──────────────────────────────────────────────────────────────────────────╮
// │ StdStringFormatter                                                       │
// ╰──────────────────────────────────────────────────────────────────────────╯

impl VariableFormatter for StdStringFormatter {
    fn matches(&self, ty: &Type) -> bool {
        ty.matches("^std::basic_string<char,.*>$")
    }

    fn display(&self, value: &Variable) -> Result<String> {
        Ok(quote(&Self::read(value)?))
    }

    fn num_children(&self, _value: &Variable) -> Result<ChildCounts> {
        Ok(ChildCounts::default())
    }

    fn indexed_children(&self, _value: &Variable, _range: Range<usize>) -> Result<Vec<Variable>> {
        Ok(Vec::new())
    }

    fn named_children(&self, _value: &Variable, _range: Range<usize>) -> Result<Vec<Variable>> {
        Ok(Vec::new())
    }
}
