//! Lecture des décalages de fréquence (overclocking) des cartes NVIDIA par la
//! bibliothèque NVML du pilote, chargée à l'exécution : l'app démarre même sans
//! pilote NVIDIA. La lecture ne demande aucun droit ; l'écriture passe par
//! pkexec et un petit script Python (voir `OC_PY` dans main.rs).

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::OnceLock;

extern "C" {
    fn dlopen(file: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

const RTLD_NOW: c_int = 2;

type Init = unsafe extern "C" fn() -> c_int;
type ByUuid = unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> c_int;
type GetOffset = unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int;
type GetRange = unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_int;
type ErrStr = unsafe extern "C" fn(c_int) -> *const c_char;

struct Lib { by_uuid: ByUuid, core: GetOffset, mem: GetOffset, core_range: GetRange, mem_range: GetRange, err: ErrStr }

// Les pointeurs de fonctions de NVML sont utilisables depuis n'importe quel fil.
unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

fn lib() -> Result<&'static Lib, String> {
    static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(|| unsafe {
        let name = CString::new("libnvidia-ml.so.1").unwrap();
        let h = dlopen(name.as_ptr(), RTLD_NOW);
        if h.is_null() { return Err("Bibliothèque NVML du pilote NVIDIA introuvable.".into()); }
        let sym = |s: &str| {
            let c = CString::new(s).unwrap();
            let p = dlsym(h, c.as_ptr());
            if p.is_null() { Err(format!("Pilote NVIDIA trop ancien pour l'overclocking ({s} absent).")) } else { Ok(p) }
        };
        let init: Init = std::mem::transmute(sym("nvmlInit_v2")?);
        let lib = Lib {
            by_uuid: std::mem::transmute(sym("nvmlDeviceGetHandleByUUID")?),
            core: std::mem::transmute(sym("nvmlDeviceGetGpcClkVfOffset")?),
            mem: std::mem::transmute(sym("nvmlDeviceGetMemClkVfOffset")?),
            core_range: std::mem::transmute(sym("nvmlDeviceGetGpcClkMinMaxVfOffset")?),
            mem_range: std::mem::transmute(sym("nvmlDeviceGetMemClkMinMaxVfOffset")?),
            err: std::mem::transmute(sym("nvmlErrorString")?),
        };
        let r = init();
        if r != 0 { return Err(format!("NVML : {}", CStr::from_ptr((lib.err)(r)).to_string_lossy())); }
        Ok(lib)
    }).as_ref().map_err(Clone::clone)
}

/// Décalages actuels (MHz) et plages permises par le pilote.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Offsets { pub core: i32, pub mem: i32, pub core_range: (i32, i32), pub mem_range: (i32, i32) }

pub fn offsets(uuid: &str) -> Result<Offsets, String> {
    let l = lib()?;
    let uuid = CString::new(uuid).map_err(|_| "carte invalide".to_string())?;
    unsafe {
        let check = |r: c_int| if r == 0 { Ok(()) } else { Err(format!("NVML : {}", CStr::from_ptr((l.err)(r)).to_string_lossy())) };
        let mut h = std::ptr::null_mut();
        check((l.by_uuid)(uuid.as_ptr(), &mut h))?;
        let mut o = Offsets::default();
        check((l.core)(h, &mut o.core))?;
        check((l.mem)(h, &mut o.mem))?;
        check((l.core_range)(h, &mut o.core_range.0, &mut o.core_range.1))?;
        check((l.mem_range)(h, &mut o.mem_range.0, &mut o.mem_range.1))?;
        Ok(o)
    }
}
