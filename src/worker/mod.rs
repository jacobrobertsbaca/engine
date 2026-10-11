use console_error_panic_hook;
use instant::Instant;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use wasmer_wasix::virtual_fs::{AsyncWriteExt, FileSystem, create_dir_all, mem_fs};
use web_sys::{DedicatedWorkerGlobalScope, MessageEvent};

use crate::types::{CppArtifacts, FsNode, Lang, WorkerOut, WorkerStart};

mod debuggee;
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
            let host_device = match msg.host_device.map(HostDeviceFile::new).transpose() {
                Ok(device) => device,
                Err(error) => {
                    return WorkerOut::Error {
                        message: format!("Invalid host device: {error}"),
                    }
                    .send();
                }
            };
            let llvm = fetch_bytes(CPP_WASM_URL).await.expect("toolchain");
            exec.write_bytes("/llvm.core.wasm", &llvm)
                .await
                .expect("toolchain");
            start_cpp(is_debug, build_start, sources, exec, fs, host_device, artifacts).await;
        }
        Lang::Rust => start_rust(is_debug, build_start, sources, exec, fs).await,
        Lang::Python => unreachable!(),
    }
}

async fn start_cpp(
    is_debug: bool,
    build_start: Instant,
    sources: Vec<String>,
    exec: Execution,
    fs: mem_fs::FileSystem,
    host_device: Option<HostDeviceFile>,
    artifacts: CppArtifacts,
) {
    // These three files add exception support without replacing the fetched sysroot.
    let exceptions =
        extract_tar_gz(include_bytes!("../../assets/cpp-exceptions/runtime.tar.gz").to_vec())
            .await
            .expect("embedded C++ exception runtime");
    let exceptions: Arc<dyn FileSystem + Send + Sync> = Arc::new(exceptions);
    exec.fs.union(&exceptions);

    let mut obj_paths: Vec<String> = Vec::with_capacity(sources.len());
    let mut union_fs: Option<Box<dyn FileSystem>> = Some(Box::new(fs));

    for source in &sources {
        let obj_path = format!("{source}.o");

        let mut clang_args = vec![
            "-cc1",
            "-triple",
            "wasm32-wasip1",
            "-Werror",
            "-emit-obj",
            "-disable-free",
            "-isysroot",
            "/",
            "-internal-isystem",
            "/include/c++/v1",
            "-internal-isystem",
            "/include",
            "-internal-isystem",
            "/include/wasm32-wasip1",
            "-ferror-limit",
            "4",
            "-fcolor-diagnostics",
            "-x",
            "c++",
            "-std=c++23",
            "-fexceptions",
            "-fcxx-exceptions",
            "-exception-model=wasm",
            "-target-feature",
            "+exception-handling",
            "-mllvm",
            "-wasm-enable-eh",
            "-mllvm",
            "-wasm-use-legacy-eh=true",
            "-o",
            obj_path.as_str(),
        ];

        if is_debug {
            clang_args.push("-O0");
            clang_args.push("-debug-info-kind=standalone");
            clang_args.push("-dwarf-version=5");
        }

        if let Some(pch) = &artifacts.precompiled_header {
            clang_args.extend(["-include-pch", pch, "-fvalidate-ast-input-files-content"]);
        }
        clang_args.push(source);

        let mut step = exec
            .step("clang")
            .binary("/llvm.core.wasm")
            .args(&clang_args);
        if let Some(fs) = union_fs.take() {
            step = step.sysroot(CPP_STDLIB_URL).fs(fs);
        }
        let exit = step.run().await.expect("Compilation succeeded");

        if !exit.is_success() {
            return stop(exit.raw(), build_start, None).send();
        }
        obj_paths.push(obj_path);
    }

    let mut link_args: Vec<&str> = vec![
        "--export-dynamic",
        "-z",
        "stack-size=1048576",
        "-L/lib/wasm32-wasip1",
        "/lib/wasm32-wasip1/crt1.o",
    ];
    for obj in &obj_paths {
        link_args.push(obj);
    }
    for archive in artifacts.archives.iter().flatten() {
        link_args.push(archive);
    }
    link_args.extend_from_slice(&[
        "/lib/wasm32-wasip1/cxa_exception.cpp.o",
        "/lib/wasm32-wasip1/cxa_personality.cpp.o",
        "-lc++",
        "-lc++abi",
        "-lunwind",
        "/lib/wasm32-unknown-wasip1/libclang_rt.builtins.a",
        "-lc",
        "-o",
        "/main.wasm",
    ]);

    let exit = exec
        .step("wasm-ld")
        .binary("/llvm.core.wasm")
        .args(&link_args)
        .run()
        .await
        .expect("Linking succeeded");

    if !exit.is_success() {
        return stop(exit.raw(), build_start, None).send();
    }

    let run_start = Instant::now();
    let mut run = exec.step("main").binary("/main.wasm").debug(is_debug);
    if let Some(device) = host_device {
        run = run.device_file(HOST_DEVICE_PATH, Box::new(device));
    }
    let exit = run.run().await.expect("Running succeeded");

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
