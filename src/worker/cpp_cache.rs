//! Conservative v1 identities. The host owns the bounded LRU; workers only accept
//! owned, digest-checked relocatable objects and never retain mutable compiler state.
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use wasmparser::{Encoding, KnownCustom, Linking, Parser, Payload, Validator, WasmFeatures};

pub const SCHEMA: &str = "debugger-sh/cpp-tu/v1";
const RELEASE: &str = include_str!("../../package.json");
pub const MAX_REPORTED_SOURCES: usize = 256;
pub const MAX_SOURCE_PATH: usize = 4096;

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Length prefixes keep identities unambiguous, including ordered arguments.
fn add(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

pub struct Inputs {
    source_hashes: BTreeMap<String, (String, Option<&'static str>)>,
    file_guards: BTreeMap<String, Option<&'static str>>,
    resources: String,
    paths: String,
    all_paths: BTreeSet<String>,
    pub user_paths: BTreeSet<String>,
    pub opaque_paths: BTreeSet<String>,
    bypass: Option<&'static str>,
}

impl Inputs {
    pub fn new<'a>(
        files: impl IntoIterator<Item = (String, &'a [u8], bool)>,
        mut paths: Vec<String>,
        sources: &[String],
    ) -> Self {
        let selected: BTreeSet<_> = sources.iter().collect();
        let files: BTreeMap<_, _> = files.into_iter().map(|(p, b, text)| (p, (b, text))).collect();
        let mut resources = Sha256::new();
        let mut source_hashes = BTreeMap::new();
        let mut file_guards = BTreeMap::new();
        let mut bypass = None;
        for (path, (bytes, text)) in &files {
            if !canonical_path(path) || path.len() > MAX_SOURCE_PATH {
                bypass = Some("unsupported-path");
            }
            let guard = if *text { text_guard(bytes) } else { None };
            file_guards.insert(path.clone(), guard);
            if selected.contains(path) {
                source_hashes.insert(path.clone(), (hash(bytes), guard));
            } else {
                add(&mut resources, path.as_bytes());
                add(&mut resources, bytes);
            }
        }
        paths.sort();
        paths.dedup();
        let all_paths = paths.iter().cloned().collect();
        let mut path_hash = Sha256::new();
        for path in paths {
            if !canonical_path(&path) || path.len() > MAX_SOURCE_PATH {
                bypass = Some("unsupported-path");
            }
            add(&mut path_hash, path.as_bytes());
        }
        let opaque_paths = files.iter().filter(|(_, (_, text))| !text).map(|(path, _)| path.clone()).collect();
        Self {
            source_hashes,
            file_guards,
            resources: format!("{:x}", resources.finalize()),
            paths: format!("{:x}", path_hash.finalize()),
            all_paths,
            user_paths: files.into_keys().collect(),
            opaque_paths,
            bypass,
        }
    }

    pub fn bypass(&self, source: &str) -> Option<&'static str> {
        if self.all_paths.contains(&format!("{source}.o.d")) { return Some("dependency-path-collision"); }
        self.bypass.or_else(|| self.source_hashes.get(source).and_then(|(_, reason)| *reason))
    }

    /// Resource bytes are always hashed, but unused headers cannot introduce
    /// compiler diagnostics or volatile expansions into this translation unit.
    pub fn dependency_bypass(&self, depfile: &[u8]) -> Option<&'static str> {
        for dependency in dependencies(depfile)? {
            let path = normalized_dependency(&dependency)?;
            if let Some(Some(reason)) = self.file_guards.get(&path) { return Some(reason); }
        }
        None
    }

    pub fn key(
        &self,
        source: &str,
        compiler: &str,
        sysroot: &str,
        exceptions: &str,
        compile_args: &[String],
        link_args: &[String],
    ) -> Option<String> {
        let (source_hash, _) = self.source_hashes.get(source)?;
        let mut key = Sha256::new();
        for value in [SCHEMA, RELEASE, env!("CARGO_PKG_VERSION"), "c++23/wasm-legacy-eh/run", compiler, sysroot, exceptions, &self.paths, &self.resources, source, source_hash] {
            add(&mut key, value.as_bytes());
        }
        add(&mut key, b"compile");
        for arg in compile_args { add(&mut key, arg.as_bytes()); }
        add(&mut key, b"link");
        for arg in link_args { add(&mut key, arg.as_bytes()); }
        Some(format!("{:x}", key.finalize()))
    }
}

fn canonical_path(path: &str) -> bool {
    path.starts_with('/') && path != "/" && !path.chars().any(|c| c.is_control() || matches!(c, '\\' | '$' | ':'))
        && path.split('/').skip(1).all(|part| !matches!(part, "" | "." | ".."))
}

/// These constructs can make diagnostics or generated code depend on more than
/// the frozen input bytes. Unknown include forms and pragmas deliberately bypass.
fn text_guard(bytes: &[u8]) -> Option<&'static str> {
    let Ok(text) = std::str::from_utf8(bytes) else { return Some("binary-text"); };
    if bytes.iter().any(|byte| *byte < 32 && !matches!(byte, b'\t' | b'\n' | b'\r') || *byte == 127) {
        return Some("binary-text");
    }
    let text = text.replace("\\\r\n", "").replace("\\\n", "");
    if volatile_macro(text.as_bytes()) {
        return Some("volatile-macro");
    }
    if ["_Pragma", "__pragma", "##", "%:", "??", "R\""].iter().any(|token| text.contains(token)) {
        return Some("dynamic-directive");
    }
    let Some(text) = strip_comments(&text) else { return Some("dynamic-directive"); };
    for line in text.lines() {
        let Some(directive) = line.trim_start().strip_prefix('#') else { continue; };
        let directive = directive.trim_start();
        if directive.starts_with(|c: char| c.is_ascii_digit()) { return Some("diagnostic-or-system-pragma"); }
        let end = directive.find(|c: char| !c.is_ascii_alphabetic() && c != '_').unwrap_or(directive.len());
        let (name, rest) = directive.split_at(end);
        let rest = rest.trim();
        match name {
            "pragma" if rest == "once" => (),
            "pragma" => return Some("diagnostic-or-system-pragma"),
            "include" => {
                let closing = match rest.as_bytes().first() { Some(b'"') => '"', Some(b'<') => '>', _ => return Some("dynamic-include") };
                let Some(end) = rest[1..].find(closing).map(|i| i + 1) else { return Some("dynamic-include"); };
                if !rest[end + 1..].trim().is_empty() || rest[1..end].contains('\\') {
                    return Some("dynamic-include");
                }
                let include = &rest[1..end];
                let ext = include.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
                if matches!(ext.as_str(), "c" | "cc" | "cp" | "cpp" | "cxx" | "c++") {
                    return Some("source-include");
                }
            }
            "include_next" | "import" | "embed" => return Some("dynamic-include"),
            _ => (),
        }
    }
    None
}

/// Also checked in actual transitive sysroot headers before admitting an object.
/// Their ordinary system-header pragmas do not change compiler warning policy.
pub fn volatile_macro(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else { return true; };
    let text = text.replace("\\\r\n", "").replace("\\\n", "");
    ["__DATE__", "__TIME__", "__TIMESTAMP__", "__COUNTER__"].iter().any(|token| text.contains(token))
}

fn strip_comments(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut quote = None;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            out.push(c);
            if c == '\\' { out.push(chars.next()?); }
            else if c == q { quote = None; }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
            out.push(c);
        } else if c == '/' && chars.peek() == Some(&'/') {
            chars.next();
            for c in chars.by_ref() { if c == '\n' { out.push(c); break; } }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            out.push(' ');
            let mut closed = false;
            while let Some(c) = chars.next() {
                if c == '\n' { out.push(c); }
                if c == '*' && chars.peek() == Some(&'/') { chars.next(); closed = true; break; }
            }
            if !closed { return None; }
        } else { out.push(c); }
    }
    Some(out)
}

/// Clang's make-style depfiles escape whitespace, '#', ':' and backslashes and
/// wrap lines. Reject malformed/multiple rules rather than guessing dependencies.
pub fn dependencies(data: &[u8]) -> Option<Vec<String>> {
    let data = std::str::from_utf8(data).ok()?.replace("\\\r\n", "").replace("\\\n", "");
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut target = false;
    let mut chars = data.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => token.push(chars.next()?),
            ':' if !target => { if token.is_empty() { return None; } token.clear(); target = true; },
            ':' | '$' => return None,
            '#' => { for c in chars.by_ref() { if c == '\n' { break; } } },
            c if c.is_whitespace() => { if !token.is_empty() { if !target { return None; } tokens.push(std::mem::take(&mut token)); } },
            c => token.push(c),
        }
    }
    if !token.is_empty() { tokens.push(token); }
    (target && !tokens.is_empty()).then_some(tokens)
}

fn normalized_dependency(path: &str) -> Option<String> {
    if !path.starts_with('/') { return None; }
    let mut parts = Vec::new();
    for part in path.split('/').skip(1) {
        match part { "" | "." => (), ".." => { parts.pop()?; }, part => parts.push(part) }
    }
    let path = format!("/{}", parts.join("/"));
    canonical_path(&path).then_some(path)
}

pub fn dependencies_valid(data: &[u8], source: &str, sources: &[String], user_paths: &BTreeSet<String>, opaque_paths: &BTreeSet<String>) -> bool {
    let Some(deps) = dependencies(data) else { return false; };
    let mut saw_source = false;
    for dependency in deps {
        let Some(path) = normalized_dependency(&dependency) else { return false; };
        if opaque_paths.contains(&path) { return false; }
        if path == source { saw_source = true; }
        else if sources.contains(&path) { return false; }
        else if !user_paths.contains(&path) && !path.starts_with("/include/") && !path.starts_with("/lib/") { return false; }
    }
    saw_source
}

pub fn valid_object(bytes: &[u8], expected_hash: &str) -> bool {
    if expected_hash.len() != 64 || hash(bytes) != expected_hash { return false; }
    if Validator::new_with_features(WasmFeatures::all()).validate_all(bytes).is_err() { return false; }
    let mut linking = false;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload {
            Ok(Payload::Version { encoding: Encoding::Module, .. }) => (),
            Ok(Payload::Version { .. }) | Ok(Payload::StartSection { .. }) | Err(_) => return false,
            Ok(Payload::CustomSection(section)) if section.name() == "linking" => {
                if linking { return false; }
                let KnownCustom::Linking(reader) = section.as_known() else { return false; };
                linking = true;
                for item in reader {
                    let valid = match item {
                        Ok(Linking::SegmentInfo(map)) => map.into_iter().all(|r| r.is_ok()),
                        Ok(Linking::InitFuncs(map)) => map.into_iter().all(|r| r.is_ok()),
                        Ok(Linking::SymbolTable(map)) => map.into_iter().all(|r| r.is_ok()),
                        Ok(Linking::ComdatInfo(map)) => map.into_iter().all(|r| r.is_ok_and(|c| c.symbols.into_iter().all(|r| r.is_ok()))),
                        _ => false,
                    };
                    if !valid { return false; }
                }
            }
            Ok(Payload::CustomSection(section)) if section.name().starts_with("reloc.") => {
                let KnownCustom::Reloc(reader) = section.as_known() else { return false; };
                if !reader.entries().into_iter().all(|r| r.is_ok()) { return false; }
            }
            _ => (),
        }
    }
    linking
}

#[cfg(test)]
mod tests {
    use super::*;
    fn inputs<'a>(main: &'a str, other: &'a str, header: &'a str, extra: bool) -> Inputs {
        let mut files: Vec<(String, &[u8], bool)> = vec![("/a.cpp".into(), main.as_bytes(), true), ("/b.cpp".into(), other.as_bytes(), true), ("/x.h".into(), header.as_bytes(), true), ("/lib.a".into(), &[0, 1, 2][..], false)];
        if extra { files.push(("/new.h".into(), b"" as &[u8], true)); }
        let paths = files.iter().map(|(p, _, _)| p.clone()).collect();
        Inputs::new(files, paths, &["/a.cpp".into(), "/b.cpp".into()])
    }
    fn key(inputs: &Inputs) -> String { inputs.key("/a.cpp", "clang", "sysroot", "eh", &["-Werror".into()], &["-lc++".into()]).unwrap() }
    #[test] fn same_length_source_and_header_edits_invalidate_but_other_tu_does_not() {
        let base = key(&inputs("return 1;", "return 2;", "#define X 1", false));
        assert_ne!(base, key(&inputs("return 3;", "return 2;", "#define X 1", false)));
        assert_ne!(base, key(&inputs("return 1;", "return 2;", "#define X 2", false)));
        assert_eq!(base, key(&inputs("return 1;", "return 3;", "#define X 1", false)));
        assert_ne!(base, key(&inputs("return 1;", "return 2;", "#define X 1", true)));
    }
    #[test] fn identity_flags_and_order_invalidate() {
        let inputs = inputs("int a;", "int b;", "", false);
        let base = key(&inputs);
        for (compiler, sysroot, eh, compile, link) in [("new", "sysroot", "eh", vec!["-Werror"], vec!["-lc++"]), ("clang", "new", "eh", vec!["-Werror"], vec!["-lc++"]), ("clang", "sysroot", "new", vec!["-Werror"], vec!["-lc++"]), ("clang", "sysroot", "eh", vec!["-O0", "-Werror"], vec!["-lc++"]), ("clang", "sysroot", "eh", vec!["-Werror"], vec!["-lc", "-lc++"])] {
            assert_ne!(base, inputs.key("/a.cpp", compiler, sysroot, eh, &compile.into_iter().map(String::from).collect::<Vec<_>>(), &link.into_iter().map(String::from).collect::<Vec<_>>()).unwrap());
        }
    }
    #[test] fn resource_bytes_paths_and_pch_archive_identity_invalidate() {
        fn resource(path: &str, bytes: &[u8]) -> Inputs {
            Inputs::new(vec![("/a.cpp".into(), b"int a;" as &[u8], true), (path.into(), bytes, false)], vec!["/a.cpp".into(), path.into()], &["/a.cpp".into()])
        }
        for path in ["/support.a", "/header.pch", "/resource.bin"] {
            assert_ne!(key(&resource(path, b"123")), key(&resource(path, b"124")));
            assert_ne!(key(&resource(path, b"123")), key(&resource(&format!("{path}.renamed"), b"123")));
        }
        assert_ne!(key(&resource("/support.a", b"123")), key(&Inputs::new(vec![("/a.cpp".into(), b"int a;" as &[u8], true)], vec!["/a.cpp".into()], &["/a.cpp".into()])));
    }
    #[test] fn unused_unsafe_headers_do_not_bypass_but_actual_dependencies_do() {
        let inputs = inputs("int a;", "int b;", "#pragma clang diagnostic ignored \"-Wunused\"", false);
        assert!(inputs.bypass("/a.cpp").is_none());
        assert!(inputs.dependency_bypass(b"/a.cpp.o: /a.cpp").is_none());
        assert_eq!(inputs.dependency_bypass(b"/a.cpp.o: /a.cpp /x.h"), Some("diagnostic-or-system-pragma"));
        assert!(volatile_macro(b"#define CLOCK __TI\\\nME__"));
    }
    #[test] fn dependency_output_collisions_bypass_for_files_and_directories() {
        let files = Inputs::new(vec![("/a.cpp".into(), b"int a;" as &[u8], true), ("/a.cpp.o.d".into(), b"student bytes" as &[u8], true)], vec!["/a.cpp".into(), "/a.cpp.o.d".into()], &["/a.cpp".into()]);
        let directory = Inputs::new(vec![("/a.cpp".into(), b"int a;" as &[u8], true)], vec!["/a.cpp".into(), "/a.cpp.o.d".into()], &["/a.cpp".into()]);
        assert_eq!(files.bypass("/a.cpp"), Some("dependency-path-collision"));
        assert_eq!(directory.bypass("/a.cpp"), Some("dependency-path-collision"));
    }
    #[test] fn dangerous_preprocessor_forms_bypass() {
        for text in ["__TIME__", "__DA\\\nTE__", "#include HEADER", "#include \"b.cpp\"", "/*x*/ #pragma GCC system_header", "# 1 \"x.h\" 3",  "#pragma clang diagnostic ignored \"-Wunused\"", "_Pragma(\"once\")", "#define X(a,b) a ## b", "abc\0def"] { assert!(text_guard(text.as_bytes()).is_some(), "{text}"); }
        assert!(text_guard(b"#line 1 \"main.cpp\"\nint main() { return 0; }").is_none());
        assert!(text_guard(b"#pragma once\n#include \"safe.h\"\nint f() { return 1; }").is_none());
    }
    #[test] fn depfile_escaping_and_source_dependency_guard() {
        assert_eq!(dependencies(b"/a.cpp.o: /a.cpp /space\\ name.h \\\n /hash\\#name.h\n").unwrap(), vec!["/a.cpp", "/space name.h", "/hash#name.h"]);
        let paths = BTreeSet::from(["/a.cpp".into(), "/b.cpp".into(), "/x.h".into()]);
        assert!(dependencies_valid(b"/a.cpp.o: /a.cpp /x.h\n", "/a.cpp", &["/a.cpp".into(), "/b.cpp".into()], &paths, &BTreeSet::new()));
        assert!(!dependencies_valid(b"/a.cpp.o: /a.cpp /b.cpp\n", "/a.cpp", &["/a.cpp".into(), "/b.cpp".into()], &paths, &BTreeSet::new()));
        assert!(!dependencies_valid(b"/a.cpp.o: /a.cpp /x.h\n", "/a.cpp", &["/a.cpp".into(), "/b.cpp".into()], &paths, &BTreeSet::from(["/x.h".into()])));
        for bad in [b"broken" as &[u8], b"x: /a.cpp\\", b"x: /a.cpp\ny: /b.cpp", b"x: $(dynamic)"] { assert!(dependencies(bad).is_none()); }
    }
    #[test] fn corrupt_and_non_object_modules_are_rejected() {
        let module = b"\0asm\x01\0\0\0";
        assert!(!valid_object(module, &hash(module)));
        assert!(!valid_object(b"not wasm", &hash(b"not wasm")));
        let object = b"\0asm\x01\0\0\0\0\x09\x07linking\x02";
        assert!(valid_object(object, &hash(object)));
        assert!(!valid_object(object, &hash(b"different bytes")));
        let corrupt = b"\0asm\x01\0\0\0\0\x09\x07linking\x03";
        assert!(!valid_object(corrupt, &hash(corrupt)));
    }
}
