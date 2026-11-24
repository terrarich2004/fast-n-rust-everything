//! The bridge shared by every language that calls its port through a C entry point (Ruby, Go,
//! PHP, Java, C#, C, C++): the crate (direct C ABI + JSON fallback), the replay check, temporary
//! edits that always get undone, and the blocks fnr puts inside a function's body.
//!
//! Go, PHP, Java, C# and C/C++ record differently from Python and Node: a few lines at the top of
//! the hot function hand each call to a recorder, which also answers shrunk "derived" inputs at exit.
//! Verifying then needs no interpreter at all: fnr loads the port itself and replays
//! the file, in a child process so a crashing port can't take it down.

use crate::backend::{self, Lang};
use crate::hotspot::Hotspot;
use crate::state::{Ctx, Failure, Micro, Port, Verify};
use crate::util::{self, combined, output, tail};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// Scalar types supported across the direct binary C ABI (`fa_direct`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CAbiScalar {
    I8,
    I16,
    I32,
    I64,
    ISize,
    U8,
    U16,
    U32,
    U64,
    USize,
    F32,
    F64,
    Bool,
}

impl CAbiScalar {
    pub fn from_rust_str(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "i8" => Self::I8,
            "i16" => Self::I16,
            "i32" => Self::I32,
            "i64" => Self::I64,
            "isize" => Self::ISize,
            "u8" => Self::U8,
            "u16" => Self::U16,
            "u32" => Self::U32,
            "u64" => Self::U64,
            "usize" => Self::USize,
            "f32" => Self::F32,
            "f64" => Self::F64,
            "bool" => Self::Bool,
            _ => return None,
        })
    }

    pub fn rust_ty(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::ISize => "isize",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::USize => "usize",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Bool => "bool",
        }
    }

    pub fn abi_rust_ty(self) -> &'static str {
        match self {
            Self::Bool => "u8",
            _ => self.rust_ty(),
        }
    }

    pub fn c_ty(self) -> &'static str {
        match self {
            Self::I8 => "int8_t",
            Self::I16 => "int16_t",
            Self::I32 => "int32_t",
            Self::I64 => "int64_t",
            Self::ISize => "ptrdiff_t",
            Self::U8 | Self::Bool => "uint8_t",
            Self::U16 => "uint16_t",
            Self::U32 => "uint32_t",
            Self::U64 => "uint64_t",
            Self::USize => "size_t",
            Self::F32 => "float",
            Self::F64 => "double",
        }
    }

    pub fn go_c_ty(self) -> &'static str {
        match self {
            Self::I8 => "C.int8_t",
            Self::I16 => "C.int16_t",
            Self::I32 => "C.int32_t",
            Self::I64 => "C.int64_t",
            Self::ISize => "C.ptrdiff_t",
            Self::U8 | Self::Bool => "C.uint8_t",
            Self::U16 => "C.uint16_t",
            Self::U32 => "C.uint32_t",
            Self::U64 => "C.uint64_t",
            Self::USize => "C.size_t",
            Self::F32 => "C.float",
            Self::F64 => "C.double",
        }
    }

    pub fn csharp_ty(self) -> &'static str {
        match self {
            Self::I8 => "sbyte",
            Self::I16 => "short",
            Self::I32 => "int",
            Self::I64 => "long",
            Self::ISize => "nint",
            Self::U8 | Self::Bool => "byte",
            Self::U16 => "ushort",
            Self::U32 => "uint",
            Self::U64 => "ulong",
            Self::USize => "nuint",
            Self::F32 => "float",
            Self::F64 => "double",
        }
    }

    pub fn java_ffm_layout(self) -> Option<&'static str> {
        Some(match self {
            Self::I8 | Self::U8 | Self::Bool => "java.lang.foreign.ValueLayout.JAVA_BYTE",
            Self::I16 => "java.lang.foreign.ValueLayout.JAVA_SHORT",
            Self::I32 => "java.lang.foreign.ValueLayout.JAVA_INT",
            Self::I64 => "java.lang.foreign.ValueLayout.JAVA_LONG",
            Self::F32 => "java.lang.foreign.ValueLayout.JAVA_FLOAT",
            Self::F64 => "java.lang.foreign.ValueLayout.JAVA_DOUBLE",
            _ => return None,
        })
    }

    pub fn java_abi_ty(self) -> Option<&'static str> {
        Some(match self {
            Self::I8 | Self::U8 | Self::Bool => "byte",
            Self::I16 => "short",
            Self::I32 => "int",
            Self::I64 => "long",
            Self::F32 => "float",
            Self::F64 => "double",
            _ => return None,
        })
    }

    pub fn zero_rust(self) -> &'static str {
        match self {
            Self::F32 | Self::F64 => "0.0",
            _ => "0",
        }
    }

    pub fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectParam {
    Scalar { name: String, ty: CAbiScalar },
    Str { name: String },
    Slice { name: String, elem: CAbiScalar },
}

/// Direct C ABI signature for `fa_direct`, available when all parameters and return type
/// map cleanly to C scalars, UTF-8 byte slices, or flat scalar slices without default gaps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectSig {
    pub params: Vec<DirectParam>,
    pub ret: CAbiScalar,
}

impl DirectSig {
    pub fn from_hotspot(hs: &Hotspot, f: &Flavor) -> Option<Self> {
        if hs.params.iter().any(|p| p.default.is_some()) {
            return None;
        }
        let ret_rust = hs.returns.as_deref().and_then(f.rust_type)?;
        let ret = CAbiScalar::from_rust_str(&ret_rust)?;
        let mut params = Vec::with_capacity(hs.params.len());
        for p in &hs.params {
            let rty = p.annotation.as_deref().and_then(f.rust_type)?;
            let pname = snake(&p.name);
            if let Some(ty) = CAbiScalar::from_rust_str(&rty) {
                params.push(DirectParam::Scalar { name: pname, ty });
            } else if rty == "String" {
                params.push(DirectParam::Str { name: pname });
            } else {
                let inner = rty.strip_prefix("Vec<").and_then(|s| s.strip_suffix('>'))?;
                let elem = CAbiScalar::from_rust_str(inner)?;
                params.push(DirectParam::Slice { name: pname, elem });
            }
        }
        Some(Self { params, ret })
    }

    pub fn is_all_scalar(&self) -> bool {
        self.params
            .iter()
            .all(|p| matches!(p, DirectParam::Scalar { .. }))
    }

    pub fn has_floats(&self) -> bool {
        self.ret.is_float()
            || self
                .params
                .iter()
                .any(|p| matches!(p, DirectParam::Scalar { ty, .. } if ty.is_float()))
    }

    pub fn scalar_list(&self) -> Option<Vec<CAbiScalar>> {
        self.params
            .iter()
            .map(|p| match p {
                DirectParam::Scalar { ty, .. } => Some(*ty),
                _ => None,
            })
            .collect()
    }
}

pub fn no_type(_: &str) -> Option<String> {
    None
}

fn split_top_generic(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth_a, mut depth_p) = (0usize, 0usize);
    let mut start = 0;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'<' => depth_a += 1,
            b'>' => depth_a = depth_a.saturating_sub(1),
            b'(' => depth_p += 1,
            b')' => depth_p = depth_p.saturating_sub(1),
            b',' if depth_a == 0 && depth_p == 0 => {
                parts.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = s[start..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

/// Compact binary wire compatibility descriptor (`fa_bin`).
pub struct BinSig;

impl BinSig {
    pub fn supports_rust_type(rty: &str) -> bool {
        let t = rty.trim();
        if CAbiScalar::from_rust_str(t).is_some() || matches!(t, "String" | "serde_json::Value") {
            return true;
        }
        if let Some(inner) = t.strip_prefix("Option<").and_then(|s| s.strip_suffix('>')) {
            return Self::supports_rust_type(inner);
        }
        if let Some(inner) = t.strip_prefix("Vec<").and_then(|s| s.strip_suffix('>')) {
            return Self::supports_rust_type(inner);
        }
        for prefix in [
            "HashMap<",
            "std::collections::HashMap<",
            "BTreeMap<",
            "std::collections::BTreeMap<",
        ] {
            if let Some(inner) = t.strip_prefix(prefix).and_then(|s| s.strip_suffix('>')) {
                let parts = split_top_generic(inner);
                return parts.len() == 2
                    && parts[0] == "String"
                    && Self::supports_rust_type(parts[1]);
            }
        }
        if let Some(inner) = t.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
            let parts = split_top_generic(inner);
            return (2..=4).contains(&parts.len())
                && parts.iter().all(|p| Self::supports_rust_type(p));
        }
        if let Some(inner) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
            && let Some((elem, len)) = inner.rsplit_once(';')
            && len.trim().parse::<usize>().is_ok()
        {
            return Self::supports_rust_type(elem);
        }
        false
    }

    pub fn from_hotspot(hs: &Hotspot, f: &Flavor) -> bool {
        if hs.params.iter().any(|p| p.kwonly) {
            return false;
        }
        let ret_ok = hs
            .returns
            .as_deref()
            .and_then(f.rust_type)
            .is_none_or(|t| Self::supports_rust_type(&t));
        let params_ok = hs.params.iter().all(|p| {
            p.annotation
                .as_deref()
                .and_then(f.rust_type)
                .is_none_or(|t| Self::supports_rust_type(&t))
        });
        ret_ok && params_ok
    }
}

/// What changes from one C-bridge language to the next.
pub struct Flavor {
    pub lang: Lang,
    /// Code fence language for TASK.md.
    pub fence: &'static str,
    /// Rust type for a type written in the original language.
    pub rust_type: fn(&str) -> Option<String>,
    /// A default value in the original language, as JSON.
    pub default_json: fn(&str) -> Option<String>,
    /// Language-specific rules for TASK.md.
    pub rules: &'static [&'static str],
}

// ------------------------------------------------------------------ the crate

pub fn snake(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    match s.as_str() {
        "type" | "match" | "fn" | "ref" | "move" | "loop" | "mod" | "use" | "impl" | "self"
        | "box" | "gen" | "in" | "where" | "trait" | "struct" | "enum" | "crate" | "super"
        | "unsafe" | "async" | "await" | "dyn" => {
            format!("r#{s}")
        }
        _ => s,
    }
}

pub fn glue_rs(hs: &Hotspot, port: &Port, default_json: fn(&str) -> Option<String>) -> String {
    let args: Vec<String> = hs
        .params
        .iter()
        .enumerate()
        .map(|(i, p)| match p.default.as_deref().and_then(default_json) {
            Some(d) => format!("arg(&args, {i}, Some({d:?}))?"),
            None => format!("arg(&args, {i}, None)?"),
        })
        .collect();
    format!(
        "//! C entry point generated by fast-n-rust: JSON array of arguments in, {{\"ok\": result}} or\n\
         //! {{\"error\": message}} out. Don't edit this file; the port lives in src/port.rs.\n\n\
         pub mod port;\n\n\
         use serde_json::{{Value, json}};\n\n\
         fn arg<T: serde::de::DeserializeOwned>(args: &[Value], i: usize, default: Option<&str>) -> Result<T, String> {{\n    \
             match args.get(i) {{\n        \
                 Some(v) => T::deserialize(v).map_err(|e| format!(\"argument {{}}: {{e}}\", i + 1)),\n        \
                 None => match default {{\n            \
                     Some(d) => serde_json::from_str(d).map_err(|e| e.to_string()),\n            \
                     None => Err(format!(\"missing argument {{}}\", i + 1)),\n        \
                 }},\n    \
             }}\n\
         }}\n\n\
         fn call(input: &[u8]) -> Result<Value, String> {{\n    \
             let args: Vec<Value> = serde_json::from_slice(input).map_err(|e| e.to_string())?;\n    \
             let result = port::{func}({args})?;\n    \
             serde_json::to_value(result).map_err(|e| e.to_string())\n\
         }}\n\n\
         /// # Safety\n\
         /// input must point to len readable bytes and out_len to a writable usize.\n\
         // SAFETY: C ABI entry point; `input` and `out_len` invariants are documented above.\n\
         #[unsafe(no_mangle)]\n\
         pub unsafe extern \"C\" fn fa_call(input: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {{\n    \
             // SAFETY: caller guarantees `input` is valid for `len` bytes.\n\
             let input = unsafe {{ std::slice::from_raw_parts(input, len) }};\n    \
             let reply = match std::panic::catch_unwind(|| call(input)) {{\n        \
                 Ok(Ok(value)) => json!({{ \"ok\": value }}),\n        \
                 Ok(Err(message)) => json!({{ \"error\": message }}),\n        \
                 Err(panic) => {{\n            \
                     let text = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned());\n            \
                     json!({{ \"error\": format!(\"panic: {{}}\", text.unwrap_or_default()) }})\n        \
                 }}\n    \
             }};\n    \
             let bytes = serde_json::to_vec(&reply).unwrap_or_default().into_boxed_slice();\n    \
             // SAFETY: caller guarantees `out_len` points to a writable `usize`.\n\
             unsafe {{ *out_len = bytes.len() }};\n    \
             Box::into_raw(bytes) as *mut u8\n\
         }}\n\n\
         /// # Safety\n\
         /// ptr and len must come from fa_call.\n\
         // SAFETY: C ABI destructor for buffers allocated by `fa_call`.\n\
         #[unsafe(no_mangle)]\n\
         pub unsafe extern \"C\" fn fa_free(ptr: *mut u8, len: usize) {{\n    \
             if !ptr.is_null() {{\n        \
                 // SAFETY: `ptr` and `len` come directly from `Box::into_raw` in `fa_call`.\n\
                 drop(unsafe {{ Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) }});\n    \
             }}\n\
         }}\n",
        func = snake(&hs.name),
        args = args.join(", "),
    ) + &format!("\n// {}\n", port.id)
}

/// Generates `src/lib.rs` with JSON `fa_call`/`fa_free`, compact binary wire `fa_bin`,
/// and direct binary C ABI `fa_direct` when the signature maps cleanly to C ABI types.
pub fn glue_rs_with_flavor(hs: &Hotspot, port: &Port, f: &Flavor) -> String {
    let mut base = glue_rs(hs, port, f.default_json);
    let func = snake(&hs.name);
    if BinSig::from_hotspot(hs, f) {
        let mut bin_params = Vec::new();
        for (i, p) in hs.params.iter().enumerate() {
            match p.default.as_deref().and_then(f.default_json) {
                Some(d) => bin_params.push(format!("arg(&mut r, {i} < count, {i}, Some({d:?}))?")),
                None => bin_params.push(format!("arg(&mut r, {i} < count, {i}, None)?")),
            }
        }
        base.push_str(&format!(
            "\n#[allow(dead_code, clippy::too_many_arguments, clippy::not_unsafe_ptr_arg_deref)]\n\
             mod fa_bin {{\n\
             {codec}\n\
             \n\
             pub fn dispatch(input: &[u8]) -> Vec<u8> {{\n    \
                 let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<Vec<u8>, String> {{\n        \
                     let mut r = Reader::new(input);\n        \
                     let count = r.args_header()?;\n        \
                     let out = super::port::{func}({bin_args})?;\n        \
                     let mut w = Writer::new();\n        \
                     w.push(1);\n        \
                     out.bin_enc(&mut w);\n        \
                     Ok(w.into_vec())\n    \
                 }}));\n    \
                 match res {{\n        \
                     Ok(Ok(bytes)) => bytes,\n        \
                     Ok(Err(msg)) => encode_err(&msg),\n        \
                     Err(panic) => {{\n            \
                         let text = panic.downcast_ref::<&str>().map(|s| (*s).to_string()).or_else(|| panic.downcast_ref::<String>().cloned());\n            \
                         encode_err(&format!(\"panic: {{}}\", text.unwrap_or_default()))\n        \
                     }}\n    \
                 }}\n\
             }}\n\
             }}\n\
             \n\
             /// # Safety\n\
             /// `input` must point to `len` readable bytes of wire v1 payload and `out_len` to a writable `usize`.\n\
             // SAFETY: Binary wire FFI entry point; invariants match `fa_call`.\n\
             #[allow(clippy::not_unsafe_ptr_arg_deref)]\n\
             #[unsafe(no_mangle)]\n\
             pub unsafe extern \"C\" fn fa_bin(input: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {{\n    \
                 // SAFETY: caller guarantees `input` is valid for `len` bytes.\n    \
                 let slice = unsafe {{ std::slice::from_raw_parts(input, len) }};\n    \
                 let bytes = fa_bin::dispatch(slice).into_boxed_slice();\n    \
                 // SAFETY: caller guarantees `out_len` points to a writable `usize`.\n    \
                 unsafe {{ *out_len = bytes.len() }};\n    \
                 Box::into_raw(bytes) as *mut u8\n\
             }}\n",
            codec = FA_BIN_CODEC,
            bin_args = bin_params.join(", "),
        ));
    }
    let Some(sig) = DirectSig::from_hotspot(hs, f) else {
        return base;
    };
    let mut abi_params = Vec::new();
    let mut conversions = String::new();
    let mut call_args = Vec::new();
    for p in &sig.params {
        match p {
            DirectParam::Scalar { name, ty } => {
                abi_params.push(format!("{name}: {}", ty.abi_rust_ty()));
                if *ty == CAbiScalar::Bool {
                    call_args.push(format!("{name} != 0"));
                } else {
                    call_args.push(name.clone());
                }
            }
            DirectParam::Str { name } => {
                abi_params.push(format!("{name}_ptr: *const u8"));
                abi_params.push(format!("{name}_len: usize"));
                conversions.push_str(&format!(
                    "            let {name} = if {name}_len == 0 {{\n                String::new()\n            }} else if {name}_ptr.is_null() {{\n                return Err(\"null string pointer\".to_string());\n            }} else {{\n                // SAFETY: caller guarantees `{name}_ptr` points to `{name}_len` readable UTF-8 bytes.\n                let bytes = unsafe {{ std::slice::from_raw_parts({name}_ptr, {name}_len) }};\n                std::str::from_utf8(bytes).map_err(|e| e.to_string())?.to_string()\n            }};\n"
                ));
                call_args.push(name.clone());
            }
            DirectParam::Slice { name, elem } => {
                abi_params.push(format!("{name}_ptr: *const {}", elem.abi_rust_ty()));
                abi_params.push(format!("{name}_len: usize"));
                let to_vec = if *elem == CAbiScalar::Bool {
                    "slice.iter().map(|&v| v != 0).collect::<Vec<bool>>()"
                } else {
                    "slice.to_vec()"
                };
                conversions.push_str(&format!(
                    "            let {name} = if {name}_len == 0 {{\n                Vec::new()\n            }} else if {name}_ptr.is_null() {{\n                return Err(\"null slice pointer\".to_string());\n            }} else {{\n                // SAFETY: caller guarantees `{name}_ptr` points to `{name}_len` readable elements.\n                let slice = unsafe {{ std::slice::from_raw_parts({name}_ptr, {name}_len) }};\n                {to_vec}\n            }};\n"
                ));
                call_args.push(name.clone());
            }
        }
    }
    abi_params.push("fa_ok: *mut i32".to_string());
    let ret_abi = sig.ret.abi_rust_ty();
    let zero_ret = sig.ret.zero_rust();
    let ok_val = if sig.ret == CAbiScalar::Bool {
        "u8::from(val)"
    } else {
        "val"
    };
    let direct_block = format!(
        "\n/// # Safety\n\
         /// Any pointer/length pair must reference valid readable memory for the duration of the call,\n\
         /// and `fa_ok` must point to a writable `i32`.\n\
         // SAFETY: Direct C ABI entry point; pointer and `fa_ok` invariants are documented above.\n\
         #[allow(clippy::too_many_arguments, clippy::not_unsafe_ptr_arg_deref)]\n\
         #[unsafe(no_mangle)]\n\
         pub unsafe extern \"C\" fn fa_direct({params}) -> {ret_abi} {{\n\
             let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {{\n\
         {conversions}\
                 port::{func}({call_args})\n\
             }}));\n\
             match res {{\n\
                 Ok(Ok(val)) => {{\n\
                     if !fa_ok.is_null() {{\n\
                         // SAFETY: caller guarantees `fa_ok` is a valid writable pointer.\n\
                         unsafe {{ *fa_ok = 1 }};\n\
                     }}\n\
                     {ok_val}\n\
                 }}\n\
                 _ => {{\n\
                     if !fa_ok.is_null() {{\n\
                         // SAFETY: caller guarantees `fa_ok` is a valid writable pointer.\n\
                         unsafe {{ *fa_ok = 0 }};\n\
                     }}\n\
                     {zero_ret}\n\
                 }}\n\
             }}\n\
         }}\n",
        params = abi_params.join(", "),
        call_args = call_args.join(", "),
    );
    base.push_str(&direct_block);
    base
}

/// Generates `tests/recorded_parity.rs` so `cargo test` verifies all recorded cases directly.
pub fn recorded_parity_test(port: &Port, loose_empty: bool) -> String {
    let ext = &port.ext;
    format!(
        r#"//! Standard `cargo test` parity suite generated from `recordings.jsonl` by fast-n-rust.
//! Run directly with `cargo test` (or via `fnr verify` / `fnr ci`).

use {ext}::{{fa_call, fa_free}};
use serde_json::Value;
use std::path::PathBuf;

fn invoke(args: &Value) -> Result<Value, String> {{
    let input = serde_json::to_vec(args).map_err(|e| e.to_string())?;
    let mut n = 0usize;
    // SAFETY: `input` is a valid slice and `n` is a writable stack `usize`; `fa_free` frees the
    // buffer allocated by `fa_call` with the returned pointer and length.
    let bytes = unsafe {{
        let ptr = fa_call(input.as_ptr(), input.len(), &mut n);
        let slice = std::slice::from_raw_parts(ptr, n).to_vec();
        fa_free(ptr, n);
        slice
    }};
    let reply: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    match reply.get("error") {{
        Some(err) => Err(err.as_str().map(str::to_string).unwrap_or_else(|| err.to_string())),
        None => Ok(reply.get("ok").cloned().unwrap_or(Value::Null)),
    }}
}}

fn same(a: &Value, b: &Value, loose_empty: bool, tol: Option<f64>) -> bool {{
    match (a, b) {{
        (Value::Number(x), Value::Number(y)) => {{
            if x.is_f64() || y.is_f64() || tol.is_some() {{
                let (x, y) = (x.as_f64().unwrap_or(f64::NAN), y.as_f64().unwrap_or(f64::NAN));
                let rel = tol.unwrap_or(1e-9);
                let abs = tol.unwrap_or(1e-12);
                x == y || (x - y).abs() <= abs.max(rel * x.abs().max(y.abs()))
            }} else {{
                x == y
            }}
        }}
        (Value::Array(x), Value::Array(y)) => {{
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q, loose_empty, tol))
        }}
        (Value::Object(x), Value::Object(y)) => {{
            x.len() == y.len()
                && x.iter()
                    .all(|(k, p)| y.get(k).is_some_and(|q| same(p, q, loose_empty, tol)))
        }}
        (Value::Null, Value::Array(v)) | (Value::Array(v), Value::Null) => loose_empty && v.is_empty(),
        (Value::Null, Value::Object(v)) | (Value::Object(v), Value::Null) => loose_empty && v.is_empty(),
        (Value::Array(a), Value::Object(o)) | (Value::Object(o), Value::Array(a)) => {{
            loose_empty && a.is_empty() && o.is_empty()
        }}
        _ => a == b,
    }}
}}

#[test]
fn recorded_calls_match_parity() {{
    let recordings = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("recordings.jsonl");
    if !recordings.exists() {{
        return;
    }}
    let tol = std::env::var("FNR_FLOAT_TOLERANCE")
        .ok()
        .and_then(|s| s.parse::<f64>().ok());
    let text = std::fs::read_to_string(&recordings).expect("read recordings.jsonl");
    let mut failed = Vec::new();
    for (idx, line) in text.lines().enumerate() {{
        if line.trim().is_empty() {{
            continue;
        }}
        let case: Value = serde_json::from_str(line).expect("parse recording line");
        let got = invoke(&case["args"]);
        let ok = match case.get("raises") {{
            Some(_) => got.is_err(),
            None => got
                .as_ref()
                .is_ok_and(|g| same(&case["result"], g, {loose_empty}, tol)),
        }};
        if !ok {{
            failed.push(format!("case {{idx}}: expected {{}}, got {{got:?}}", case["result"]));
            if failed.len() >= 8 {{
                break;
            }}
        }}
    }}
    assert!(failed.is_empty(), "parity mismatches:\n{{}}", failed.join("\n"));
}}
"#,
    )
}

pub fn cargo_toml(port: &Port) -> String {
    format!(
        "# Rust port of {id}, generated by fast-n-rust.\n\
         [package]\nname = \"{ext}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n\
         [lib]\nname = \"{ext}\"\ncrate-type = [\"cdylib\", \"rlib\"]\n\n\
         [dependencies]\nserde = {{ version = \"1\", features = [\"derive\"] }}\nserde_json = \"1\"\n\n{profile}",
        id = port.id,
        ext = port.ext,
        profile = backend::RELEASE_PROFILE,
    )
}

/// The stub the agent fills in, typed from the original signature where the types are known.
pub fn port_rs(hs: &Hotspot, port: &Port, f: &Flavor) -> String {
    let ty = |t: Option<&str>| {
        t.and_then(f.rust_type)
            .unwrap_or_else(|| "serde_json::Value".to_string())
    };
    let params: Vec<String> = hs
        .params
        .iter()
        .map(|p| format!("{}: {}", snake(&p.name), ty(p.annotation.as_deref())))
        .collect();
    format!(
        "//! Rust port of {id} ({file}:{line}), generated by fast-n-rust.\n\
         //! Replace the todo!() with a faithful, fast port. Arguments arrive already deserialized; change the\n\
         //! types if the recorded calls need others. Return Err(message) where {label} would fail.\n\
         //! Check it with:  fnr verify {id}\n\
         #![allow(non_snake_case)]\n\n\
         #[allow(unused_variables)]\n\
         pub fn {func}({params}) -> Result<{ret}, String> {{\n    todo!(\"port {id}\")\n}}\n",
        id = port.id,
        file = hs.file,
        line = hs.line,
        label = f.lang.label(),
        func = snake(&hs.name),
        params = params.join(", "),
        ret = ty(hs.returns.as_deref()),
    )
}

fn sample_calls(path: &Path, func: &str, n: usize) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return vec![];
    };
    let clip = |s: String, n: usize| {
        if s.chars().count() <= n {
            s
        } else {
            format!("{}…", s.chars().take(n - 1).collect::<String>())
        }
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|c| c.get("derived").is_none())
        .take(n)
        .map(|case| {
            let args: Vec<String> = case["args"]
                .as_array()
                .map(|a| a.iter().map(|v| v.to_string()).collect())
                .unwrap_or_default();
            let outcome = match case.get("raises") {
                Some(e) => format!("fails with {}", e.as_str().unwrap_or("an error")),
                None => case["result"].to_string(),
            };
            format!(
                "{}  ->  {}",
                clip(format!("{func}({})", args.join(", ")), 110),
                clip(outcome, 60)
            )
        })
        .collect()
}

pub fn task_md(
    hs: &Hotspot,
    helpers: &[Hotspot],
    port: &Port,
    crate_dir: &Path,
    f: &Flavor,
) -> String {
    let fence = "```";
    let label = f.lang.label();
    let samples = sample_calls(&crate_dir.join("recordings.jsonl"), &hs.name, 8).join("\n");
    let mut helper_md = String::new();
    if !hs.calls.is_empty() {
        helper_md.push_str(&format!(
            "## Helpers it calls\n\nPort these as private Rust functions in src/port.rs (they stay in {label} for everyone else).\n\n"
        ));
        for name in &hs.calls {
            match helpers.iter().find(|h| &h.name == name) {
                Some(h) => {
                    helper_md.push_str(&format!("{fence}{}\n{}\n{fence}\n\n", f.fence, h.source))
                }
                None => helper_md.push_str(&format!("- `{name}`: read it in {}\n", hs.file)),
            }
        }
    }
    let rules: String = f.rules.iter().map(|r| format!("- {r}\n")).collect();
    format!(
        "# Port `{id}` to Rust\n\n\
You're in a Rust crate fast-n-rust (`fnr`) generated for this: `{dir}`.\n\
Write `{func}` in `src/port.rs` so it behaves **exactly** like the {label} below, and runs fast.\n\
`src/lib.rs` is generated glue (JSON in, JSON out through a C entry point): don't edit it.\n\
Only edit files in this folder. Don't touch the {label} project.\n\n\
## The original ({file}:{line})\n\n{fence}{lang}\n{source}\n{fence}\n\n\
{helper_md}\
## Real calls\n\n\
fnr recorded {recorded} real calls (out of {seen} while the app ran). Arguments arrive as JSON. A few:\n\n{fence}\n{samples}\n{fence}\n\n\
## Rules\n\n\
- Same result for every input, edge cases included: empty input, zero, negatives, non-ASCII text, very long input. verify tests these too.\n\
{rules}\
- Return `Err(message)` where the original would fail.\n\
- Make it fast: no needless allocation, borrow, `with_capacity`, fast paths for common cases. Threads are fine if the result is the same.\n\
- Keep the function name.\n\n\
## Check\n\n\
Run `cargo check` here. Then fnr builds the crate in release mode and replays all recorded calls against it\n\
(`fnr verify {id}`).\n",
        id = port.id,
        dir = crate_dir.display(),
        func = hs.name,
        file = hs.file,
        line = hs.line,
        lang = f.fence,
        source = hs.source,
        recorded = port.recorded,
        seen = util::thousands(port.calls_seen),
    )
}

const FA_BIN_CODEC: &str = r#"use std::collections::{BTreeMap, HashMap};

pub enum BinAtom {
    I32(i32),
    I64(i64),
    F64(f64),
}

pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self { buf: Vec::with_capacity(256) }
    }
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
    pub fn push(&mut self, b: u8) {
        self.buf.push(b);
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_bits().to_le_bytes());
    }
    pub fn str(&mut self, s: &str) {
        self.buf.push(5);
        self.u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
    }
}

pub struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    pub fn peek(&self) -> Result<u8, String> {
        self.bytes.get(self.pos).copied().ok_or_else(|| "unexpected end of binary input".into())
    }
    pub fn u8(&mut self) -> Result<u8, String> {
        let b = self.peek()?;
        self.pos += 1;
        Ok(b)
    }
    pub fn u32(&mut self) -> Result<u32, String> {
        let end = self.pos + 4;
        let slice = self.bytes.get(self.pos..end).ok_or_else(|| "truncated u32".to_string())?;
        self.pos = end;
        Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }
    pub fn i32(&mut self) -> Result<i32, String> {
        let end = self.pos + 4;
        let slice = self.bytes.get(self.pos..end).ok_or_else(|| "truncated i32".to_string())?;
        self.pos = end;
        Ok(i32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }
    pub fn i64(&mut self) -> Result<i64, String> {
        let end = self.pos + 8;
        let slice = self.bytes.get(self.pos..end).ok_or_else(|| "truncated i64".to_string())?;
        self.pos = end;
        Ok(i64::from_le_bytes([
            slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
        ]))
    }
    pub fn u64(&mut self) -> Result<u64, String> {
        Ok(self.i64()? as u64)
    }
    pub fn f64(&mut self) -> Result<f64, String> {
        let end = self.pos + 8;
        let slice = self.bytes.get(self.pos..end).ok_or_else(|| "truncated f64".to_string())?;
        self.pos = end;
        Ok(f64::from_bits(u64::from_le_bytes([
            slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
        ])))
    }
    pub fn str_body(&mut self) -> Result<String, String> {
        let n = self.u32()? as usize;
        let end = self.pos + n;
        let slice = self.bytes.get(self.pos..end).ok_or_else(|| "truncated string".to_string())?;
        self.pos = end;
        std::str::from_utf8(slice).map(str::to_string).map_err(|e| e.to_string())
    }
    pub fn args_header(&mut self) -> Result<usize, String> {
        let tag = self.u8()?;
        if tag != 6 {
            return Err(format!("expected binary arg array (tag 6), got {tag}"));
        }
        Ok(self.u32()? as usize)
    }
}

pub trait BinCodec: Sized {
    fn bin_enc(&self, w: &mut Writer);
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String>;
    fn from_atom(_atom: BinAtom) -> Result<Self, String> {
        Err("type mismatch for packed scalar array".into())
    }
}

pub fn encode_err(msg: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.push(0);
    w.u32(msg.len() as u32);
    w.buf.extend_from_slice(msg.as_bytes());
    w.into_vec()
}

pub fn arg<T: BinCodec + serde::de::DeserializeOwned>(
    r: &mut Reader<'_>,
    has_val: bool,
    i: usize,
    default: Option<&str>,
) -> Result<T, String> {
    if has_val {
        T::bin_dec(r).map_err(|e| format!("argument {}: {e}", i + 1))
    } else if let Some(d) = default {
        serde_json::from_str(d).map_err(|e| e.to_string())
    } else {
        Err(format!("missing argument {}", i + 1))
    }
}

fn read_int_val(r: &mut Reader<'_>) -> Result<i64, String> {
    match r.u8()? {
        11 => Ok(i64::from(r.i32()?)),
        3 => r.i64(),
        12 => i64::try_from(r.u64()?).map_err(|_| "u64 out of i64 range".into()),
        4 => Ok(r.f64()? as i64),
        t => Err(format!("expected integer tag, got {t}")),
    }
}

fn read_u64_val(r: &mut Reader<'_>) -> Result<u64, String> {
    match r.u8()? {
        12 => r.u64(),
        11 => u64::try_from(r.i32()?).map_err(|_| "negative i32 for unsigned int".into()),
        3 => u64::try_from(r.i64()?).map_err(|_| "negative i64 for unsigned int".into()),
        4 => Ok(r.f64()? as u64),
        t => Err(format!("expected unsigned int tag, got {t}")),
    }
}

fn read_float_val(r: &mut Reader<'_>) -> Result<f64, String> {
    match r.u8()? {
        4 => r.f64(),
        11 => Ok(f64::from(r.i32()?)),
        3 => Ok(r.i64()? as f64),
        12 => Ok(r.u64()? as f64),
        t => Err(format!("expected float tag, got {t}")),
    }
}

impl BinCodec for bool {
    fn bin_enc(&self, w: &mut Writer) {
        w.push(if *self { 2 } else { 1 });
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        match r.u8()? {
            1 => Ok(false),
            2 => Ok(true),
            t => Err(format!("expected bool tag, got {t}")),
        }
    }
}

impl BinCodec for i32 {
    fn bin_enc(&self, w: &mut Writer) {
        w.push(11);
        w.i32(*self);
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        let v = read_int_val(r)?;
        i32::try_from(v).map_err(|_| format!("integer {v} out of i32 range"))
    }
    fn from_atom(atom: BinAtom) -> Result<Self, String> {
        match atom {
            BinAtom::I32(v) => Ok(v),
            BinAtom::I64(v) => i32::try_from(v).map_err(|_| "out of i32 range".into()),
            BinAtom::F64(v) => Ok(v as i32),
        }
    }
}

impl BinCodec for i64 {
    fn bin_enc(&self, w: &mut Writer) {
        w.push(3);
        w.i64(*self);
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        read_int_val(r)
    }
    fn from_atom(atom: BinAtom) -> Result<Self, String> {
        match atom {
            BinAtom::I32(v) => Ok(i64::from(v)),
            BinAtom::I64(v) => Ok(v),
            BinAtom::F64(v) => Ok(v as i64),
        }
    }
}

macro_rules! impl_small_int {
    ($($ty:ty),*) => {
        $(
            impl BinCodec for $ty {
                fn bin_enc(&self, w: &mut Writer) {
                    w.push(11);
                    w.i32(i32::from(*self));
                }
                fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
                    let v = read_int_val(r)?;
                    <$ty>::try_from(v).map_err(|_| format!("integer {v} out of range"))
                }
                fn from_atom(atom: BinAtom) -> Result<Self, String> {
                    match atom {
                        BinAtom::I32(v) => <$ty>::try_from(v).map_err(|_| "out of range".into()),
                        BinAtom::I64(v) => <$ty>::try_from(v).map_err(|_| "out of range".into()),
                        BinAtom::F64(v) => <$ty>::try_from(v as i64).map_err(|_| "out of range".into()),
                    }
                }
            }
        )*
    };
}
impl_small_int!(i8, i16, u8, u16);

impl BinCodec for isize {
    fn bin_enc(&self, w: &mut Writer) {
        w.push(3);
        w.i64(*self as i64);
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        let v = read_int_val(r)?;
        isize::try_from(v).map_err(|_| format!("integer {v} out of range"))
    }
    fn from_atom(atom: BinAtom) -> Result<Self, String> {
        match atom {
            BinAtom::I32(v) => isize::try_from(v).map_err(|_| "out of range".into()),
            BinAtom::I64(v) => isize::try_from(v).map_err(|_| "out of range".into()),
            BinAtom::F64(v) => isize::try_from(v as i64).map_err(|_| "out of range".into()),
        }
    }
}

impl BinCodec for u32 {
    fn bin_enc(&self, w: &mut Writer) {
        if *self <= i32::MAX as u32 {
            w.push(11);
            w.i32(*self as i32);
        } else {
            w.push(3);
            w.i64(i64::from(*self));
        }
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        let v = read_u64_val(r)?;
        u32::try_from(v).map_err(|_| format!("integer {v} out of u32 range"))
    }
    fn from_atom(atom: BinAtom) -> Result<Self, String> {
        match atom {
            BinAtom::I32(v) => u32::try_from(v).map_err(|_| "out of range".into()),
            BinAtom::I64(v) => u32::try_from(v).map_err(|_| "out of range".into()),
            BinAtom::F64(v) => u32::try_from(v as i64).map_err(|_| "out of range".into()),
        }
    }
}

macro_rules! impl_unsigned_wide {
    ($($ty:ty),*) => {
        $(
            impl BinCodec for $ty {
                fn bin_enc(&self, w: &mut Writer) {
                    w.push(12);
                    w.u64(*self as u64);
                }
                fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
                    let v = read_u64_val(r)?;
                    <$ty>::try_from(v).map_err(|_| format!("integer {v} out of range"))
                }
                fn from_atom(atom: BinAtom) -> Result<Self, String> {
                    match atom {
                        BinAtom::I32(v) => <$ty>::try_from(v).map_err(|_| "out of range".into()),
                        BinAtom::I64(v) => <$ty>::try_from(v).map_err(|_| "out of range".into()),
                        BinAtom::F64(v) => <$ty>::try_from(v as u64).map_err(|_| "out of range".into()),
                    }
                }
            }
        )*
    };
}
impl_unsigned_wide!(u64, usize);

impl BinCodec for f32 {
    fn bin_enc(&self, w: &mut Writer) {
        w.push(4);
        w.f64(f64::from(*self));
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        Ok(read_float_val(r)? as f32)
    }
    fn from_atom(atom: BinAtom) -> Result<Self, String> {
        Ok(match atom {
            BinAtom::I32(v) => v as f32,
            BinAtom::I64(v) => v as f32,
            BinAtom::F64(v) => v as f32,
        })
    }
}

impl BinCodec for f64 {
    fn bin_enc(&self, w: &mut Writer) {
        w.push(4);
        w.f64(*self);
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        read_float_val(r)
    }
    fn from_atom(atom: BinAtom) -> Result<Self, String> {
        Ok(match atom {
            BinAtom::I32(v) => f64::from(v),
            BinAtom::I64(v) => v as f64,
            BinAtom::F64(v) => v,
        })
    }
}

impl BinCodec for String {
    fn bin_enc(&self, w: &mut Writer) {
        w.str(self);
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        match r.u8()? {
            5 => r.str_body(),
            t => Err(format!("expected string tag, got {t}")),
        }
    }
}

impl<T: BinCodec> BinCodec for Option<T> {
    fn bin_enc(&self, w: &mut Writer) {
        match self {
            Some(v) => v.bin_enc(w),
            None => w.push(0),
        }
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        if r.peek()? == 0 {
            let _ = r.u8()?;
            Ok(None)
        } else {
            Ok(Some(T::bin_dec(r)?))
        }
    }
}

impl<T: BinCodec> BinCodec for Vec<T> {
    fn bin_enc(&self, w: &mut Writer) {
        w.push(6);
        w.u32(self.len() as u32);
        for item in self {
            item.bin_enc(w);
        }
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        let tag = r.u8()?;
        let n = r.u32()? as usize;
        let mut out = Vec::with_capacity(n);
        match tag {
            6 => {
                for _ in 0..n {
                    out.push(T::bin_dec(r)?);
                }
            }
            8 => {
                for _ in 0..n {
                    out.push(T::from_atom(BinAtom::I32(r.i32()?))?);
                }
            }
            9 => {
                for _ in 0..n {
                    out.push(T::from_atom(BinAtom::I64(r.i64()?))?);
                }
            }
            10 => {
                for _ in 0..n {
                    out.push(T::from_atom(BinAtom::F64(r.f64()?))?);
                }
            }
            0 => return Ok(Vec::new()),
            _ => return Err(format!("expected array tag, got {tag}")),
        }
        Ok(out)
    }
}

impl<T: BinCodec, const N: usize> BinCodec for [T; N] {
    fn bin_enc(&self, w: &mut Writer) {
        w.push(6);
        w.u32(N as u32);
        for item in self {
            item.bin_enc(w);
        }
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        let tag = r.u8()?;
        let n = r.u32()? as usize;
        if n != N {
            return Err(format!("expected {N}-element array, got {n}"));
        }
        let mut out = Vec::with_capacity(N);
        match tag {
            6 => {
                for _ in 0..N {
                    out.push(T::bin_dec(r)?);
                }
            }
            8 => {
                for _ in 0..N {
                    out.push(T::from_atom(BinAtom::I32(r.i32()?))?);
                }
            }
            9 => {
                for _ in 0..N {
                    out.push(T::from_atom(BinAtom::I64(r.i64()?))?);
                }
            }
            10 => {
                for _ in 0..N {
                    out.push(T::from_atom(BinAtom::F64(r.f64()?))?);
                }
            }
            _ => return Err(format!("expected array tag, got {tag}")),
        }
        out.try_into().map_err(|_| format!("expected {N} elements"))
    }
}

impl<A: BinCodec, B: BinCodec> BinCodec for (A, B) {
    fn bin_enc(&self, w: &mut Writer) {
        w.push(6);
        w.u32(2);
        self.0.bin_enc(w);
        self.1.bin_enc(w);
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        if r.u8()? != 6 || r.u32()? != 2 {
            return Err("expected 2-tuple".into());
        }
        Ok((A::bin_dec(r)?, B::bin_dec(r)?))
    }
}

impl<A: BinCodec, B: BinCodec, C: BinCodec> BinCodec for (A, B, C) {
    fn bin_enc(&self, w: &mut Writer) {
        w.push(6);
        w.u32(3);
        self.0.bin_enc(w);
        self.1.bin_enc(w);
        self.2.bin_enc(w);
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        if r.u8()? != 6 || r.u32()? != 3 {
            return Err("expected 3-tuple".into());
        }
        Ok((A::bin_dec(r)?, B::bin_dec(r)?, C::bin_dec(r)?))
    }
}

impl<A: BinCodec, B: BinCodec, C: BinCodec, D: BinCodec> BinCodec for (A, B, C, D) {
    fn bin_enc(&self, w: &mut Writer) {
        w.push(6);
        w.u32(4);
        self.0.bin_enc(w);
        self.1.bin_enc(w);
        self.2.bin_enc(w);
        self.3.bin_enc(w);
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        if r.u8()? != 6 || r.u32()? != 4 {
            return Err("expected 4-tuple".into());
        }
        Ok((A::bin_dec(r)?, B::bin_dec(r)?, C::bin_dec(r)?, D::bin_dec(r)?))
    }
}

impl<V: BinCodec> BinCodec for HashMap<String, V> {
    fn bin_enc(&self, w: &mut Writer) {
        w.push(7);
        w.u32(self.len() as u32);
        for (k, v) in self {
            w.u32(k.len() as u32);
            w.buf.extend_from_slice(k.as_bytes());
            v.bin_enc(w);
        }
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        match r.u8()? {
            7 => {
                let n = r.u32()? as usize;
                let mut m = HashMap::with_capacity(n);
                for _ in 0..n {
                    let k = r.str_body()?;
                    m.insert(k, V::bin_dec(r)?);
                }
                Ok(m)
            }
            0 => Ok(HashMap::new()),
            t => Err(format!("expected map tag, got {t}")),
        }
    }
}

impl<V: BinCodec> BinCodec for BTreeMap<String, V> {
    fn bin_enc(&self, w: &mut Writer) {
        w.push(7);
        w.u32(self.len() as u32);
        for (k, v) in self {
            w.u32(k.len() as u32);
            w.buf.extend_from_slice(k.as_bytes());
            v.bin_enc(w);
        }
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        match r.u8()? {
            7 => {
                let n = r.u32()? as usize;
                let mut m = BTreeMap::new();
                for _ in 0..n {
                    let k = r.str_body()?;
                    m.insert(k, V::bin_dec(r)?);
                }
                Ok(m)
            }
            0 => Ok(BTreeMap::new()),
            t => Err(format!("expected map tag, got {t}")),
        }
    }
}

impl BinCodec for super::Value {
    fn bin_enc(&self, w: &mut Writer) {
        match self {
            super::Value::Null => w.push(0),
            super::Value::Bool(b) => b.bin_enc(w),
            super::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    if let Ok(i32v) = i32::try_from(i) {
                        w.push(11);
                        w.i32(i32v);
                    } else {
                        w.push(3);
                        w.i64(i);
                    }
                } else if let Some(u) = n.as_u64() {
                    w.push(12);
                    w.u64(u);
                } else {
                    w.push(4);
                    w.f64(n.as_f64().unwrap_or(0.0));
                }
            }
            super::Value::String(s) => w.str(s),
            super::Value::Array(arr) => {
                w.push(6);
                w.u32(arr.len() as u32);
                for x in arr {
                    x.bin_enc(w);
                }
            }
            super::Value::Object(obj) => {
                w.push(7);
                w.u32(obj.len() as u32);
                for (k, v) in obj {
                    w.u32(k.len() as u32);
                    w.buf.extend_from_slice(k.as_bytes());
                    v.bin_enc(w);
                }
            }
        }
    }
    fn bin_dec(r: &mut Reader<'_>) -> Result<Self, String> {
        match r.u8()? {
            0 => Ok(super::Value::Null),
            1 => Ok(super::Value::Bool(false)),
            2 => Ok(super::Value::Bool(true)),
            3 => Ok(super::Value::from(r.i64()?)),
            4 => Ok(super::Value::from(r.f64()?)),
            5 => Ok(super::Value::String(r.str_body()?)),
            6 => {
                let n = r.u32()? as usize;
                let mut out = Vec::with_capacity(n);
                for _ in 0..n {
                    out.push(Self::bin_dec(r)?);
                }
                Ok(super::Value::Array(out))
            }
            7 => {
                let n = r.u32()? as usize;
                let mut out = serde_json::Map::new();
                for _ in 0..n {
                    let k = r.str_body()?;
                    out.insert(k, Self::bin_dec(r)?);
                }
                Ok(super::Value::Object(out))
            }
            8 => {
                let n = r.u32()? as usize;
                let mut out = Vec::with_capacity(n);
                for _ in 0..n {
                    out.push(super::Value::from(r.i32()?));
                }
                Ok(super::Value::Array(out))
            }
            9 => {
                let n = r.u32()? as usize;
                let mut out = Vec::with_capacity(n);
                for _ in 0..n {
                    out.push(super::Value::from(r.i64()?));
                }
                Ok(super::Value::Array(out))
            }
            10 => {
                let n = r.u32()? as usize;
                let mut out = Vec::with_capacity(n);
                for _ in 0..n {
                    out.push(super::Value::from(r.f64()?));
                }
                Ok(super::Value::Array(out))
            }
            11 => Ok(super::Value::from(r.i32()?)),
            12 => Ok(super::Value::from(r.u64()?)),
            t => Err(format!("unknown binary wire tag {t}")),
        }
    }
    fn from_atom(atom: BinAtom) -> Result<Self, String> {
        Ok(match atom {
            BinAtom::I32(v) => super::Value::from(v),
            BinAtom::I64(v) => super::Value::from(v),
            BinAtom::F64(v) => super::Value::from(v),
        })
    }
}"#;

/// Exact C entry points generated by `glue_rs_with_flavor`.
pub fn scaffold(
    ctx: &Ctx,
    hs: &Hotspot,
    helpers: &[Hotspot],
    port: &Port,
    f: &Flavor,
) -> Result<String> {
    let dir = ctx.crate_dir(port);
    fs::create_dir_all(dir.join("src"))?;
    fs::create_dir_all(dir.join("tests"))?;
    fs::create_dir_all(dir.join(".cargo"))?;
    fs::write(dir.join("Cargo.toml"), cargo_toml(port))?;
    fs::write(dir.join(".cargo/config.toml"), backend::CARGO_CONFIG)?;
    let lib_path = dir.join("src/lib.rs");
    let parity_path = dir.join("tests/recorded_parity.rs");
    fs::write(&lib_path, glue_rs_with_flavor(hs, port, f))?;
    fs::write(&parity_path, recorded_parity_test(port, f.lang == Lang::Go))?;
    let body = dir.join("src/port.rs");
    let created_body = !body.exists();
    if created_body {
        fs::write(&body, port_rs(hs, port, f))?;
    }
    let mut fmt_files = vec![lib_path, parity_path];
    if created_body {
        fmt_files.push(body);
    }
    let _ = Command::new("rustfmt")
        .args(["--edition", "2024"])
        .args(&fmt_files)
        .output();
    let task = task_md(hs, helpers, port, &dir, f);
    fs::write(dir.join("TASK.md"), &task)?;
    Ok(task)
}

// ------------------------------------------------------------------ replay

#[derive(Deserialize)]
struct ReplayJob {
    lib: PathBuf,
    cases: PathBuf,
    #[serde(default)]
    meta: PathBuf,
    func: String,
    out: PathBuf,
    /// Go: a nil slice or map records as null; the port's empty Vec is the same thing.
    #[serde(default)]
    loose_empty: bool,
    #[serde(default = "half")]
    budget: f64,
    #[serde(default)]
    float_tolerance: Option<f64>,
    #[serde(default)]
    known_diffs: Option<PathBuf>,
    #[serde(default)]
    fuzz_mode: bool,
    #[serde(default)]
    oracle_out: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ShrunkFailure {
    pub case: usize,
    pub original_args: Value,
    pub shrunk_args: Value,
    pub expected: String,
    pub got: String,
    pub probes: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NativeFuzzReport {
    pub mode: String,
    pub total: usize,
    pub passed: usize,
    pub mismatches: usize,
    pub panics: usize,
    pub nondeterministic: usize,
    pub errors: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle_note: Option<String>,
    pub failures: Vec<Failure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shrunk: Vec<ShrunkFailure>,
}

fn half() -> f64 {
    0.5
}

/// Build-and-check for C-bridge ports: replays the recordings in a child process.
pub fn verify(ctx: &Ctx, port: &Port, loose_empty: bool) -> Result<Verify> {
    verify_with(ctx, port, loose_empty, None, None)
}

/// Build-and-check with optional float tolerance and known-diffs waiver file.
pub fn verify_with(
    ctx: &Ctx,
    port: &Port,
    loose_empty: bool,
    float_tolerance: Option<f64>,
    known_diffs: Option<&Path>,
) -> Result<Verify> {
    let dir = ctx.crate_dir(port);
    let (_, lib) = port.lang.artifact(&port.ext);
    let out = dir.join("verify.json");
    let _ = fs::remove_file(&out);
    let default_kd = dir.join("known-diffs.jsonl");
    let kd = known_diffs
        .map(Path::to_path_buf)
        .or_else(|| default_kd.is_file().then_some(default_kd));
    let job = json!({
        "lib": ctx.lib_dir().join(lib),
        "cases": dir.join("recordings.jsonl"),
        "meta": dir.join("recordings.meta.json"),
        "func": port.func,
        "out": out,
        "loose_empty": loose_empty,
        "float_tolerance": float_tolerance,
        "known_diffs": kd,
    });
    let job_path = dir.join("job-verify.json");
    fs::write(&job_path, serde_json::to_vec_pretty(&job)?)?;
    let me = std::env::current_exe().context("finding fnr itself")?;
    let run = output(Command::new(me).arg("__replay").arg(&job_path), "fnr")?;
    let mut v = if run.status.success() && out.exists() {
        serde_json::from_str::<Verify>(&fs::read_to_string(&out)?)
            .context("reading verify results")?
    } else {
        let total = fs::read_to_string(dir.join("recordings.jsonl"))
            .map(|t| t.lines().count())
            .unwrap_or(0);
        Verify {
            total,
            failed: total,
            failures: vec![Failure {
                case: 0,
                call: format!("{}(...) (replaying the recorded calls)", port.func),
                expected: "the same results".into(),
                got: format!(
                    "the port crashed ({}): {}",
                    run.status,
                    tail(&combined(&run), 12)
                ),
            }],
            ..Verify::default()
        }
    };
    v.built = true;
    Ok(v)
}

/// Run synthesized fuzz cases against a compiled C-bridge port in an isolated child process.
pub fn fuzz_cases(
    ctx: &Ctx,
    port: &Port,
    loose_empty: bool,
    cases_path: &Path,
    out_path: &Path,
    float_tolerance: Option<f64>,
) -> Result<NativeFuzzReport> {
    fuzz_cases_inner(
        ctx,
        port,
        loose_empty,
        cases_path,
        None,
        out_path,
        float_tolerance,
        None,
    )
}

/// Run differential fuzz cases comparing the Rust port against outputs generated by the original runtime.
#[allow(clippy::too_many_arguments)]
pub fn fuzz_cases_against_oracle(
    ctx: &Ctx,
    port: &Port,
    loose_empty: bool,
    cases_path: &Path,
    oracle_out_path: &Path,
    out_path: &Path,
    float_tolerance: Option<f64>,
    oracle_note: Option<String>,
) -> Result<NativeFuzzReport> {
    fuzz_cases_inner(
        ctx,
        port,
        loose_empty,
        cases_path,
        Some(oracle_out_path),
        out_path,
        float_tolerance,
        oracle_note,
    )
}

#[allow(clippy::too_many_arguments)]
fn fuzz_cases_inner(
    ctx: &Ctx,
    port: &Port,
    loose_empty: bool,
    cases_path: &Path,
    oracle_out_path: Option<&Path>,
    out_path: &Path,
    float_tolerance: Option<f64>,
    oracle_note: Option<String>,
) -> Result<NativeFuzzReport> {
    let dir = ctx.crate_dir(port);
    let (_, lib) = port.lang.artifact(&port.ext);
    let _ = fs::remove_file(out_path);
    let job = json!({
        "lib": ctx.lib_dir().join(lib),
        "cases": cases_path,
        "func": port.func,
        "out": out_path,
        "loose_empty": loose_empty,
        "float_tolerance": float_tolerance,
        "fuzz_mode": true,
        "oracle_out": oracle_out_path,
    });
    let job_path = dir.join("job-fuzz.json");
    fs::write(&job_path, serde_json::to_vec_pretty(&job)?)?;
    let me = std::env::current_exe().context("finding fnr itself")?;
    let run = output(Command::new(me).arg("__replay").arg(&job_path), "fnr")?;
    if run.status.success() && out_path.exists() {
        let mut rep: NativeFuzzReport =
            serde_json::from_str(&fs::read_to_string(out_path)?).context("reading fuzz results")?;
        if rep.oracle_note.is_none() {
            rep.oracle_note = oracle_note;
        }
        Ok(rep)
    } else {
        let total = fs::read_to_string(cases_path)
            .map(|t| t.lines().count())
            .unwrap_or(0);
        Ok(NativeFuzzReport {
            mode: if oracle_out_path.is_some() {
                "differential".into()
            } else {
                "crash+determinism".into()
            },
            total,
            panics: 1,
            oracle_note,
            failures: vec![Failure {
                case: 0,
                call: format!("{}(...) (fuzzing)", port.func),
                expected: "no crash".into(),
                got: format!(
                    "the port crashed ({}): {}",
                    run.status,
                    tail(&combined(&run), 12)
                ),
            }],
            ..NativeFuzzReport::default()
        })
    }
}

type FaCall = unsafe extern "C" fn(*const u8, usize, *mut usize) -> *mut u8;
type FaBin = unsafe extern "C" fn(*const u8, usize, *mut usize) -> *mut u8;
type FaFree = unsafe extern "C" fn(*mut u8, usize);

fn bin_enc_val(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(0),
        Value::Bool(b) => out.push(if *b { 2 } else { 1 }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if let Ok(i32v) = i32::try_from(i) {
                    out.push(11);
                    out.extend_from_slice(&i32v.to_le_bytes());
                } else {
                    out.push(3);
                    out.extend_from_slice(&i.to_le_bytes());
                }
            } else if let Some(u) = n.as_u64() {
                out.push(12);
                out.extend_from_slice(&u.to_le_bytes());
            } else {
                out.push(4);
                out.extend_from_slice(&n.as_f64().unwrap_or(0.0).to_bits().to_le_bytes());
            }
        }
        Value::String(s) => {
            out.push(5);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Array(arr) => {
            out.push(6);
            out.extend_from_slice(&(arr.len() as u32).to_le_bytes());
            for item in arr {
                bin_enc_val(item, out);
            }
        }
        Value::Object(obj) => {
            out.push(7);
            out.extend_from_slice(&(obj.len() as u32).to_le_bytes());
            for (k, val) in obj {
                out.extend_from_slice(&(k.len() as u32).to_le_bytes());
                out.extend_from_slice(k.as_bytes());
                bin_enc_val(val, out);
            }
        }
    }
}

fn bin_dec_val(bytes: &[u8], pos: &mut usize) -> std::result::Result<Value, String> {
    let get_u8 = |p: &mut usize| -> std::result::Result<u8, String> {
        let b = bytes.get(*p).copied().ok_or_else(|| "eof".to_string())?;
        *p += 1;
        Ok(b)
    };
    let get_u32 = |p: &mut usize| -> std::result::Result<u32, String> {
        let s = bytes.get(*p..*p + 4).ok_or_else(|| "eof u32".to_string())?;
        *p += 4;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    };
    let get_i32 = |p: &mut usize| -> std::result::Result<i32, String> {
        let s = bytes.get(*p..*p + 4).ok_or_else(|| "eof i32".to_string())?;
        *p += 4;
        Ok(i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    };
    let get_i64 = |p: &mut usize| -> std::result::Result<i64, String> {
        let s = bytes.get(*p..*p + 8).ok_or_else(|| "eof i64".to_string())?;
        *p += 8;
        Ok(i64::from_le_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ]))
    };
    let get_f64 = |p: &mut usize| -> std::result::Result<f64, String> {
        let s = bytes.get(*p..*p + 8).ok_or_else(|| "eof f64".to_string())?;
        *p += 8;
        Ok(f64::from_bits(u64::from_le_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ])))
    };
    let get_str = |p: &mut usize| -> std::result::Result<String, String> {
        let n = get_u32(p)? as usize;
        let s = bytes.get(*p..*p + n).ok_or_else(|| "eof str".to_string())?;
        *p += n;
        std::str::from_utf8(s)
            .map(str::to_string)
            .map_err(|e| e.to_string())
    };
    match get_u8(pos)? {
        0 => Ok(Value::Null),
        1 => Ok(Value::Bool(false)),
        2 => Ok(Value::Bool(true)),
        3 => Ok(Value::from(get_i64(pos)?)),
        4 => Ok(Value::from(get_f64(pos)?)),
        5 => Ok(Value::String(get_str(pos)?)),
        6 => {
            let n = get_u32(pos)? as usize;
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                out.push(bin_dec_val(bytes, pos)?);
            }
            Ok(Value::Array(out))
        }
        7 => {
            let n = get_u32(pos)? as usize;
            let mut m = serde_json::Map::new();
            for _ in 0..n {
                let k = get_str(pos)?;
                m.insert(k, bin_dec_val(bytes, pos)?);
            }
            Ok(Value::Object(m))
        }
        8 => {
            let n = get_u32(pos)? as usize;
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                out.push(Value::from(get_i32(pos)?));
            }
            Ok(Value::Array(out))
        }
        9 => {
            let n = get_u32(pos)? as usize;
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                out.push(Value::from(get_i64(pos)?));
            }
            Ok(Value::Array(out))
        }
        10 => {
            let n = get_u32(pos)? as usize;
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                out.push(Value::from(get_f64(pos)?));
            }
            Ok(Value::Array(out))
        }
        11 => Ok(Value::from(get_i32(pos)?)),
        12 => Ok(Value::from(get_i64(pos)? as u64)),
        t => Err(format!("bad wire tag {t}")),
    }
}

fn bin_dec_reply(bytes: &[u8]) -> std::result::Result<Value, String> {
    let mut pos = 1usize;
    match bytes.first().copied() {
        Some(1) => bin_dec_val(bytes, &mut pos),
        Some(0) => {
            let s = bytes
                .get(1..5)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
                .unwrap_or(0);
            let msg = bytes
                .get(5..5 + s)
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or("binary error");
            Err(msg.to_string())
        }
        _ => Err("empty binary reply".into()),
    }
}

fn show(v: &Value, limit: usize) -> String {
    let s = v.to_string();
    if s.chars().count() <= limit {
        s
    } else {
        format!("{}…", s.chars().take(limit - 1).collect::<String>())
    }
}

fn load_known_diffs(path: Option<&Path>) -> HashSet<String> {
    let Some(p) = path else { return HashSet::new() };
    let Ok(text) = fs::read_to_string(p) else {
        return HashSet::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| serde_json::to_string(&v["args"]).ok())
        .collect()
}

/// The hidden  fnr __replay <job>  command.
pub fn replay(job_path: &Path) -> Result<()> {
    let job: ReplayJob =
        serde_json::from_str(&fs::read_to_string(job_path)?).context("reading the replay job")?;
    // SAFETY: the library is the port fnr just built; its entry points have these signatures.
    let lib = unsafe { libloading::Library::new(&job.lib) }
        .with_context(|| format!("loading {}", job.lib.display()))?;
    let call: libloading::Symbol<FaCall> =
        unsafe { lib.get(b"fa_call") }.context("fa_call missing")?;
    let free: libloading::Symbol<FaFree> =
        unsafe { lib.get(b"fa_free") }.context("fa_free missing")?;
    let has_direct = unsafe { lib.get::<*const ()>(b"fa_direct") }.is_ok();
    let bin_sym = unsafe { lib.get::<FaBin>(b"fa_bin") }.ok();
    let invoke = |input: &[u8]| -> std::result::Result<Value, String> {
        let mut n = 0usize;
        let bytes = unsafe {
            let p = call(input.as_ptr(), input.len(), &mut n);
            let bytes = std::slice::from_raw_parts(p, n).to_vec();
            free(p, n);
            bytes
        };
        let reply: Value = serde_json::from_slice(&bytes).map_err(|e| format!("bad reply: {e}"))?;
        match reply.get("error") {
            Some(e) => Err(e
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| e.to_string())),
            None => Ok(reply.get("ok").cloned().unwrap_or(Value::Null)),
        }
    };

    if job.fuzz_mode {
        let text = fs::read_to_string(&job.cases).context("reading fuzz cases")?;
        let cases: Vec<Value> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let oracle: Option<Vec<Value>> = job.oracle_out.as_ref().filter(|p| p.exists()).map(|p| {
            fs::read_to_string(p)
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .collect()
        });
        let has_oracle = oracle.as_ref().is_some_and(|o| o.len() == cases.len());
        let mut rep = NativeFuzzReport {
            mode: if has_oracle {
                "differential".into()
            } else {
                "crash+determinism".into()
            },
            total: cases.len(),
            ..NativeFuzzReport::default()
        };
        for (i, case) in cases.iter().enumerate() {
            let input = serde_json::to_vec(&case["args"])?;
            let r1 = invoke(&input);
            let r2 = invoke(&input);
            let is_panic = |r: &std::result::Result<Value, String>| {
                r.as_ref().is_err_and(|e| e.starts_with("panic:"))
            };
            if is_panic(&r1) || is_panic(&r2) {
                rep.panics += 1;
                if rep.failures.len() < 8 {
                    rep.failures.push(Failure {
                        case: i,
                        call: format!("{}({})", job.func, show(&case["args"], 120)),
                        expected: "no panic".into(),
                        got: format!("{r1:?}"),
                    });
                }
                continue;
            }
            let det = match (&r1, &r2) {
                (Ok(a), Ok(b)) => same_with_tol(a, b, job.loose_empty, job.float_tolerance),
                (Err(a), Err(b)) => a == b,
                _ => false,
            };
            if !det {
                rep.nondeterministic += 1;
                if rep.failures.len() < 8 {
                    rep.failures.push(Failure {
                        case: i,
                        call: format!("{}({})", job.func, show(&case["args"], 120)),
                        expected: format!("{r1:?}"),
                        got: format!("{r2:?}"),
                    });
                }
                continue;
            }
            if let Some(ref o_rows) = oracle
                && has_oracle
            {
                let want_row = &o_rows[i];
                let ok = if want_row.get("error").is_some() {
                    r1.is_err()
                } else {
                    let want = &want_row["ok"];
                    r1.as_ref()
                        .is_ok_and(|g| same_with_tol(want, g, job.loose_empty, job.float_tolerance))
                };
                if !ok {
                    rep.mismatches += 1;
                    if rep.failures.len() < 8 {
                        rep.failures.push(Failure {
                            case: i,
                            call: format!("{}({})", job.func, show(&case["args"], 120)),
                            expected: if let Some(e) = want_row.get("error") {
                                format!("error: {}", show(e, 160))
                            } else {
                                show(&want_row["ok"], 160)
                            },
                            got: match &r1 {
                                Ok(v) => show(v, 160),
                                Err(e) => format!("error: {e}"),
                            },
                        });
                    }
                    continue;
                }
            }
            if r1.is_err() {
                rep.errors += 1;
            }
            rep.passed += 1;
        }
        fs::write(&job.out, serde_json::to_vec(&rep)?)?;
        return Ok(());
    }

    let waived_set = load_known_diffs(job.known_diffs.as_deref());
    let text = fs::read_to_string(&job.cases).context("reading recordings.jsonl")?;
    let cases: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let (mut passed, mut failed, mut waived, mut derived, mut edges) = (0, 0, 0, 0, 0);
    let mut failures = Vec::new();
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        let input = serde_json::to_vec(&case["args"])?;
        let is_derived = case.get("derived").is_some();
        let is_edge = case["derived"] == "edge";
        if is_derived {
            derived += 1;
            edges += usize::from(is_edge);
        } else {
            inputs.push(input.clone());
            outputs.push(case["result"].clone());
        }
        let got = invoke(&input);
        let (ok, expected) = match case.get("raises") {
            Some(e) => (
                got.is_err(),
                format!("fails with {}", e.as_str().unwrap_or("an error")),
            ),
            None => {
                let want = &case["result"];
                (
                    got.as_ref().is_ok_and(|g| {
                        same_with_tol(want, g, job.loose_empty, job.float_tolerance)
                    }),
                    show(want, 160),
                )
            }
        };
        if ok {
            passed += 1;
            continue;
        }
        if let Ok(key) = serde_json::to_string(&case["args"])
            && waived_set.contains(&key)
        {
            passed += 1;
            waived += 1;
            continue;
        }
        failed += 1;
        if failures.len() < 8 {
            let args: Vec<String> = case["args"]
                .as_array()
                .map(|a| a.iter().map(|v| show(v, 80)).collect())
                .unwrap_or_default();
            failures.push(Failure {
                case: i,
                call: format!(
                    "{}{}({})",
                    if is_edge {
                        "(edge) "
                    } else if is_derived {
                        "(derived) "
                    } else {
                        ""
                    },
                    job.func,
                    args.join(", ")
                ),
                expected,
                got: match got {
                    Ok(v) => show(&v, 160),
                    Err(e) => format!("error: {}", show(&Value::String(e), 200)),
                },
            });
        }
    }

    let mut v = Verify {
        total: cases.len(),
        derived,
        edges,
        passed,
        failed,
        waived,
        failures,
        ..Verify::default()
    };
    let original_ns = fs::read_to_string(&job.meta)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|m| m["original_ns"].as_f64());
    if let Some(original_ns) = original_ns.filter(|n| *n > 0.0 && failed == 0 && !inputs.is_empty())
    {
        let (mut best, mut spent, mut passes) = (f64::INFINITY, 0.0, 0);
        while passes < 3 || (spent < job.budget && passes < 2000) {
            let t = Instant::now();
            for input in &inputs {
                let _ = std::hint::black_box(invoke(input));
            }
            let took = t.elapsed().as_secs_f64();
            best = best.min(took);
            spent += took;
            passes += 1;
        }
        let json_total_ns = best / inputs.len() as f64 * 1e9;
        // Measure JSON parse + reply serialization overhead to separate compute vs FFI marshal time.
        let mut best_json = f64::INFINITY;
        for _ in 0..3 {
            let t = Instant::now();
            for (input, out) in inputs.iter().zip(outputs.iter()) {
                let parsed: Vec<Value> = serde_json::from_slice(input).unwrap_or_default();
                let reply = serde_json::to_vec(&json!({ "ok": out })).unwrap_or_default();
                std::hint::black_box((parsed, reply));
            }
            best_json = best_json.min(t.elapsed().as_secs_f64());
        }
        let json_marshal_ns = best_json / inputs.len() as f64 * 1e9;

        let mut bin_total_ns = None;
        let mut bin_marshal_ns = None;
        if let Some(ref fa_bin) = bin_sym {
            let bin_inputs: Vec<Vec<u8>> = inputs
                .iter()
                .filter_map(|inp| serde_json::from_slice::<Value>(inp).ok())
                .map(|v| {
                    let mut buf = Vec::with_capacity(128);
                    bin_enc_val(&v, &mut buf);
                    buf
                })
                .collect();
            if bin_inputs.len() == inputs.len() {
                // Parity sanity check on recorded inputs so wire v1 regressions fail loudly.
                for (bin_in, want) in bin_inputs.iter().zip(outputs.iter()).take(4) {
                    let mut n = 0usize;
                    let bytes = unsafe {
                        let p = fa_bin(bin_in.as_ptr(), bin_in.len(), &mut n);
                        let b = std::slice::from_raw_parts(p, n).to_vec();
                        free(p, n);
                        b
                    };
                    if let Ok(got) = bin_dec_reply(&bytes) {
                        ensure!(
                            same_with_tol(want, &got, job.loose_empty, job.float_tolerance),
                            "fa_bin parity mismatch vs fa_call: expected {}, got {}",
                            show(want, 120),
                            show(&got, 120)
                        );
                    }
                }
                let (mut best_b, mut spent_b, mut passes_b) = (f64::INFINITY, 0.0, 0);
                while passes_b < 3 || (spent_b < job.budget * 0.5 && passes_b < 2000) {
                    let t = Instant::now();
                    for bin_in in &bin_inputs {
                        let mut n = 0usize;
                        let bytes = unsafe {
                            let p = fa_bin(bin_in.as_ptr(), bin_in.len(), &mut n);
                            let b = std::slice::from_raw_parts(p, n).to_vec();
                            free(p, n);
                            b
                        };
                        let _ = std::hint::black_box(bin_dec_reply(&bytes));
                    }
                    let took = t.elapsed().as_secs_f64();
                    best_b = best_b.min(took);
                    spent_b += took;
                    passes_b += 1;
                }
                bin_total_ns = Some(best_b / inputs.len() as f64 * 1e9);

                let bin_replies: Vec<Vec<u8>> = outputs
                    .iter()
                    .map(|out| {
                        let mut buf = Vec::with_capacity(64);
                        buf.push(1);
                        bin_enc_val(out, &mut buf);
                        buf
                    })
                    .collect();
                let mut best_bm = f64::INFINITY;
                for _ in 0..3 {
                    let t = Instant::now();
                    for (bin_in, bin_rep) in bin_inputs.iter().zip(bin_replies.iter()) {
                        let mut p = 0usize;
                        let dec_in = bin_dec_val(bin_in, &mut p);
                        let dec_rep = bin_dec_reply(bin_rep);
                        let _ = std::hint::black_box((dec_in.ok(), dec_rep.ok()));
                    }
                    best_bm = best_bm.min(t.elapsed().as_secs_f64());
                }
                bin_marshal_ns = Some(best_bm / inputs.len() as f64 * 1e9);
            }
        }

        let (rust_ns, ffi_ns, binary_wire) = if has_direct {
            (
                (json_total_ns - json_marshal_ns).max(10.0),
                json_marshal_ns,
                None,
            )
        } else if let (Some(bt), Some(bm)) = (bin_total_ns, bin_marshal_ns) {
            (bt, bm, Some(true))
        } else {
            (json_total_ns, json_marshal_ns, Some(false))
        };
        v.micro = Some(Micro {
            original_ns,
            rust_ns,
            speedup: if rust_ns > 0.0 {
                original_ns / rust_ns
            } else {
                0.0
            },
            ffi_ns: Some(ffi_ns),
            direct_abi: Some(has_direct),
            binary_wire,
        });
    }
    fs::write(&job.out, serde_json::to_vec(&v)?)?;
    Ok(())
}

/// Same value, give or take float rounding.
pub fn same(a: &Value, b: &Value, loose_empty: bool) -> bool {
    same_with_tol(a, b, loose_empty, None)
}

/// Same value, with optional explicit relative/absolute float tolerance.
pub fn same_with_tol(a: &Value, b: &Value, loose_empty: bool, tol: Option<f64>) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if x.is_f64() || y.is_f64() || tol.is_some() {
                let (x, y) = (
                    x.as_f64().unwrap_or(f64::NAN),
                    y.as_f64().unwrap_or(f64::NAN),
                );
                let rel = tol.unwrap_or(1e-9);
                let abs = tol.unwrap_or(1e-12);
                x == y || (x - y).abs() <= abs.max(rel * x.abs().max(y.abs()))
            } else {
                x == y
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|(p, q)| same_with_tol(p, q, loose_empty, tol))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter().all(|(k, p)| {
                    y.get(k)
                        .is_some_and(|q| same_with_tol(p, q, loose_empty, tol))
                })
        }
        (Value::Null, Value::Array(v)) | (Value::Array(v), Value::Null) => {
            loose_empty && v.is_empty()
        }
        (Value::Null, Value::Object(v)) | (Value::Object(v), Value::Null) => {
            loose_empty && v.is_empty()
        }
        (Value::Array(a), Value::Object(o)) | (Value::Object(o), Value::Array(a)) => {
            loose_empty && a.is_empty() && o.is_empty()
        }
        _ => a == b,
    }
}

// ------------------------------------------------------------------ temporary edits

/// Files changed (or created) for one run. Everything goes back on restore() or drop, and a copy
/// sits in .fast/backup until then, so even a killed run is undone by the next one.
pub struct Patches {
    dir: PathBuf,
    files: Vec<(PathBuf, Option<String>)>,
}

impl Patches {
    pub fn new(ctx: &Ctx) -> Result<Patches> {
        let dir = ctx.fast.join("backup");
        restore_leftovers(&dir)?;
        fs::create_dir_all(&dir)?;
        Ok(Patches {
            dir,
            files: Vec::new(),
        })
    }

    fn track(&mut self, path: &Path) -> Result<()> {
        if self.files.iter().any(|(p, _)| p == path) {
            return Ok(());
        }
        let original = if path.exists() {
            Some(fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?)
        } else {
            None
        };
        let n = self.files.len();
        fs::write(
            self.dir.join(format!("{n}.path")),
            path.to_string_lossy().as_bytes(),
        )?;
        if let Some(text) = &original {
            fs::write(self.dir.join(format!("{n}.orig")), text)?;
        }
        self.files.push((path.to_path_buf(), original));
        Ok(())
    }

    pub fn write(&mut self, path: &Path, text: &str) -> Result<()> {
        self.track(path)?;
        fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }

    pub fn restore(&mut self) -> Result<()> {
        let mut first_err = None;
        for (path, original) in self.files.drain(..).rev() {
            let r = match original {
                Some(text) => fs::write(&path, text),
                None => fs::remove_file(&path).or_else(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(e)
                    }
                }),
            };
            if let Err(e) = r {
                first_err.get_or_insert(anyhow::anyhow!(
                    "restoring {}: {e} (a copy is in {})",
                    path.display(),
                    self.dir.display()
                ));
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        let _ = fs::remove_dir_all(&self.dir);
        Ok(())
    }
}

impl Drop for Patches {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn restore_leftovers(dir: &Path) -> Result<()> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "path") {
            let target = PathBuf::from(fs::read_to_string(&p)?);
            let orig = p.with_extension("orig");
            if orig.exists() {
                fs::copy(&orig, &target)
                    .with_context(|| format!("restoring {}", target.display()))?;
            } else {
                let _ = fs::remove_file(&target);
            }
            crate::ui::warn(format!(
                "restored {} from an interrupted run",
                target.display()
            ));
        }
    }
    let _ = fs::remove_dir_all(dir);
    Ok(())
}

// ------------------------------------------------------------------ blocks inside a function body

fn code_part(line: &str) -> &str {
    let line = line.split("//").next().unwrap_or(line);
    line.trim_end()
}

/// 0-based index of the line whose last code character is the { that opens the function's body.
/// The function is the header line nearest to hint (1-based) that is_header accepts.
pub fn body_line(text: &str, hint: u32, is_header: impl Fn(&str) -> bool) -> Option<usize> {
    let lines: Vec<&str> = text.lines().collect();
    let header = (0..lines.len())
        .filter(|&i| is_header(lines[i]))
        .min_by_key(|&i| (i as i64 + 1 - hint as i64).abs())?;
    for (j, line) in lines.iter().enumerate().skip(header).take(40) {
        let code = code_part(line);
        if code.ends_with('{') {
            return Some(j);
        }
        if code.ends_with(';') || (j > header && code.ends_with('}')) {
            return None;
        }
    }
    None
}

/// Insert (or replace) the block for id right after the body's opening line, indented like the body.
pub fn insert_in_body(
    text: &str,
    comment: &str,
    id: &str,
    body: &str,
    find: impl Fn(&str) -> Option<usize>,
) -> Result<String> {
    let text = strip_in_body(text, comment, id);
    let Some(at) = find(&text) else {
        bail!("couldn't find where the function's body starts (its {{ must end a line)")
    };
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let indent_of = |l: &str| {
        l.chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect::<String>()
    };
    let open = indent_of(lines[at]);
    let next = lines[at + 1..]
        .iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| indent_of(l))
        .unwrap_or_default();
    let indent = if next.len() > open.len() {
        next
    } else {
        format!("{open}    ")
    };
    let (begin, end) = backend::markers(comment, id);
    let mut block = format!("{indent}{begin}{nl}");
    for line in body.lines() {
        block.push_str(&format!("{indent}{line}{nl}"));
    }
    block.push_str(&format!("{indent}{end}{nl}"));
    let offset: usize = lines[..=at].iter().map(|l| l.len()).sum();
    let mut head = text[..offset].to_string();
    if !head.ends_with('\n') {
        head.push_str(nl);
    }
    Ok(format!("{head}{block}{}", &text[offset..]))
}

/// Remove the block for id (either `fnr` or legacy `fast-anything`); the rest comes back byte for byte.
pub fn strip_in_body(text: &str, comment: &str, id: &str) -> String {
    let strip_pair = |src: &str, begin: &str, end: &str| -> String {
        let Some(b) = src.find(begin) else {
            return src.to_string();
        };
        let Some(e) = src[b..].find(end).map(|e| b + e + end.len()) else {
            return src.to_string();
        };
        let start = src[..b].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let stop = src[e..].find('\n').map(|p| e + p + 1).unwrap_or(src.len());
        format!("{}{}", &src[..start], &src[stop..])
    };
    let (begin, end) = backend::markers(comment, id);
    let out = strip_pair(text, &begin, &end);
    let (l_begin, l_end) = backend::legacy_markers(comment, id);
    strip_pair(&out, &l_begin, &l_end)
}

/// A file next to the original, named so it sorts last and is easy to spot.
pub fn sibling(file: &Path, name: &str) -> Result<PathBuf> {
    Ok(file.parent().context("file has no folder")?.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_blocks_come_out_byte_for_byte() {
        let header = |l: &str| l.contains("func lev(");
        for original in [
            "package main\n\nfunc lev(a, b string) int {\n\treturn 1\n}\n",
            "package main\r\n\r\nfunc lev(a, b string) int { // the distance\r\n\treturn 1\r\n}\r\n",
            "x\nfunc lev(a string,\n    b string) int\n{\n    return 1;\n}\n",
        ] {
            let find = |t: &str| body_line(t, 3, header);
            let patched = insert_in_body(
                original,
                "//",
                "main.lev",
                "if fast() {\n\treturn 2\n}",
                find,
            )
            .unwrap();
            assert!(patched.contains("// >>> fnr: main.lev >>>"));
            assert_eq!(
                insert_in_body(
                    &patched,
                    "//",
                    "main.lev",
                    "if fast() {\n\treturn 2\n}",
                    find
                )
                .unwrap(),
                patched
            );
            assert_eq!(strip_in_body(&patched, "//", "main.lev"), original);
            let legacy = patched.replace("fnr:", "fast-anything:");
            assert_eq!(strip_in_body(&legacy, "//", "main.lev"), original);
        }
        assert!(body_line("abstract int lev(int a);\n", 1, |l| l.contains("lev(")).is_none());
    }

    #[test]
    fn values_compare_like_the_languages_do() {
        let j = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        assert!(same(&j("[1, 2.0]"), &j("[1, 2.0000000000001]"), false));
        assert!(!same(&j("[1]"), &j("[2]"), false));
        assert!(same(&j("null"), &j("[]"), true) && !same(&j("null"), &j("[]"), false));
        assert!(same(&j("{\"a\": [1]}"), &j("{\"a\": [1]}"), false));
        assert!(same(&j("3"), &j("3.0"), false));
        assert!(same_with_tol(&j("1.0"), &j("1.005"), false, Some(0.01)));
        assert!(!same_with_tol(&j("1.0"), &j("1.05"), false, Some(0.01)));
    }

    #[test]
    fn direct_sig_and_parity_harness_generate_clean_rust() {
        use crate::hotspot::Param;
        let hs = Hotspot {
            id: "mandelbrot.c:mandelbrot".into(),
            file: "mandelbrot.c".into(),
            line: 1,
            end_line: 10,
            name: "mandelbrot".into(),
            qualname: "mandelbrot".into(),
            params: vec![
                Param {
                    name: "width".into(),
                    annotation: Some("int".into()),
                    default: None,
                    kwonly: false,
                },
                Param {
                    name: "ok".into(),
                    annotation: Some("bool".into()),
                    default: None,
                    kwonly: false,
                },
            ],
            returns: Some("long long".into()),
            ..Hotspot::default()
        };
        let port = Port {
            id: hs.id.clone(),
            file: hs.file.clone(),
            module: "mandelbrot".into(),
            func: hs.name.clone(),
            ext: "fa_mandelbrot".into(),
            crate_dir: ".fast/ports/mandelbrot".into(),
            recorded: 8,
            calls_seen: 8,
            verify: None,
            enabled: false,
            lang: Lang::C,
            owner: None,
            line: 1,
            sources: vec![],
            build_profile: None,
        };
        let flavor = Flavor {
            lang: Lang::C,
            fence: "c",
            rust_type: crate::cfamily::rust_type_c,
            default_json: |_| None,
            rules: &[],
        };
        let sig = DirectSig::from_hotspot(&hs, &flavor).unwrap();
        assert!(sig.is_all_scalar());
        assert_eq!(sig.ret, CAbiScalar::I64);
        let lib = glue_rs_with_flavor(&hs, &port, &flavor);
        assert!(lib.contains("pub mod port;"));
        assert!(lib.contains("pub unsafe extern \"C\" fn fa_direct("));
        assert!(lib.contains("pub unsafe extern \"C\" fn fa_bin("));
        assert!(lib.contains("ok: u8"));
        assert!(BinSig::supports_rust_type("Vec<(i32, i32)>"));
        assert!(BinSig::supports_rust_type(
            "std::collections::HashMap<String, Vec<f64>>"
        ));
        let parity = recorded_parity_test(&port, false);
        assert!(parity.contains("use fa_mandelbrot::{fa_call, fa_free};"));
        assert!(parity.contains("fn recorded_calls_match_parity()"));
    }
}
