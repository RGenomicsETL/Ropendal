use std::ffi::{CString, c_int, c_void};
use std::ptr;
use std::sync::{Once, OnceLock};

use ::bytes::Bytes;
use opendal::Buffer;
use savvy::ffi::{DllInfo, SEXP};
use savvy::{IntoExtPtrSexp, RawSexp, Sexp, savvy, savvy_init};
use savvy_ffi::altrep::{
    ALTREP, MARK_NOT_MUTABLE, R_altrep_data1, R_altrep_data2, R_altrep_inherits,
    R_make_altraw_class, R_new_altrep, R_set_altraw_Elt_method, R_set_altraw_Get_region_method,
    R_set_altrep_Coerce_method, R_set_altrep_Duplicate_method, R_set_altrep_Inspect_method,
    R_set_altrep_Length_method, R_set_altrep_data2, R_set_altvec_Dataptr_method,
    R_set_altvec_Dataptr_or_null_method,
};
use savvy_ffi::{
    R_ExternalPtrAddr, R_xlen_t, RAW, RAW_ELT, RAWSXP, Rboolean, Rboolean_TRUE, Rf_allocVector,
    Rf_coerceVector, Rf_duplicate, Rf_xlength, SEXPTYPE,
};

use crate::r_values::{bool_scalar, buffer_to_raw_sexp, real_scalar};

unsafe extern "C" {
    static mut R_NilValue: SEXP;

    fn R_MakeExternalPtr(p: *mut c_void, tag: SEXP, prot: SEXP) -> SEXP;
    fn R_PreserveObject(x: SEXP);
    fn R_ExternalPtrTag(s: SEXP) -> SEXP;
    fn R_SetExternalPtrTag(s: SEXP, tag: SEXP);
    fn Rf_protect(x: SEXP) -> SEXP;
    fn Rf_unprotect(n: c_int);
}

struct LocalProtect;

impl Drop for LocalProtect {
    fn drop(&mut self) {
        unsafe { Rf_unprotect(1) };
    }
}

fn local_protect(value: &Sexp) -> LocalProtect {
    unsafe { Rf_protect(value.0) };
    LocalProtect
}

static OPENDAL_BYTES_TAG_INIT: Once = Once::new();
static mut OPENDAL_BYTES_TAG: SEXP = std::ptr::null_mut();
static OPENDAL_BYTES_TAG_SENTINEL: u8 = 0;

fn opendal_bytes_tag() -> SEXP {
    OPENDAL_BYTES_TAG_INIT.call_once(|| unsafe {
        let tag = R_MakeExternalPtr(
            (&OPENDAL_BYTES_TAG_SENTINEL as *const u8)
                .cast_mut()
                .cast::<c_void>(),
            R_NilValue,
            R_NilValue,
        );
        let tag_sexp = Sexp(tag);
        let _tag_guard = local_protect(&tag_sexp);
        R_PreserveObject(tag);
        OPENDAL_BYTES_TAG = tag;
    });

    unsafe { OPENDAL_BYTES_TAG }
}

fn tag_opendal_bytes_ptr(value: &Sexp) {
    unsafe { R_SetExternalPtrTag(value.0, opendal_bytes_tag()) };
}

fn is_tagged_opendal_bytes_ptr(value: &Sexp) -> bool {
    unsafe { R_ExternalPtrTag(value.0) == opendal_bytes_tag() }
}

/// Immutable Rust-owned byte buffer.
///
/// @export
#[savvy]
pub struct OpendalBytes {
    buffer: Buffer,
}

impl OpendalBytes {
    pub(crate) fn new(buffer: Buffer) -> Self {
        Self { buffer }
    }

    pub(crate) fn buffer(&self) -> Buffer {
        self.buffer.clone()
    }
}

struct OpendalBytesAltRaw {
    bytes: Bytes,
}

impl IntoExtPtrSexp for OpendalBytesAltRaw {}

static OPENDAL_BYTES_ALTREP_CLASS: OnceLock<savvy_ffi::altrep::R_altrep_class_t> = OnceLock::new();

#[savvy_init]
fn init_opendal_bytes_altrep(dll_info: *mut DllInfo) -> savvy::Result<()> {
    let class_name = CString::new("opendal_bytes_raw").expect("static class name has no NUL");
    let package_name = CString::new("Ropendal").expect("static package name has no NUL");
    let class_t =
        unsafe { R_make_altraw_class(class_name.as_ptr(), package_name.as_ptr(), dll_info) };

    unsafe {
        R_set_altrep_Length_method(class_t, Some(opendal_altrep_length));
        R_set_altrep_Inspect_method(class_t, Some(opendal_altrep_inspect));
        R_set_altrep_Duplicate_method(class_t, Some(opendal_altrep_duplicate));
        R_set_altrep_Coerce_method(class_t, Some(opendal_altrep_coerce));
        R_set_altvec_Dataptr_method(class_t, Some(opendal_altvec_dataptr));
        R_set_altvec_Dataptr_or_null_method(class_t, Some(opendal_altvec_dataptr_or_null));
        R_set_altraw_Elt_method(class_t, Some(opendal_altraw_elt));
        R_set_altraw_Get_region_method(class_t, Some(opendal_altraw_get_region));
    }

    OPENDAL_BYTES_ALTREP_CLASS
        .set(class_t)
        .map_err(|_| savvy::Error::new("OpendalBytes ALTREP class was already initialized"))?;
    Ok(())
}

unsafe fn opendal_altrep_body_mut(x: SEXP) -> Option<&'static mut OpendalBytesAltRaw> {
    let data1 = unsafe { R_altrep_data1(x) };
    let ptr = unsafe { R_ExternalPtrAddr(data1) } as *mut OpendalBytesAltRaw;
    unsafe { ptr.as_mut() }
}

unsafe fn opendal_altrep_materialized(x: SEXP, allow_allocate: bool) -> Option<SEXP> {
    let data2 = unsafe { R_altrep_data2(x) };
    if unsafe { data2 != R_NilValue } {
        return Some(data2);
    }
    if !allow_allocate {
        return None;
    }
    let body = unsafe { opendal_altrep_body_mut(x) }?;
    let len = body.bytes.len();
    let out = unsafe { Rf_allocVector(RAWSXP, len as R_xlen_t) };
    unsafe { Rf_protect(out) };
    if len > 0 {
        unsafe { ptr::copy_nonoverlapping(body.bytes.as_ptr(), RAW(out), len) };
    }
    unsafe { R_set_altrep_data2(x, out) };
    unsafe { Rf_unprotect(1) };
    Some(out)
}

unsafe extern "C" fn opendal_altrep_length(x: SEXP) -> R_xlen_t {
    if let Some(materialized) = unsafe { opendal_altrep_materialized(x, false) } {
        return unsafe { Rf_xlength(materialized) };
    }
    unsafe { opendal_altrep_body_mut(x) }
        .map(|body| body.bytes.len() as R_xlen_t)
        .unwrap_or(0)
}

unsafe extern "C" fn opendal_altrep_inspect(
    x: SEXP,
    _: c_int,
    _: c_int,
    _: c_int,
    _: Option<unsafe extern "C" fn(SEXP, c_int, c_int, c_int)>,
) -> Rboolean {
    let materialized = unsafe { R_altrep_data2(x) != R_NilValue };
    savvy::io::r_print(
        &format!("opendal_bytes_raw (materialized: {materialized})\n"),
        false,
    );
    Rboolean_TRUE
}

unsafe extern "C" fn opendal_altrep_duplicate(x: SEXP, _deep_copy: Rboolean) -> SEXP {
    let Some(materialized) = (unsafe { opendal_altrep_materialized(x, true) }) else {
        return unsafe { R_NilValue };
    };
    unsafe { Rf_duplicate(materialized) }
}

unsafe extern "C" fn opendal_altrep_coerce(x: SEXP, sexp_type: SEXPTYPE) -> SEXP {
    let Some(materialized) = (unsafe { opendal_altrep_materialized(x, true) }) else {
        return unsafe { R_NilValue };
    };
    unsafe { Rf_coerceVector(materialized, sexp_type) }
}

unsafe extern "C" fn opendal_altvec_dataptr(x: SEXP, _writable: Rboolean) -> *mut c_void {
    unsafe { opendal_altrep_materialized(x, true) }
        .map(|materialized| unsafe { RAW(materialized).cast::<c_void>() })
        .unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn opendal_altvec_dataptr_or_null(x: SEXP) -> *const c_void {
    unsafe { opendal_altrep_materialized(x, false) }
        .map(|materialized| unsafe { RAW(materialized).cast::<c_void>() as *const c_void })
        .unwrap_or(ptr::null())
}

unsafe extern "C" fn opendal_altraw_elt(x: SEXP, i: R_xlen_t) -> u8 {
    if let Some(materialized) = unsafe { opendal_altrep_materialized(x, false) } {
        return unsafe { RAW_ELT(materialized, i) };
    }
    let Some(body) = (unsafe { opendal_altrep_body_mut(x) }) else {
        return 0;
    };
    if i < 0 {
        return 0;
    }
    body.bytes.get(i as usize).copied().unwrap_or(0)
}

unsafe extern "C" fn opendal_altraw_get_region(
    x: SEXP,
    start: R_xlen_t,
    n: R_xlen_t,
    dst: *mut u8,
) -> R_xlen_t {
    if dst.is_null() || start < 0 || n <= 0 {
        return 0;
    }
    let start = start as usize;
    if let Some(materialized) = unsafe { opendal_altrep_materialized(x, false) } {
        let len = unsafe { Rf_xlength(materialized) } as usize;
        if start >= len {
            return 0;
        }
        let n = (n as usize).min(len - start);
        unsafe { ptr::copy_nonoverlapping(RAW(materialized).add(start), dst, n) };
        return n as R_xlen_t;
    }
    let Some(body) = (unsafe { opendal_altrep_body_mut(x) }) else {
        return 0;
    };
    let len = body.bytes.len();
    if start >= len {
        return 0;
    }
    let n = (n as usize).min(len - start);
    unsafe { ptr::copy_nonoverlapping(body.bytes.as_ptr().add(start), dst, n) };
    n as R_xlen_t
}

fn buffer_to_altrep_raw_sexp(buffer: Buffer) -> savvy::Result<savvy::Sexp> {
    let bytes = buffer.to_bytes();
    if bytes.len() > R_xlen_t::MAX as usize {
        return Err(savvy::Error::new(
            "byte buffer is too large for an R raw vector",
        ));
    }
    let class_t = *OPENDAL_BYTES_ALTREP_CLASS
        .get()
        .ok_or_else(|| savvy::Error::new("OpendalBytes ALTREP class is not initialized"))?;
    let data1 = OpendalBytesAltRaw { bytes }.into_external_pointer();
    let _data1_guard = local_protect(&data1);
    let out = unsafe { R_new_altrep(class_t, data1.0, R_NilValue) };
    unsafe { MARK_NOT_MUTABLE(out) };
    Ok(Sexp(out))
}

#[savvy]
fn opendal_bytes_len(bytes: Sexp) -> savvy::Result<savvy::Sexp> {
    let Some(buffer) = buffer_from_opendal_bytes_sexp(&bytes)? else {
        return Err(savvy::Error::new("expected OpendalBytes"));
    };
    real_scalar(buffer.len() as f64)?.into()
}

#[savvy]
fn opendal_bytes_as_raw(bytes: Sexp) -> savvy::Result<savvy::Sexp> {
    let Some(buffer) = buffer_from_opendal_bytes_sexp(&bytes)? else {
        return Err(savvy::Error::new("expected OpendalBytes"));
    };
    buffer_to_raw_sexp(buffer).map(|x| x.into())
}

#[savvy]
fn opendal_bytes_as_altrep_raw(bytes: Sexp) -> savvy::Result<savvy::Sexp> {
    let Some(buffer) = buffer_from_opendal_bytes_sexp(&bytes)? else {
        return Err(savvy::Error::new("expected OpendalBytes"));
    };
    buffer_to_altrep_raw_sexp(buffer)
}

#[savvy]
fn opendal_bytes_is_altrep_raw(x: Sexp) -> savvy::Result<savvy::Sexp> {
    let is_altrep = OPENDAL_BYTES_ALTREP_CLASS
        .get()
        .is_some_and(|class_t| unsafe {
            ALTREP(x.0) == 1 && R_altrep_inherits(x.0, *class_t) == Rboolean_TRUE
        });
    bool_scalar(is_altrep)?.into()
}

#[savvy]
fn opendal_bytes_from_raw(data: RawSexp) -> savvy::Result<savvy::Sexp> {
    opendal_bytes_to_sexp(Buffer::from(data.to_vec()))
}

#[savvy]
fn opendal_bytes_slice(bytes: Sexp, offset: f64, size: Option<f64>) -> savvy::Result<savvy::Sexp> {
    let Some(buffer) = buffer_from_opendal_bytes_sexp(&bytes)? else {
        return Err(savvy::Error::new("expected OpendalBytes"));
    };
    let len = buffer.len();
    let start = numeric_byte_count(offset, "offset")?;
    let requested = match size {
        Some(value) => Some(numeric_byte_count(value, "size")?),
        None => None,
    };
    if start >= len {
        return opendal_bytes_to_sexp(Buffer::new());
    }
    let end = match requested {
        Some(n) => start.saturating_add(n).min(len),
        None => len,
    };
    opendal_bytes_to_sexp(buffer.slice(start..end))
}

fn numeric_byte_count(value: f64, name: &str) -> savvy::Result<usize> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 {
        return Err(savvy::Error::new(format!(
            "{name} must be a non-negative whole number"
        )));
    }
    if value > usize::MAX as f64 {
        return Err(savvy::Error::new(format!(
            "{name} is too large for this platform"
        )));
    }
    Ok(value as usize)
}

pub(crate) fn opendal_bytes_to_sexp(buffer: Buffer) -> savvy::Result<savvy::Sexp> {
    let mut out = <savvy::Sexp>::try_from(OpendalBytes::new(buffer))?;
    let _out_guard = local_protect(&out);
    tag_opendal_bytes_ptr(&out);
    out.set_class([
        "Ropendal::OpendalBytes",
        "OpendalBytes",
        "savvy_Ropendal__sealed",
    ])?;
    Ok(out)
}

pub(crate) fn buffer_from_opendal_bytes_sexp(value: &Sexp) -> savvy::Result<Option<Buffer>> {
    let Some(classes) = value.get_class() else {
        return Ok(None);
    };
    if !classes.iter().any(|class| *class == "OpendalBytes") {
        return Ok(None);
    }

    let ptr_value = if value.is_environment() {
        let env = savvy::EnvironmentSexp(value.0);
        let Some(ptr) = env.get(".ptr")? else {
            return Err(savvy::Error::new("OpendalBytes object is missing .ptr"));
        };
        ptr
    } else {
        Sexp(value.0)
    };

    ptr_value.assert_external_pointer()?;
    if !is_tagged_opendal_bytes_ptr(&ptr_value) {
        return Err(savvy::Error::new("invalid OpendalBytes pointer"));
    }

    let ptr = unsafe { savvy::get_external_pointer_addr(ptr_value.0)? as *const OpendalBytes };
    let Some(bytes) = (unsafe { ptr.as_ref() }) else {
        return Err(savvy::Error::new("invalid OpendalBytes pointer"));
    };
    Ok(Some(bytes.buffer()))
}
