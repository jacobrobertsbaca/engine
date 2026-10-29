use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use wasm_bindgen::JsValue;

use crate::dap::debugger::DapDebugger;
use crate::dap::types::requested_range;
use crate::debug::formatters::ChildCounts;
use crate::debug::{Debugger, Variable};
use crate::types::PauseReason;
use crate::util::Ref;

/// A handle handed out via a DAP `variablesReference`: a flat scope list, or one
/// expandable variable (with its child counts cached to avoid recomputing them).
#[derive(Clone)]
enum DwarfRef {
    List(Vec<Variable>),
    Variable { var: Variable, counts: ChildCounts },
}

/// Per-stop table of DWARF variable handles, rebuilt on each pause so stale
/// handles from a previous stop never resolve.
#[derive(Default)]
struct DwarfVars {
    next_ref: i64,
    entries: HashMap<i64, DwarfRef>,
}

impl DwarfVars {
    fn clear(&mut self) {
        self.next_ref = 0;
        self.entries.clear();
    }

    fn allocate(&mut self, reference: DwarfRef) -> i64 {
        self.next_ref += 1;
        self.entries.insert(self.next_ref, reference);
        self.next_ref
    }

    fn get(&self, reference: i64) -> Option<&DwarfRef> {
        self.entries.get(&reference)
    }
}

/// DAP backend for DWARF-instrumented languages (C, C++, Rust, etc.).
pub struct DwarfBackend {
    debugger: Ref<Debugger>,
    vars: DwarfVars,
}

impl DwarfBackend {
    pub fn new(debugger: Ref<Debugger>) -> Self {
        Self { debugger, vars: DwarfVars::default() }
    }

    fn scope_response(&mut self, name: &str, vars: Vec<Variable>) -> Value {
        let nvars = vars.len();
        let reference = self.vars.allocate(DwarfRef::List(vars));
        json!({
            "name": name,
            "variablesReference": reference,
            "expensive": false,
            "namedVariables": nvars
        })
    }

    fn variable_response(&mut self, var: &Variable) -> Result<Value> {
        let result = var.num_children();
        let counts_available = result.is_ok();
        let display = match &result {
            Err(e) => format!("<error: {e}>"),
            Ok(_) => var.display().unwrap_or_else(|e| format!("<error: {e}>")),
        };
        let counts = result.unwrap_or_default();
        let sub_ref = if counts.is_empty() {
            0
        } else {
            self.vars
                .allocate(DwarfRef::Variable { var: var.clone(), counts: counts.clone() })
        };

        let mut value = json!({
            "name": var.name(),
            "value": display,
            "type": var.ty().name(),
            "variablesReference": sub_ref,
        });

        if let Some(map) = value.as_object_mut() {
            if counts_available {
                map.insert("indexedVariables".into(), counts.indexed.into());
                map.insert("namedVariables".into(), counts.named.into());
            }
            if let Some(addr) = var.address() {
                map.insert("memoryReference".into(), addr.to_string().into());
            }
        }

        Ok(value)
    }
}

impl DapDebugger for DwarfBackend {
    fn configuration_done(&mut self) -> Result<Value> {
        self.debugger.continue_();
        Ok(Value::Null)
    }

    fn set_breakpoints(&mut self, source: &str, lines: &[i64]) -> Result<Value> {
        let bps: Vec<_> = lines
            .iter()
            .map(|line| {
                let location = self.debugger.set_breakpoint(source, *line);
                if let Some(location) = location {
                    json!({ "verified": true, "line": location.line })
                } else {
                    json!({ "verified": false })
                }
            })
            .collect();
        Ok(json!({ "breakpoints": bps }))
    }

    fn stack_trace(&self) -> Result<Value> {
        let frames = self
            .debugger
            .backtrace()
            .context("Failed to get backtrace")?;
        let total = frames.len();
        let stack_frames: Vec<_> = frames
            .iter()
            .map(|f| {
                let source = f.source.as_ref().map(|p| json!({ "path": p }));
                json!({
                    "id": f.id,
                    "name": f.name,
                    "line": f.line,
                    "column": f.column,
                    "source": source,
                })
            })
            .collect();
        Ok(json!({
            "stackFrames": stack_frames,
            "totalFrames": total,
        }))
    }

    fn scopes(&mut self, args: &Value) -> Result<Value> {
        let frame_id = args.get("frameId").and_then(|v| v.as_i64()).unwrap_or(0) as u32;
        let variables = self.debugger.get_variables(frame_id)?;

        let mut scopes: Vec<Value> = Vec::new();
        if !variables.arguments.is_empty() {
            scopes.push(self.scope_response("Arguments", variables.arguments));
        }
        if !variables.locals.is_empty() {
            scopes.push(self.scope_response("Locals", variables.locals));
        }
        if !variables.globals.is_empty() {
            scopes.push(self.scope_response("Globals", variables.globals));
        }

        Ok(json!({ "scopes": scopes }))
    }

    fn variables(&mut self, args: &Value) -> Result<Value> {
        let reference = args
            .get("variablesReference")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        let reference = self
            .vars
            .get(reference)
            .context("Unknown variablesReference")?
            .clone();

        let entries = requested_children(args, &reference)?;
        let variables: Result<Vec<_>> = entries
            .iter()
            .map(|var| self.variable_response(var))
            .collect();
        Ok(json!({
            "variables": variables?
        }))
    }

    fn continue_(&mut self) -> Result<Value> {
        self.debugger.continue_();
        Ok(json!({ "allThreadsContinued": true }))
    }

    fn next(&mut self) -> Result<Value> {
        self.debugger.step_over();
        Ok(json!({}))
    }

    fn step_in(&mut self) -> Result<Value> {
        self.debugger.step_into();
        Ok(json!({}))
    }

    fn step_out(&mut self) -> Result<Value> {
        self.debugger.step_out();
        Ok(json!({}))
    }

    fn handle_paused(&mut self, msg: &JsValue) -> Result<Value> {
        self.vars.clear();
        let reason = js_sys::Reflect::get(msg, &"reason".into())
            .map_err(|_| anyhow!("paused message missing reason"))?;
        let reason: PauseReason = serde_wasm_bindgen::from_value(reason)
            .map_err(|e| anyhow!("PauseReason deserialization: {e}"))?;
        Ok(json!({
            "reason": match reason {
                PauseReason::Breakpoint => "breakpoint",
                PauseReason::Step => "step",
            },
            "threadId": 1,
            "allThreadsStopped": true,
        }))
    }
}

fn requested_children(args: &Value, reference: &DwarfRef) -> Result<Vec<Variable>> {
    let filter = args.get("filter").and_then(|v| v.as_str());

    match reference {
        DwarfRef::List(entries) => {
            let range = requested_range(args, entries.len());
            match filter {
                Some("indexed") => Ok(Vec::default()),
                Some("named") | None => Ok(entries[range].to_vec()),
                Some(filter) => Err(anyhow!("Invalid variable filter: '{:?}'", filter)),
            }
        }

        DwarfRef::Variable { var, counts } => match filter {
            Some("indexed") => {
                let range = requested_range(args, counts.indexed);
                var.indexed_children(range)
            }
            Some("named") => {
                let range = requested_range(args, counts.named);
                var.named_children(range)
            }
            None => requested_mixed_children(args, var, counts),
            Some(filter) => Err(anyhow!("Invalid variable filter: '{:?}'", filter)),
        },
    }
}

fn requested_mixed_children(
    args: &Value,
    var: &Variable,
    counts: &ChildCounts,
) -> Result<Vec<Variable>> {
    let range = requested_range(args, counts.total());
    let named_start = range.start.min(counts.named);
    let named_end = range.end.min(counts.named);
    let indexed_start = range.start.saturating_sub(counts.named).min(counts.indexed);
    let indexed_end = range.end.saturating_sub(counts.named).min(counts.indexed);

    let mut children = var.named_children(named_start..named_end)?;
    children.extend(var.indexed_children(indexed_start..indexed_end)?);
    Ok(children)
}
