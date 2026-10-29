use console_error_panic_hook;
use instant::Instant;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use wasmer_wasix::virtual_fs::{AsyncWriteExt, FileSystem, create_dir_all, mem_fs};
use web_sys::{DedicatedWorkerGlobalScope, MessageEvent};

use crate::types::{CppArtifacts, CppCacheBypass, CppCacheEntry, CppCacheRequest, CppCacheStats, FsNode, Lang, WorkerOut, WorkerStart};

mod debuggee;
mod cpp_cache;
pub(crate) mod execution;
pub(crate) mod host_device;
mod io;
mod runtime;

use execution::{Execution, extract_tar_gz, fetch_bytes};
use host_device::{HOST_DEVICE_PATH, HostDeviceFile};

// ╭──────────────────────────────────────────────────────────────────────────╮
// │ Helpers                                                                  │
// ╰──────────────────────────────────────────────────────────────────────────╯

pub(crate) async fn create_user_fs(node: FsNode) -> Result<mem_fs::FileSystem, std::io::Error> {
    let fs = mem_fs::FileSystem::default();
    create_user_fs_rec(&fs, &PathBuf::from("/"), &node).await?;
    Ok(fs)
}

async fn create_user_fs_rec(
    fs: &mem_fs::FileSystem,
    base_path: &PathBuf,
    node: &FsNode,
) -> Result<(), std::io::Error> {
    match node {
        FsNode::File(contents) => {
            let mut file = fs
                .new_open_options()
                .create(true)
                .write(true)
                .open(base_path)?;
            file.write_all(contents.as_bytes())
                .await
                .expect("Failed to write injected file");
            file.flush().await.expect("Flushed file")
        }
        FsNode::Dir(children) => {
            create_dir_all(fs, base_path)?;
            for (name, child_node) in children {
                let mut child_path = base_path.clone();
                child_path.push(name);
                Box::pin(create_user_fs_rec(fs, &child_path, child_node)).await?;
            }
        }
    }
    Ok(())
}

fn is_source_ext(ext: &str, lang: Lang) -> bool {
    match lang {
        Lang::C => matches!(
            ext.to_ascii_lowercase().as_str(),
            "c" | "cc" | "cp" | "cpp" | "cxx" | "c++"
        ),
        Lang::Rust => ext.eq_ignore_ascii_case("rs"),
        Lang::Python => false,
    }
}

fn collect_sources(node: &FsNode, base_path: &PathBuf, lang: Lang, sources: &mut Vec<String>) {
    match node {
        FsNode::File(_) => {
            if let Some(ext) = base_path.extension().and_then(|ext| ext.to_str()) {
                if is_source_ext(ext, lang) {
                    sources.push(base_path.to_string_lossy().to_string());
                }
            }
        }
        FsNode::Dir(children) => {
            collect_dir_sources(children, base_path, lang, sources);
        }
    }
}

fn collect_dir_sources(
    children: &std::collections::HashMap<String, FsNode>,
    base_path: &PathBuf,
    lang: Lang,
    sources: &mut Vec<String>,
) {
    for (name, child_node) in children {
        let mut child_path = base_path.clone();
        child_path.push(name);
        collect_sources(child_node, &child_path, lang, sources);
    }
}

fn rust_obj_path(source: &str) -> String {
    format!("{}.o", source.trim_end_matches(".rs"))
}

fn collect_rust_objs(fs: &mem_fs::FileSystem, source: &str) -> Vec<String> {
    let obj_path = rust_obj_path(source);
    let mut objs = vec![obj_path.clone()];

    let path = Path::new(source);
    let dir = path.parent().unwrap_or(Path::new("/"));
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

    let Ok(mut entries) = fs.read_dir(dir) else {
        return objs;
    };
    for entry in entries.by_ref().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(stem) && name.ends_with(".rcgu.o") {
            objs.push(format!("{}/{}", dir.to_string_lossy(), name));
        }
    }
    objs
}

fn find_rust_rlibs(fs: &mem_fs::FileSystem) -> Vec<String> {
    let dir = Path::new("/lib/rustlib/wasm32-wasip1/lib");
    let mut entries = fs.read_dir(dir).expect("Rust sysroot rlib directory");
    let names: Vec<String> = entries
        .by_ref()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    [
        "libpanic_abort-",
        "libstd-",
        "libwasi-",
        "libcfg_if-",
        "librustc_demangle-",
        "libstd_detect-",
        "libhashbrown-",
        "librustc_std_workspace_alloc-",
        "libminiz_oxide-",
        "libadler2-",
        "libunwind-",
        "liblibc-",
        "librustc_std_workspace_core-",
        "liballoc-",
        "libcore-",
        "libcompiler_builtins-",
    ]
    .iter()
    .map(|prefix| {
        names
            .iter()
            .find(|name| name.starts_with(prefix) && name.ends_with(".rlib"))
            .map(|name| format!("/lib/rustlib/wasm32-wasip1/lib/{name}"))
            .unwrap_or_else(|| panic!("missing rlib for prefix {prefix}"))
    })
    .collect()
}

// ╭──────────────────────────────────────────────────────────────────────────╮
// │ Worker                                                                   │
// ╰──────────────────────────────────────────────────────────────────────────╯

// Single source of truth for these is `prefetch_urls` in lib.rs (warmed by the host).
pub(crate) const CPP_WASM_URL: &str = "https://fabioibanez.github.io/website/llvm.core.wasm";
pub(crate) const CPP_STDLIB_URL: &str = "https://fabioibanez.github.io/website/llvm-resources.tar.gz";

/// Builds a `Stop` with build/run timing. `run_start` is `None` when execution never
/// began (e.g. a compile error), in which case the elapsed time is all build.
pub(crate) fn stop(exit_code: i32, build_start: Instant, run_start: Option<Instant>) -> WorkerOut<'static> {
    let ms = |d: instant::Duration| d.as_secs_f64() * 1000.0;
    let (build_ms, run_ms) = match run_start {
        Some(run) => (ms(run - build_start), ms(run.elapsed())),
        None => (ms(build_start.elapsed()), 0.0),
    };
    WorkerOut::Stop { exit_code, build_ms, run_ms }
}

async fn start(msg: WorkerStart) {
    if msg.build_only && msg.lang != Lang::C {
        return WorkerOut::Error { message: "Compile-only preparation requires C/C++".into() }.send();
    }
    if msg.lang != Lang::C && (msg.cpp_artifacts.is_some() || !msg.binary_files.is_empty()) {
        return WorkerOut::Error { message: "Binary build inputs require C/C++".into() }.send();
    }
    if msg.host_device.is_some() && !matches!(msg.lang, Lang::C | Lang::Python) {
        return WorkerOut::Error {
            message: "Host devices are supported only for C/C++ and Python execution".into(),
        }
        .send();
    }
    match msg.lang {
        Lang::Python => crate::python::worker::start(msg).await,
        Lang::C | Lang::Rust => start_native(msg).await,
    }
}

async fn start_native(msg: WorkerStart) {
    let build_start = Instant::now();
    let mut sources = Vec::new();
    collect_dir_sources(&msg.fs, &PathBuf::from("/"), msg.lang, &mut sources);
    let mut artifacts = msg.cpp_artifacts.unwrap_or_default();
    if let Some(selected) = artifacts.sources.take() {
        if selected.is_empty() || selected.iter().any(|path| !sources.contains(path)) {
            return WorkerOut::Error { message: "C++ sources must select existing source files".into() }.send();
        }
        sources = selected;
    }
    for path in artifacts.archives.iter().flatten().chain(artifacts.precompiled_header.iter()) {
        if !msg.binary_files.contains_key(path) {
            return WorkerOut::Error { message: format!("Missing binary build input: {path}") }.send();
        }
    }
    sources.sort();
    let hash_start = Instant::now();
    let cache_inputs = msg.cpp_cache.as_ref().map(|_| cpp_cache_inputs(&msg.fs, &msg.binary_files, &sources));
    let input_hash_ms = hash_start.elapsed().as_secs_f64() * 1000.0;

    let lang_name = match msg.lang {
        Lang::C => "C/C++",
        Lang::Rust => "Rust",
        Lang::Python => unreachable!(),
    };
    assert!(
        !sources.is_empty(),
        "No {lang_name} source files found in provided filesystem"
    );

    let fs = create_user_fs(FsNode::Dir(msg.fs))
        .await
        .expect("created user files filesystem");

    for (path, bytes) in msg.binary_files {
        if !path.starts_with('/') || path.split('/').skip(1).any(|part| matches!(part, "" | "." | "..")) {
            return WorkerOut::Error { message: format!("Invalid binary input path: {path}") }.send();
        }
        let written = async {
            create_dir_all(&fs, Path::new(&path).parent().unwrap())?;
            let mut file = fs.new_open_options().write(true).create_new(true).open(&path)?;
            file.write_all(&bytes).await
        }.await;
        if let Err(error) = written {
            return WorkerOut::Error { message: format!("Cannot mount binary input {path}: {error}") }.send();
        }
    }

    let exec = Execution::new(msg.stdin_buffer);
    let is_debug = msg.is_debug;

    match msg.lang {
        Lang::C => {
            let host_device = match msg.host_device.filter(|_| !msg.build_only).map(HostDeviceFile::new).transpose() {
                Ok(device) => device,
                Err(error) => {
                    return WorkerOut::Error {
                        message: format!("Invalid host device: {error}"),
                    }
                    .send();
                }
            };
            let llvm = fetch_bytes(CPP_WASM_URL).await.expect("toolchain");
            start_cpp(is_debug, msg.build_only, build_start, sources, exec, fs, host_device, artifacts, msg.cpp_cache, cache_inputs, &llvm, input_hash_ms).await;
        }
        Lang::Rust => start_rust(is_debug, build_start, sources, exec, fs).await,
        Lang::Python => unreachable!(),
    }
}

fn cpp_cache_inputs(
    fs: &std::collections::HashMap<String, FsNode>,
    binaries: &std::collections::HashMap<String, serde_bytes::ByteBuf>,
    sources: &[String],
) -> cpp_cache::Inputs {
    fn walk<'a>(node: &'a FsNode, path: String, files: &mut Vec<(String, &'a [u8], bool)>, paths: &mut Vec<String>) {
        paths.push(path.clone());
        match node {
            FsNode::File(text) => files.push((path, text.as_bytes(), true)),
            FsNode::Dir(children) => for (name, child) in children { walk(child, format!("{path}/{name}"), files, paths); },
        }
    }
    let mut files = Vec::new();
    let mut paths = Vec::new();
    for (name, node) in fs { walk(node, format!("/{name}"), &mut files, &mut paths); }
    for (path, bytes) in binaries {
        paths.push(path.clone());
        paths.extend(Path::new(path).ancestors().skip(1).filter(|path| *path != Path::new("/")).map(|path| path.to_string_lossy().into_owned()));
        files.push((path.clone(), bytes.as_ref(), false));
    }
    cpp_cache::Inputs::new(files, paths, sources)
}

struct CppCacheJob {
    generation: u32,
    entries: std::collections::HashMap<String, CppCacheEntry>,
    pending: Vec<CppCacheEntry>,
    pending_bytes: usize,
    stats: CppCacheStats,
}

impl CppCacheJob {
    fn bypass(&mut self, source: &str, reason: &str) {
        if self.stats.bypasses.len() < cpp_cache::MAX_REPORTED_SOURCES {
            self.stats.bypasses.push(CppCacheBypass { source: source.chars().take(cpp_cache::MAX_SOURCE_PATH).collect(), reason: reason.into() });
        }
    }

    fn send(self, publish: bool) {
        if publish {
            for entry in self.pending {
                WorkerOut::CppCacheObject {
                    generation: self.generation,
                    source: entry.source,
                    key: entry.key,
                    object_hash: entry.object_hash,
                    object: &entry.object,
                }.send();
            }
        }
        WorkerOut::CppCacheStats { generation: self.generation, stats: self.stats }.send();
    }
}

fn cpp_clang_args(source: &str, is_debug: bool, artifacts: &CppArtifacts, depfile: bool) -> Vec<String> {
    let mut args: Vec<String> = [
        "-cc1", "-triple", "wasm32-wasip1", "-Werror", "-emit-obj", "-disable-free", "-isysroot", "/",
        "-internal-isystem", "/include/c++/v1", "-internal-isystem", "/include", "-internal-isystem", "/include/wasm32-wasip1",
        "-ferror-limit", "4", "-fcolor-diagnostics", "-x", "c++", "-std=c++23", "-fexceptions", "-fcxx-exceptions", "-exception-model=wasm",
        "-target-feature", "+exception-handling", "-mllvm", "-wasm-enable-eh", "-mllvm", "-wasm-use-legacy-eh=true", "-o",
    ].into_iter().map(String::from).collect();
    args.push(format!("{source}.o"));
    if is_debug { args.extend(["-O0", "-debug-info-kind=standalone", "-dwarf-version=5"].into_iter().map(String::from)); }
    if let Some(pch) = &artifacts.precompiled_header { args.extend(["-include-pch".into(), pch.clone(), "-fvalidate-ast-input-files-content".into()]); }
    if depfile { args.extend(["-dependency-file".into(), format!("{source}.o.d"), "-MT".into(), format!("{source}.o"), "-sys-header-deps".into()]); }
    args.push(source.into());
    args
}

fn cpp_link_args(sources: &[String], artifacts: &CppArtifacts) -> Vec<String> {
    let mut args: Vec<String> = ["--export-dynamic", "-z", "stack-size=1048576", "-L/lib/wasm32-wasip1", "/lib/wasm32-wasip1/crt1.o"].into_iter().map(String::from).collect();
    args.extend(sources.iter().map(|source| format!("{source}.o")));
    args.extend(artifacts.archives.iter().flatten().cloned());
    args.extend([
        "/lib/wasm32-wasip1/cxa_exception.cpp.o", "/lib/wasm32-wasip1/cxa_personality.cpp.o", "-lc++", "-lc++abi", "-lunwind",
        "/lib/wasm32-unknown-wasip1/libclang_rt.builtins.a", "-lc", "-o", "/main.wasm",
    ].into_iter().map(String::from));
    args
}

async fn start_cpp(
    is_debug: bool,
    build_only: bool,
    build_start: Instant,
    sources: Vec<String>,
    exec: Execution,
    fs: mem_fs::FileSystem,
    host_device: Option<HostDeviceFile>,
    artifacts: CppArtifacts,
    request: Option<CppCacheRequest>,
    inputs: Option<cpp_cache::Inputs>,
    compiler_bytes: &[u8],
    input_hash_ms: f64,
) {
    // The identities below come from the exact bytes mounted for this job, never
    // from a caller-supplied version, URL, length or timestamp.
    let sysroot_bytes = fetch_bytes(CPP_STDLIB_URL).await.expect("C++ sysroot");
    let hash_start = Instant::now();
    let identities = request.as_ref().map(|_| (
        cpp_cache::hash(compiler_bytes),
        cpp_cache::hash(&sysroot_bytes),
        cpp_cache::hash(include_bytes!("../../assets/cpp-exceptions/runtime.tar.gz")),
    ));
    let mut cache = request.map(|request| CppCacheJob {
        generation: request.generation,
        entries: request.entries.into_iter().take(cpp_cache::MAX_REPORTED_SOURCES).map(|entry| (entry.source.clone(), entry)).collect(),
        pending: Vec::new(),
        pending_bytes: 0,
        stats: CppCacheStats { hash_ms: input_hash_ms + hash_start.elapsed().as_secs_f64() * 1000.0, ..Default::default() },
    });
    let sysroot: Arc<dyn FileSystem + Send + Sync> = Arc::new(extract_tar_gz(sysroot_bytes).await.expect("C++ sysroot filesystem"));
    exec.fs.union(&sysroot);
    // These files add exception support without replacing the fetched sysroot.
    let exceptions: Arc<dyn FileSystem + Send + Sync> = Arc::new(
        extract_tar_gz(include_bytes!("../../assets/cpp-exceptions/runtime.tar.gz").to_vec()).await.expect("embedded C++ exception runtime"),
    );
    exec.fs.union(&exceptions);
    let user_fs: Arc<dyn FileSystem + Send + Sync> = Arc::new(fs);
    exec.fs.union(&user_fs);
    // User resources cannot shadow the exact trusted compiler bytes identified
    // above or enter the immutable compiler-module cache under its marker.
    exec.write_bytes("/llvm.core.wasm", compiler_bytes).await.expect("toolchain");
    let link_args = cpp_link_args(&sources, &artifacts);

    for source in &sources {
        let obj_path = format!("{source}.o");
        let hash_start = Instant::now();
        let bypass = if is_debug { Some("debug-profile") }
            else if artifacts.precompiled_header.is_some() { Some("opaque-precompiled-header") }
            else if cache.is_some() && exec.fs.metadata(Path::new(&format!("{source}.o.d"))).is_ok() { Some("dependency-path-collision") }
            else { inputs.as_ref().and_then(|inputs| inputs.bypass(source)) };
        let clang_args = cpp_clang_args(source, is_debug, &artifacts, cache.is_some() && bypass.is_none());
        let key = if bypass.is_none() {
            inputs.as_ref().zip(identities.as_ref()).and_then(|(inputs, (compiler, sysroot, exceptions))| inputs.key(source, compiler, sysroot, exceptions, &clang_args, &link_args))
        } else { None };
        if let Some(cache) = &mut cache { cache.stats.hash_ms += hash_start.elapsed().as_secs_f64() * 1000.0; }
        let lookup_start = Instant::now();
        let mut cached = false;
        if let Some(cache) = &mut cache {
            if let Some(reason) = bypass { cache.bypass(source, reason); }
            else if let Some(key) = &key {
                if let Some(entry) = cache.entries.remove(source) {
                    if entry.key == *key {
                        if entry.object.len() <= 128 * 1024 * 1024 && cpp_cache::valid_object(&entry.object, &entry.object_hash) {
                            // write_bytes creates a fresh owned copy in this job's fresh fs.
                            cached = exec.write_bytes(&obj_path, &entry.object).await.is_ok();
                        }
                        if !cached && cache.stats.invalid_sources.len() < cpp_cache::MAX_REPORTED_SOURCES { cache.stats.invalid_sources.push(source.clone()); }
                    }
                }
                if cached {
                    cache.stats.hits += 1;
                    if cache.stats.hit_sources.len() < cpp_cache::MAX_REPORTED_SOURCES { cache.stats.hit_sources.push(source.clone()); }
                } else { cache.stats.misses += 1; }
            } else { cache.bypass(source, "unsupported-input"); }
            cache.stats.lookup_ms += lookup_start.elapsed().as_secs_f64() * 1000.0;
        }
        // A hit is counted only here, when the clang step is actually omitted.
        if cached { continue; }
        let output_before = exec.output_bytes();
        let compile_start = Instant::now();
        let exit = exec.step("clang").binary("/llvm.core.wasm").compiler().args(&clang_args).run().await.expect("Compilation succeeded");
        if let Some(cache) = &mut cache { cache.stats.compile_ms += compile_start.elapsed().as_secs_f64() * 1000.0; }
        // Dependency receipts are compiler-private. They must never appear in
        // the student filesystem or differ between cache misses and hits.
        let depfile = if key.is_some() {
            let dep_path = format!("{source}.o.d");
            let bytes = exec.read_bytes(&dep_path).await;
            let _ = exec.fs.remove_file(Path::new(&dep_path));
            if exec.fs.metadata(Path::new(&dep_path)).is_ok() {
                if let Some(cache) = cache { cache.send(false); }
                return WorkerOut::Error { message: "Cannot remove compiler dependency receipt".into() }.send();
            }
            Some(bytes)
        } else { None };
        if !exit.is_success() {
            if let Some(cache) = cache { cache.send(false); }
            return stop(exit.raw(), build_start, None).send();
        }
        if let Some(cache) = &mut cache {
            if let Some(key) = key {
                if exec.output_bytes() != output_before { cache.bypass(source, "compiler-diagnostics"); continue; }
                let depfile = depfile.expect("eligible compile has dependency receipt result");
                if !depfile.as_ref().ok().zip(inputs.as_ref()).is_some_and(|(depfile, inputs)| cpp_cache::dependencies_valid(depfile, source, &sources, &inputs.user_paths, &inputs.opaque_paths)) {
                    cache.bypass(source, "unsupported-dependencies"); continue;
                }
                if let Some(reason) = inputs.as_ref().and_then(|inputs| inputs.dependency_bypass(depfile.as_ref().unwrap())) {
                    cache.bypass(source, reason); continue;
                }
                let mut volatile_dependency = false;
                for dependency in cpp_cache::dependencies(depfile.as_ref().unwrap()).unwrap() {
                    if inputs.as_ref().is_some_and(|inputs| !inputs.user_paths.contains(&dependency)) {
                        match exec.read_bytes(&dependency).await {
                            Ok(bytes) if !cpp_cache::volatile_macro(&bytes) => (),
                            _ => { volatile_dependency = true; break; },
                        }
                    }
                }
                if volatile_dependency { cache.bypass(source, "volatile-system-header"); continue; }
                let Ok(object) = exec.read_bytes(&obj_path).await else { cache.bypass(source, "missing-object"); continue; };
                let object_hash = cpp_cache::hash(&object);
                if !cpp_cache::valid_object(&object, &object_hash) { cache.bypass(source, "invalid-object"); continue; }
                if cache.pending.len() >= 256 || object.len() > 128 * 1024 * 1024 - cache.pending_bytes {
                    cache.bypass(source, "staging-budget"); continue;
                }
                cache.pending_bytes += object.len();
                cache.pending.push(CppCacheEntry { source: source.clone(), key, object_hash, object: object.into() });
            }
        }
    }

    // Preparation stops before linking, student instantiation or host devices.
    if build_only {
        if let Some(cache) = cache { cache.send(true); }
        return stop(0, build_start, None).send();
    }
    let link_start = Instant::now();
    let exit = exec.step("wasm-ld").binary("/llvm.core.wasm").compiler().args(&link_args).run().await.expect("Linking succeeded");
    if let Some(cache) = &mut cache { cache.stats.link_ms = link_start.elapsed().as_secs_f64() * 1000.0; }
    if !exit.is_success() {
        if let Some(cache) = cache { cache.send(false); }
        return stop(exit.raw(), build_start, None).send();
    }

    let run_start = Instant::now();
    let mut run = exec.step("main").binary("/main.wasm").debug(is_debug);
    if let Some(device) = host_device { run = run.device_file(HOST_DEVICE_PATH, Box::new(device)); }
    let exit = run.run().await.expect("Running succeeded");
    if let Some(cache) = cache { cache.send(exit.is_success()); }
    stop(exit.raw(), build_start, Some(run_start)).send();
}

async fn start_rust(
    is_debug: bool,
    build_start: Instant,
    sources: Vec<String>,
    exec: Execution,
    fs: mem_fs::FileSystem,
) {
    let mut union_fs: Option<Box<dyn FileSystem>> = Some(Box::new(fs));

    for source in &sources {
        let obj_path = rust_obj_path(source);
        let mut rustc_args = vec![
            source.as_str(),
            "--sysroot",
            "/",
            "--target",
            "wasm32-wasip1",
            "-Cpanic=abort",
            "-Ccodegen-units=1",
            "-Zthreads=1",
            "--emit=obj",
            "-o",
            obj_path.as_str(),
        ];

        if is_debug {
            rustc_args.extend(["-g", "-Copt-level=0"]);
        }

        let mut step = exec
            .step("rustc")
            .binary("https://debugger-sh.github.io/engine/rustc.wasm")
            .args(&rustc_args);
        if let Some(fs) = union_fs.take() {
            step = step
                .sysroot("https://debugger-sh.github.io/engine/rust-sysroot.tar.gz")
                .fs(fs);
        }
        let exit = step.run().await.expect("Compilation succeeded");

        if !exit.is_success() {
            return stop(exit.raw(), build_start, None).send();
        }
    }

    let mut link_args: Vec<String> = vec![
        "--export=__main_void".into(),
        "-z".into(),
        "stack-size=1048576".into(),
        "--stack-first".into(),
        "--no-demangle".into(),
        "/lib/rustlib/wasm32-wasip1/lib/self-contained/crt1-command.o".into(),
    ];
    for source in &sources {
        link_args.extend(collect_rust_objs(&exec.fs, source));
    }
    link_args.extend(find_rust_rlibs(&exec.fs));
    link_args.extend([
        "-L/lib/rustlib/wasm32-wasip1/lib/self-contained".into(),
        "-lc".into(),
        "-o".into(),
        "/main.wasm".into(),
        "--gc-sections".into(),
        "-O0".into(),
    ]);

    let exit = exec
        .step("wasm-ld")
        .binary("https://fabioibanez.github.io/website/llvm.core.wasm")
        .args(link_args.iter().map(String::as_str))
        .run()
        .await
        .expect("Linking succeeded");

    if !exit.is_success() {
        return stop(exit.raw(), build_start, None).send();
    }

    let run_start = Instant::now();
    let exit = exec
        .step("main")
        .binary("/main.wasm")
        .debug(is_debug)
        .run()
        .await
        .expect("Running succeeded");

    stop(exit.raw(), build_start, Some(run_start)).send();
}

#[wasm_bindgen]
pub fn main() {
    console_error_panic_hook::set_once();
    let scope = DedicatedWorkerGlobalScope::from(JsValue::from(js_sys::global()));

    let onmessage = Closure::wrap(Box::new(move |msg: MessageEvent| {
        let message: WorkerStart = serde_wasm_bindgen::from_value(msg.data()).expect("");
        wasm_bindgen_futures::spawn_local(start(message));
    }) as Box<dyn Fn(MessageEvent)>);
    scope.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();

    WorkerOut::Ready.send();
}
