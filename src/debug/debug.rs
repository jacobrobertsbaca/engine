use std::cell::OnceCell;
use std::rc::Rc;

use crate::debug::dwarf::{Die, Location, R};
use crate::debug::formatters::{self, VariableFormatter};
use crate::debug::{Type, TypeGraph, Variable, get_location, get_variables as debug_get_variables};
use crate::types::{
    BreakpointMode, CodeOffset, DebugFunction, DebugInfo, MemoryDescriptor, WasmLocation,
};
use crate::util::{Ref, WeakRef};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsCast;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StackFrame {
    pub id: u32,
    pub name: String,
    pub line: u32,
    pub column: u32,
    pub source: Option<String>,
}

/// Represents a category of formatters.
///
/// When determining which formatter to use for a variable,
/// the debugger will exhaust all possibilities *within* a category
/// before moving onto the next one. This is a (somewhat naive)
/// simplification of LLDB's behaviour, which can be extended upon.
///
/// This allows us to, for example, try all user formatters before trying
/// all standard library formatters before all core formatters, etc.
pub struct FormatterCategory {
    #[allow(dead_code)]
    name: &'static str,
    formatters: Vec<Rc<dyn VariableFormatter>>,
}

impl FormatterCategory {
    fn formatter_for(&self, ty: &Type) -> Option<Rc<dyn VariableFormatter>> {
        self.formatters
            .iter()
            .rev()
            .find(|f| f.matches(ty))
            .cloned()
    }
}

// ╭──────────────────────────────────────────────────────────────────────────╮
// │ Main-thread Debugger                                                     │
// ╰──────────────────────────────────────────────────────────────────────────╯

/// Main-thread debugger that operates on shared memory from an attached worker.
/// Constructed from `DebugInfo` received via the worker's `debug` message.
pub struct Debugger {
    me: WeakRef<Self>,
    info: DebugInfo,
    types: Ref<TypeGraph>,
    state: js_sys::Int32Array,
    formatters: Vec<FormatterCategory>,
}

impl Debugger {
    pub fn new(info: DebugInfo) -> Ref<Debugger> {
        Ref::new_cyclic(|me| {
            let state = info.get_bp_state();
            let types = TypeGraph::new(me, &info.dwarf);
            Self {
                me: me.clone(),
                info,
                state,
                types,
                formatters: vec![
                    FormatterCategory {
                        name: "default",
                        formatters: vec![
                            Rc::new(formatters::default::ScalarFormatter),
                            Rc::new(formatters::default::ReferentialFormatter),
                            Rc::new(formatters::default::StructureFormatter),
                            Rc::new(formatters::default::ArrayFormatter),
                        ],
                    },
                    FormatterCategory {
                        name: "cpp",
                        formatters: vec![
                            Rc::new(formatters::cpp::StdStringFormatter),
                            Rc::new(formatters::cpp::StdMapFormatter),
                            Rc::new(formatters::cpp::StdVectorFormatter),
                        ],
                    },
                    FormatterCategory {
                        name: "user",
                        formatters: vec![],
                    },
                ],
            }
        })
    }

    /// Computes the formatter for a given type, if any
    pub fn formatter_for(&self, ty: &Type) -> Option<Rc<dyn VariableFormatter>> {
        for category in self.formatters.iter().rev() {
            let mut current = ty.clone();
            loop {
                if let Some(formatter) = category.formatter_for(&current) {
                    return Some(formatter);
                }
                let next = current.skip_one_modifier();
                if next == current {
                    break;
                }
                current = next;
            }
        }

        None
    }

    fn stack_view(&self) -> (js_sys::DataView, u32, u32) {
        let sp = self.state.get_index(0) as u32;
        let buffer = self.info.stack.memory.buffer();
        let buffer = buffer.unchecked_ref::<js_sys::ArrayBuffer>();
        let stack_top = buffer.byte_length();
        let view = js_sys::DataView::new(buffer, 0, stack_top as usize);
        (view, sp, stack_top)
    }

    /// Walks the debug stack and returns the current call stack.
    pub fn backtrace(&self) -> anyhow::Result<Vec<StackFrame>> {
        let (view, sp, stack_top) = self.stack_view();
        let mut frames = Vec::new();
        let mut pos = sp;

        while pos < stack_top {
            let pc = view.get_uint32_endian(pos as usize, true).into();
            let func = match self.info.fn_at(pc) {
                Some(f) => f,
                None => break,
            };
            let die = func.die_ref.deref(&self.info.dwarf)?;

            let loc = self
                .info
                .locations
                .iter()
                .filter(|l| l.address <= pc)
                .max_by_key(|l| l.address);

            frames.push(StackFrame {
                id: frames.len() as u32,
                name: die.qualified_name().unwrap_or(String::new()),
                line: loc.as_ref().map_or(0, |l| l.line as u32),
                column: loc.as_ref().map_or(0, |l| l.column as u32),
                source: loc.as_ref().and_then(|l| {
                    self.info
                        .dwarf
                        .file_at(l.file_index)
                        .map(|p| p.to_string_lossy().into_owned())
                }),
            });
            pos += func.size as u32;
        }

        Ok(frames)
    }

    /// Sets one breakpoint for one `(file, line)` pair.
    /// Returns the location it gets set at, if any.
    pub fn set_breakpoint(&self, file: &str, line: i64) -> Option<&Location> {
        let flags = self.info.get_bp_flags();

        let target = std::path::Path::new(file);
        let target = if target.is_absolute() {
            target.to_path_buf()
        } else {
            std::path::Path::new("/").join(target)
        };

        // If a breakpoint cannot be set on the requested line,
        // it will be set on the next executable statement.
        //
        // TODO: how to handle inline functions where a line
        // may map to multiple instructions?
        //
        // TODO: how to speed this query up for large location lists?
        let (index, loc) = self
            .info
            .locations
            .iter()
            .enumerate()
            .filter(|l| {
                let file = self.info.dwarf.file_at(l.1.file_index);
                let loc_line = l.1.line as i64;
                file.map_or(false, |f| *f == target) && loc_line >= line
            })
            .min_by(|l1, l2| {
                l1.1.line
                    .cmp(&l2.1.line)
                    .then_with(|| l1.1.address.cmp(&l2.1.address))
            })?;

        flags.set_index(index as u32, 1);
        Some(loc)
    }

    /// Walks the debug stack to find the offset of the Nth stack frame
    fn frame_offset_at(&self, frame_id: u32) -> Option<u32> {
        let (view, sp, stack_top) = self.stack_view();
        let mut pos = sp;

        for _ in 0..frame_id {
            if pos >= stack_top {
                return None;
            }
            let pc = view.get_uint32_endian(pos as usize, true).into();
            let func = self.info.fn_at(pc)?;
            pos += func.size as u32;
        }

        if pos >= stack_top {
            return None;
        }

        Some(pos)
    }

    /// Returns the variables visible in `frame_id`, split into `(arguments, locals)`.
    ///
    /// Arguments are DIE children tagged `DW_TAG_formal_parameter`; locals are
    /// `DW_TAG_variable` (the modern tag) or `DW_TAG_local_variable`. Variables
    /// whose location expression cannot be resolved (e.g. optimized out /
    /// require unsupported opcodes) are dropped.
    pub fn get_variables(&self, frame_id: u32) -> Result<VariablesResult> {
        let frame_offset = self
            .frame_offset_at(frame_id)
            .context(format!("Could not find frame offset for frame {frame_id}"))?;

        let context = EvaluationContext::new(self.me.clone(), frame_offset);
        let DerivedEvaluationContext { pc, die, .. } = context.derived()?;

        let var_dies = debug_get_variables(&die, pc);
        let mut variables = VariablesResult::default();

        // TODO: How to better report errors for failed variable evaluations in this loop?
        // What should happen if we fail to get a variables name/type/pieces/etc.?
        for var_die in &var_dies {
            let name = var_die.name().unwrap_or_default();

            let Some(type_id) = var_die.type_ref() else {
                continue;
            };

            let ty = self.types.get(type_id);

            let Some(expr) = get_location(var_die, pc) else {
                continue;
            };

            let Ok(pieces) = context.evaluate(expr) else {
                continue;
            };

            let variable = Variable::new(context.clone(), name, pieces, ty);
            match var_die.tag() {
                gimli::DW_TAG_formal_parameter => variables.arguments.push(variable),
                gimli::DW_TAG_variable | gimli::DW_TAG_local_variable => {
                    variables.locals.push(variable)
                }
                _ => {}
            }
        }

        Ok(variables)
    }

    /// Borrow of the underlying [`DebugInfo`], used by handlers that need to
    /// peek at memory or DWARF without owning the debugger.
    pub fn info(&self) -> &DebugInfo {
        &self.info
    }

    /// A reference to the program's linear memory
    pub fn memory(&self) -> &MemoryDescriptor {
        &self.info.memory
    }

    /// A reference to the debugger's debug stack
    pub fn stack(&self) -> &MemoryDescriptor {
        &self.info.stack
    }

    /// Returns the size of an address in bytes on the current platform.
    pub fn address_size(&self) -> usize {
        // TODO: Can this ever change?
        4
    }

    fn notify_resume(&self) {
        js_sys::Atomics::store(&self.state, 0, 0).unwrap();
        js_sys::Atomics::notify(&self.state, 0).unwrap();
    }

    /// Resumes the worker by signaling through the SAB (normal execution: breakpoints only).
    pub fn continue_(&self) {
        js_sys::Atomics::store(&self.state, 1, BreakpointMode::Normal.into()).unwrap();
        self.notify_resume();
    }

    /// Resume with step-into semantics (stop at the next instrumented breakpoint site).
    pub fn step_into(&self) {
        js_sys::Atomics::store(&self.state, 1, BreakpointMode::StepInto.into()).unwrap();
        self.notify_resume();
    }

    /// Resume with step-over semantics (same or outer frame versus `last_sp`).
    pub fn step_over(&self) {
        js_sys::Atomics::store(&self.state, 1, BreakpointMode::StepOver.into()).unwrap();
        self.notify_resume();
    }

    /// Resume with step-out semantics (outer frame only versus `last_sp`).
    pub fn step_out(&self) {
        js_sys::Atomics::store(&self.state, 1, BreakpointMode::StepOut.into()).unwrap();
        self.notify_resume();
    }
}

// ╭──────────────────────────────────────────────────────────────────────────╮
// │ Variable Evaluation                                                      │
// ╰──────────────────────────────────────────────────────────────────────────╯

/// Contains all state needed to evaluate a [gimli::Expression].
#[derive(Clone)]
pub struct EvaluationContext {
    /// A reference to the debugger that triggered the evaluation
    debugger: WeakRef<Debugger>,
    /// The debug stack offset to the beginning of the stack frame
    /// where evaluations occur
    offset: u32,
    /// The cached frame base to use across evaluations.
    fb: OnceCell<u64>,
}

pub struct DerivedEvaluationContext<'a> {
    pub pc: CodeOffset,
    pub func: &'a DebugFunction,
    pub die: Die<'a>,
}

#[derive(Default)]
pub struct VariablesResult {
    pub arguments: Vec<Variable>,
    pub locals: Vec<Variable>,
    pub globals: Vec<Variable>,
}

impl EvaluationContext {
    pub fn new(debugger: WeakRef<Debugger>, offset: u32) -> Self {
        Self {
            debugger,
            offset,
            fb: Default::default(),
        }
    }

    /// Get a reference to the underlying debugger, if it exists.
    pub fn debugger(&self) -> Option<&Debugger> {
        self.debugger.as_deref()
    }

    /// Get the derived info (PC, DIE, etc.) from this evaluation context
    pub fn derived(&self) -> Result<DerivedEvaluationContext<'_>> {
        let debugger = self
            .debugger
            .as_deref()
            .context("Cannot evaluate expression without debugger")?;

        let pc: CodeOffset = debugger.stack().read_u32(self.offset.into()).into();

        let func = debugger
            .info()
            .fn_at(pc)
            .context(format!("Could not find function for pc {pc}"))?;

        let die = func.die_ref.deref(&debugger.info().dwarf)?;

        Ok(DerivedEvaluationContext { pc, func, die })
    }

    /// Evaluates an expression and returns the result as an unsigned value, if possible.
    pub fn evaluate_unsigned(&self, expr: gimli::Expression<R>) -> Result<u64> {
        // TODO: How does this method interact with multi-piece values?
        let pieces = self.evaluate(expr)?;
        let piece = pieces.first().context("Evaluation resulted in no pieces")?;
        match &piece.location {
            gimli::Location::Value { value } => gimli_value_to_u64(*value),
            gimli::Location::Address { address } => Ok(*address),
            _ => anyhow::bail!(
                "Could not determine value from piece location {:?}",
                piece.location
            ),
        }
    }

    /// Evaluates an expression and returns the result as a signed value, if possible.
    pub fn evaluate_signed(&self, expr: gimli::Expression<R>) -> Result<i64> {
        let pieces = self.evaluate(expr)?;
        let piece = pieces.first().context("Evaluation resulted in no pieces")?;
        match &piece.location {
            gimli::Location::Value { value } => gimli_value_to_i64(*value),
            gimli::Location::Address { address } => (*address)
                .try_into()
                .map_err(|_| anyhow::anyhow!("DWARF value out of range for i64")),
            _ => anyhow::bail!(
                "Could not determine value from piece location {:?}",
                piece.location
            ),
        }
    }

    /// Evaluates an expression and returns the resulting pieces.
    pub fn evaluate(&self, expr: gimli::Expression<R>) -> Result<Vec<gimli::Piece<R>>> {
        let DerivedEvaluationContext { pc, die, func } = self.derived()?;
        let encoding = die.ctx().unit.encoding();
        let mut eval = expr.evaluation(encoding);

        loop {
            let result = eval.evaluate()?;
            match result {
                gimli::EvaluationResult::Complete => return Ok(eval.result()),
                gimli::EvaluationResult::RequiresWasmLocal { index } => {
                    eval.resume_with_wasm_value(
                        self.wasm_value(func, &WasmLocation::Local(index as usize))?,
                    )?;
                }
                gimli::EvaluationResult::RequiresWasmStack { index } => {
                    eval.resume_with_wasm_value(
                        self.wasm_value(func, &WasmLocation::Operand(index as usize))?,
                    )?;
                }
                gimli::EvaluationResult::RequiresWasmGlobal { index } => {
                    eval.resume_with_wasm_value(
                        self.wasm_value(func, &WasmLocation::Operand(index as usize))?,
                    )?;
                }
                gimli::EvaluationResult::RequiresFrameBase => {
                    if let Some(fb) = self.fb.get() {
                        eval.resume_with_frame_base(*fb)?;
                    } else {
                        let expr = die
                            .expression(gimli::DW_AT_frame_base, pc)
                            .context("Required DW_AT_frame_base missing on function")?;
                        let fb = self.evaluate_unsigned(expr)?;
                        eval.resume_with_frame_base(*self.fb.get_or_init(|| fb))?;
                    }
                }
                _ => anyhow::bail!("Could not execute evaluation step {result:?}"),
            }
        }
    }

    fn wasm_value(&self, func: &DebugFunction, loc: &WasmLocation) -> Result<gimli::Value> {
        let entry = func
            .layout
            .iter()
            .find(|e| &e.location == loc)
            .context(format!(
                "Location {loc:?} could not be found in function {func:?}"
            ))?;

        let debugger = self
            .debugger
            .as_deref()
            .context("Cannot determine wasm value without debugger")?;

        let offset = (self.offset as usize) + entry.offset;
        Ok(gimli::Value::Generic(
            debugger.stack().read_u64(offset.into()),
        ))
    }
}

fn gimli_value_to_u64(value: gimli::Value) -> Result<u64> {
    match value {
        gimli::Value::Generic(v) if (v as i64) >= 0 => Ok(v),
        gimli::Value::I8(v) if v >= 0 => Ok(v as u64),
        gimli::Value::I16(v) if v >= 0 => Ok(v as u64),
        gimli::Value::I32(v) if v >= 0 => Ok(v as u64),
        gimli::Value::I64(v) if v >= 0 => Ok(v as u64),
        gimli::Value::U8(v) => Ok(v.into()),
        gimli::Value::U16(v) => Ok(v.into()),
        gimli::Value::U32(v) => Ok(v.into()),
        gimli::Value::U64(v) => Ok(v),
        _ => anyhow::bail!("DWARF value {value:?} cannot be converted to u64"),
    }
}

fn gimli_value_to_i64(value: gimli::Value) -> Result<i64> {
    match value {
        gimli::Value::Generic(v) => Ok(v as i64),
        gimli::Value::I8(v) => Ok(v.into()),
        gimli::Value::I16(v) => Ok(v.into()),
        gimli::Value::I32(v) => Ok(v.into()),
        gimli::Value::I64(v) => Ok(v),
        gimli::Value::U8(v) => Ok(v.into()),
        gimli::Value::U16(v) => Ok(v.into()),
        gimli::Value::U32(v) => Ok(v.into()),
        gimli::Value::U64(v) => v
            .try_into()
            .map_err(|_| anyhow::anyhow!("DWARF value {value:?} out of range for i64")),
        _ => anyhow::bail!("DWARF value {value:?} cannot be converted to i64"),
    }
}
