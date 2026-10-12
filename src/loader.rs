//! Minimal hand-written dlopen wrapper: since glibc >= 2.34 dlopen/dlsym are
//! part of libc, so no -ldl is needed. Lib deliberately has no Drop — the
//! function pointers stay valid for the process lifetime, avoiding the whole
//! class of dangling-pointer problems after dlclose (bench processes are
//! short-lived; the leak is inconsequential and safe).

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};

extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *mut c_char;
}

const RTLD_NOW: c_int = 0x2;
const RTLD_GLOBAL: c_int = 0x100;

/// An open dlopen handle. Deliberately no `Drop`: see the module docs.
pub struct Lib {
    _handle: *mut c_void,
}

impl Lib {
    /// `global=true` uses RTLD_GLOBAL: when preloading the dependency chain
    /// (libwd -> libkaelz4), later dlopens resolve DT_NEEDED against
    /// already-loaded objects by SONAME, working around missing rpaths.
    pub fn open(path: &str, global: bool) -> Result<Lib, String> {
        let c = CString::new(path).map_err(|e| format!("{path}: {e}"))?;
        unsafe {
            let _ = dlerror(); // clear any stale error
            let flags = RTLD_NOW | if global { RTLD_GLOBAL } else { 0 };
            let h = dlopen(c.as_ptr(), flags);
            if h.is_null() {
                let e = dlerror();
                let msg = if e.is_null() {
                    "unknown dlopen error".to_string()
                } else {
                    CStr::from_ptr(e).to_string_lossy().into_owned()
                };
                // keep it single-line so the RESULT/VERIFY line protocol stays intact
                return Err(msg.replace(['\n', '\r'], " "));
            }
            Ok(Lib { _handle: h })
        }
    }

    /// Whether the library exports `name` (dlsym non-null; dlerror is
    /// treated as absence).
    pub fn has_symbol(&self, name: &str) -> bool {
        self.raw_sym(name).is_ok()
    }

    fn raw_sym(&self, name: &str) -> Result<*mut c_void, String> {
        let c = CString::new(name).map_err(|e| format!("{name}: {e}"))?;
        unsafe {
            let _ = dlerror();
            let p = dlsym(self._handle, c.as_ptr());
            if p.is_null() {
                let e = dlerror();
                if !e.is_null() {
                    let msg = CStr::from_ptr(e).to_string_lossy().into_owned();
                    return Err(msg.replace(['\n', '\r'], " "));
                }
                return Err(format!("{name}: symbol not found"));
            }
            Ok(p)
        }
    }

    /// Resolve a symbol as a function pointer of type T. T must be a bare
    /// fn pointer (same width as *mut c_void).
    ///
    /// # Safety
    /// The caller guarantees that name's C signature matches T.
    pub unsafe fn sym_fn<T>(&self, name: &str) -> Result<T, String> {
        let p = self.raw_sym(name)?;
        Ok(std::mem::transmute_copy(&p))
    }

    /// Preload dependencies in order (RTLD_GLOBAL); an individual failure is
    /// logged but not fatal — the main library may carry its own rpath and
    /// open fine without preloading. Returns a description of the entries
    /// that actually loaded.
    pub fn preload_all(paths: &[String]) -> String {
        let mut loaded = Vec::new();
        for p in paths {
            match Lib::open(p, true) {
                Ok(_) => loaded.push(p.clone()),
                Err(e) => eprintln!("preload {p} failed: {e}"),
            }
        }
        loaded.join(",")
    }
}
