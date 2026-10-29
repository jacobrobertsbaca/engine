use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::collections::HashMap;
use tsify::Tsify;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasmer::{MemoryType, Pages};
use web_sys::DedicatedWorkerGlobalScope;

use derive_more::Display;

use crate::debug::dwarf::{DieReference, Dwarf, Location};

macro_rules! offset_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Default,
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Display,
            Tsify,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        #[display("0x{_0:x}")]
        pub struct $name(u64);

        impl From<u32> for $name {
            fn from(v: u32) -> Self {
                Self(v as u64)
            }
        }

        impl From<u64> for $name {
            fn from(v: u64) -> Self {
                Self(v)
            }
        }

        impl From<usize> for $name {
            fn from(v: usize) -> Self {
                Self(v as u64)
            }
        }

        impl From<$name> for u32 {
            fn from(v: $name) -> Self {
                v.0 as u32
            }
        }

        impl From<$name> for u64 {
            fn from(v: $name) -> Self {
                v.0
            }
        }

        impl From<$name> for usize {
            fn from(v: $name) -> Self {
                v.0 as usize
            }
        }
    };
}

// ╭──────────────────────────────────────────────────────────────────────────╮
// │ Types                                                                    │
// ╰──────────────────────────────────────────────────────────────────────────╯

offset_type!(
    /// Represents a byte offset into program memory.
    MemoryOffset
);

impl MemoryOffset {
    pub fn is_null(&self) -> bool {
        *self == Self::default()
    }
}

offset_type!(
    /// Represents a byte offset into the code section.
    CodeOffset
);

#[derive(Debug, Tsify, Deserialize)]
#[serde(untagged)]
pub enum FsNode {
    File(String),
    Dir(HashMap<String, FsNode>),
}

#[derive(Debug, Clone, Copy, Tsify, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    C,
    Python,
    Rust,
}

#[derive(Debug, Tsify, Deserialize)]
pub struct WorkerStart {
    pub fs: HashMap<String, FsNode>,

    #[serde(default)]
    #[tsify(type = "Record<string, Uint8Array>")]
    pub binary_files: HashMap<String, serde_bytes::ByteBuf>,
    #[serde(default)]
    pub cpp_artifacts: Option<CppArtifacts>,
    #[serde(default)]
    pub cpp_cache: Option<CppCacheRequest>,
    #[serde(default)]
    pub build_only: bool,

    #[serde(default)]
    pub host_device: Option<HostDeviceStart>,

    #[serde(with = "serde_wasm_bindgen::preserve")]
    pub stdin_buffer: js_sys::SharedArrayBuffer,
    pub is_debug: bool,
    pub lang: Lang,
}

/// Optional inputs produced by the same C++ toolchain as this engine.
#[derive(Default, Debug, Tsify, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CppArtifacts {
    #[serde(default)]
    pub sources: Option<Vec<String>>,
    #[serde(default)]
    pub archives: Option<Vec<String>>,
    #[serde(default)]
    pub precompiled_header: Option<String>,
}

/// Internal, owned snapshots of this engine's memory-only translation-unit cache.
#[derive(Debug, Tsify, Deserialize)]
pub struct CppCacheRequest {
    pub generation: u32,
    pub entries: Vec<CppCacheEntry>,
}

#[derive(Debug, Tsify, Deserialize)]
pub struct CppCacheEntry {
    pub source: String,
    pub key: String,
    pub object_hash: String,
    #[tsify(type = "Uint8Array")]
    pub object: serde_bytes::ByteBuf,
}

#[derive(Default, Debug, Tsify, Serialize)]
pub struct CppCacheStats {
    pub hits: u32,
    pub misses: u32,
    pub bypasses: Vec<CppCacheBypass>,
    pub hit_sources: Vec<String>,
    pub invalid_sources: Vec<String>,
    pub hash_ms: f64,
    pub lookup_ms: f64,
    pub compile_ms: f64,
    pub link_ms: f64,
}

#[derive(Debug, Tsify, Serialize)]
pub struct CppCacheBypass {
    pub source: String,
    pub reason: String,
}

#[derive(Debug, Tsify, Deserialize)]
pub struct HostDeviceStart {
    #[serde(with = "serde_wasm_bindgen::preserve")]
    pub guest_to_host: js_sys::SharedArrayBuffer,
    #[serde(with = "serde_wasm_bindgen::preserve")]
    pub host_to_guest: js_sys::SharedArrayBuffer,
}

#[derive(Clone, Copy, Debug, Tsify, Serialize_repr)]
#[repr(u8)]
#[tsify(type = "1 | 2")]
pub enum StdoutMode {
    Out = 1,
    Err = 2,
}

#[derive(Tsify, Serialize)]
#[serde(tag = "type")]
pub enum WorkerOut<'a> {
    #[serde(rename = "ready")]
    Ready,
    #[serde(rename = "stdout")]
    Stdout {
        #[tsify(type = "Uint8Array")]
        #[serde(with = "serde_bytes")]
        data: &'a [u8],
        mode: StdoutMode,
    },
    #[serde(rename = "debug")]
    Debug { info: DebugInfo },

    #[serde(rename = "python_debug")]
    PythonDebug {
        #[serde(with = "serde_wasm_bindgen::preserve")]
        state: js_sys::SharedArrayBuffer,
    },

    /// Emit a build/engine artifact for the main thread to consume.
    #[serde(rename = "artifact")]
    Artifact {
        #[tsify(type = "Uint8Array")]
        #[serde(with = "serde_bytes")]
        data: &'a [u8],
        name: String,
    },

    #[serde(rename = "cpp_cache_object")]
    CppCacheObject {
        generation: u32,
        source: String,
        key: String,
        object_hash: String,
        #[tsify(type = "Uint8Array")]
        #[serde(with = "serde_bytes")]
        object: &'a [u8],
    },
    #[serde(rename = "cpp_cache_stats")]
    CppCacheStats {
        generation: u32,
        stats: CppCacheStats,
    },

    /// Indicate that execution has paused.
    ///
    /// DWARF backends send `reason`; Python sends a `frame` snapshot it derives the
    /// reason from. Each backend reads only its own field (see `DapDebugger::handle_paused`).
    #[serde(rename = "paused")]
    Paused {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<PauseReason>,
        #[serde(skip_serializing_if = "Option::is_none")]
        frame: Option<String>,
    },
    #[serde(rename = "stop")]
    Stop {
        exit_code: i32,
        /// Wall time in the worker from job start until the run step begins.
        build_ms: f64,
        /// Wall time in the worker for the final run step only.
        run_ms: f64,
    },

    /// A setup/runtime failure in the engine itself (e.g. the language runtime
    /// failed to load) — distinct from the user's program exiting non-zero.
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(rename = "host_wake")]
    HostWake,
}

impl<'a> WorkerOut<'a> {
    pub fn send(&self) {
        let scope = DedicatedWorkerGlobalScope::from(JsValue::from(js_sys::global()));
        scope
            .post_message(
                &serde_wasm_bindgen::to_value(self)
                    .expect("serialization worked")
                    .into(),
            )
            .expect("post_message succeeded");
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDescriptor {
    #[serde(with = "serde_wasm_bindgen::preserve")]
    pub memory: js_sys::WebAssembly::Memory,
    pub ty: MemoryType,
}

impl MemoryDescriptor {
    pub fn new(initial: u32, maximum: u32) -> Self {
        let desc = js_sys::Object::new();
        js_sys::Reflect::set(&desc, &"initial".into(), &(initial as u32).into()).unwrap();
        js_sys::Reflect::set(&desc, &"maximum".into(), &(maximum as u32).into()).unwrap();
        js_sys::Reflect::set(&desc, &"shared".into(), &true.into()).unwrap();

        let memory = js_sys::WebAssembly::Memory::new(&desc).expect("create WebAssembly.Memory");

        let ty = MemoryType::new(Pages(initial), Some(Pages(maximum)), true);
        Self { memory, ty }
    }
}

/// Debug information parsed from DWARF
#[derive(Debug, Clone, Tsify, Serialize, Deserialize)]
pub struct DebugInfo {
    /// List of debuggable functions, sorted by low_pc
    pub functions: Vec<DebugFunction>,

    /// List of locations where breakpoints can be placed.
    /// Each location has had instrumentation code generated for it,
    /// and has an entry in [DebugInfo::breakpoints].
    ///
    /// This differs from [Dwarf::locations] in that the latter may
    /// return locations for which no code has been emitted, and thus
    /// no breakpoints may be placed.
    pub locations: Vec<Location>,

    /// This buffer encodes execution state and breakpoint metadata in a
    /// compact, fixed layout. All fields are little-endian.
    ///
    /// ```md
    /// ┌────────┬──────────────┬──────────────────────────────────────────────┐
    /// │ Offset │ Size         │ Field                                        │
    /// ├────────┼──────────────┼──────────────────────────────────────────────┤
    /// │ 0      │ u32          │ Stack Pointer                                │
    /// │        │              │   Current value of the stack pointer (SP).   │
    /// │        │              │   Will be 0 while the debuggee is running,   │
    /// │        │              │   and non-zero when paused on a breakpoint.  │
    /// │        │              │                                              │
    /// │ 4      │ u32          │ Mode                                         │
    /// │        │              │   Execution control mode:                    │
    /// │        │              │     0 → Pause on breakpoints                 │
    /// │        │              │     1 → Step into                            │
    /// │        │              │     2 → Step over                            │
    /// │        │              │     3 → Step out                             │
    /// │        │              │                                              │
    /// │ 8      │ u8[N]        │ Breakpoint flags                             │
    /// │        │              │   One byte per breakpoint location.          │
    /// │        │              │   Each entry counts how many times the       │
    /// │        │              │   corresponding breakpoint has been set.     │
    /// └────────┴──────────────┴──────────────────────────────────────────────┘
    /// ```
    ///
    /// Notes:
    /// - `N` is the number of breakpoint locations being tracked.
    /// - Breakpoint flags begin at offset [`BP_PREFIX_BYTES`] and are densely packed.
    ///
    #[serde(with = "serde_wasm_bindgen::preserve")]
    pub breakpoints: js_sys::SharedArrayBuffer,

    /// The main memory of the executing program
    pub memory: MemoryDescriptor,

    /// The debug stack of the executing program
    pub stack: MemoryDescriptor,

    /// Wrapper around DWARF debug information
    #[serde(with = "crate::debug::dwarf::serde")]
    pub dwarf: Dwarf,
}

/// Size in bytes of the `breakpoints` buffer prefix (SP + mode).
pub const BP_PREFIX_BYTES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum BreakpointMode {
    /// Stop on locations that have breakpoints set (default)
    Normal = 0,
    /// Stop on the next location unconditionally
    StepInto = 1,
    /// Stop on the next location in the same or an older frame
    StepOver = 2,
    /// Stop on the next location in an older frame
    StepOut = 3,
}

#[derive(Clone, Copy, Debug, Tsify, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum PauseReason {
    Breakpoint = 0,
    Step = 1,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub enum WasmLocation {
    /// The index of a local in the currently executing function.
    Local(usize),
    /// The index of a global.
    Global(usize),
    /// The index of an item on the operand stack. 0 is the bottom of the operand stack.
    Operand(usize),
}

#[derive(Debug, Clone, Tsify, Serialize, Deserialize)]
pub struct DebugFunction {
    /// The first address in this function
    pub low_pc: CodeOffset,
    /// The first address past the end of this function
    pub high_pc: CodeOffset,
    /// Reference to dwarf die for this function
    pub die_ref: DieReference,
    /// The total size in bytes of the stack frame, including it's 32-bit tag
    pub size: usize,
    /// The entries in this stack frame
    pub layout: Vec<DebugFrameEntry>,
}

#[derive(Debug, Clone, Tsify, Serialize, Deserialize)]
pub struct DebugFrameEntry {
    /// The byte offset of this entry in its containing stack frame
    pub offset: usize,
    /// The WebAssembly location (local, global, or stack) represented by this entry's value
    pub location: WasmLocation,
}

// ╭──────────────────────────────────────────────────────────────────────────╮
// │ Implementations                                                          │
// ╰──────────────────────────────────────────────────────────────────────────╯

impl MemoryDescriptor {
    /// Returns the actual number of bytes currently allocated in this memory
    pub fn byte_size(&self) -> usize {
        let buffer = self.memory.buffer();
        let buffer = buffer.unchecked_ref::<js_sys::ArrayBuffer>();
        buffer.byte_length() as usize
    }

    pub fn read(&self, addr: MemoryOffset, len: usize) -> Vec<u8> {
        let offset: usize = addr.into();
        let memory_buffer = self.memory.buffer();
        let buffer = memory_buffer.unchecked_ref::<js_sys::ArrayBuffer>();
        let mut out = vec![0u8; len];
        let n = (buffer.byte_length() as usize)
            .saturating_sub(offset)
            .min(len);
        if n > 0 {
            js_sys::Uint8Array::new_with_byte_offset_and_length(
                &buffer.into(),
                offset as u32,
                n as u32,
            )
            .copy_to(&mut out[..n]);
        }
        out
    }

    pub fn read_u64(&self, addr: MemoryOffset) -> u64 {
        let bytes = self.read(addr, 8);
        u64::from_le_bytes(bytes.try_into().unwrap()).into()
    }

    pub fn read_u32(&self, addr: MemoryOffset) -> u32 {
        let bytes = self.read(addr, 4);
        u32::from_le_bytes(bytes.try_into().unwrap()).into()
    }
}

impl DebugInfo {
    /// Whether we store the debug stack separately from the main program memory.
    pub fn is_multi_memory(&self) -> bool {
        !js_sys::Object::is(self.memory.memory.as_ref(), self.stack.memory.as_ref())
    }

    pub fn get_bp_state(&self) -> js_sys::Int32Array {
        js_sys::Int32Array::new_with_byte_offset_and_length(&self.breakpoints, 0, 2)
    }

    pub fn get_bp_flags(&self) -> js_sys::Uint8Array {
        js_sys::Uint8Array::new_with_byte_offset(&self.breakpoints, BP_PREFIX_BYTES as u32)
    }

    /// Finds the index of the function containing this address, if any
    pub fn fn_index_at(&self, pc: CodeOffset) -> Option<usize> {
        self.functions
            .binary_search_by(|f| {
                if pc < f.low_pc {
                    std::cmp::Ordering::Greater
                } else if pc >= f.high_pc {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()
    }

    /// Finds the function containing this address, if any
    pub fn fn_at(&self, pc: CodeOffset) -> Option<&DebugFunction> {
        self.fn_index_at(pc).map(|idx| &self.functions[idx])
    }
}

impl DebugFunction {
    pub fn contains(&self, pc: CodeOffset) -> bool {
        pc >= self.low_pc && pc < self.high_pc
    }

    /// Clears the layout of the stack frame and resets it to its minimum size.
    pub fn reset(&mut self) {
        self.size = 0;
        self.size += 4; // Space for function PC
        self.layout.clear();
    }

    /// Ensures an entry exists for `loc` and returns its offset.
    /// `bkpt` will be added to the lifetime of the found or created entry.
    /// Returns [None] if an entry could not be created (e.g. we cannot store wasm ref types).
    pub fn place(&mut self, location: WasmLocation) -> usize {
        if let Some(pos) = self.layout.iter().position(|e| e.location == location) {
            return self.layout[pos].offset;
        }

        let offset = self.size;
        let size = 8;
        self.size += size;
        self.layout.push(DebugFrameEntry { offset, location });
        offset
    }
}

impl TryFrom<i32> for BreakpointMode {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Normal),
            1 => Ok(Self::StepInto),
            2 => Ok(Self::StepOver),
            3 => Ok(Self::StepOut),
            _ => Err(()),
        }
    }
}

impl From<BreakpointMode> for i32 {
    fn from(value: BreakpointMode) -> Self {
        value as i32
    }
}
