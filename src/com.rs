//! Minimal support for implementing Slang COM interfaces in Rust.

use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{NonNull, null_mut};
use std::sync::atomic::{AtomicU32, Ordering, fence};

use crate::{Blob, FileSystemImpl, IUnknown, Interface, UUID, sys, uuid};

const SLANG_OK: sys::SlangResult = 0;
const SLANG_FAIL: sys::SlangResult = 0x80004005_u32 as i32;
const SLANG_E_NO_INTERFACE: sys::SlangResult = 0x80004002_u32 as i32;
const SLANG_E_NOT_FOUND: sys::SlangResult = 0x82000005_u32 as i32;

const ICASTABLE_IID: UUID = uuid(0x87ede0e1_4852_44b0_8bf2cb31874de239);

/// A Rust type exposed to Slang as a reference counted COM object.
///
/// # Safety
/// `VTABLE` must be a valid vtable for every interface in `IIDS`, and its
/// `ISlangUnknown` part must come from `unknown_vtable::<Self>()`.
pub(crate) unsafe trait Object: Sized + 'static {
	type Vtable: 'static;
	const VTABLE: &'static Self::Vtable;
	const IIDS: &'static [UUID];
}

#[repr(C)]
struct Header<T: Object> {
	vtable: &'static T::Vtable,
	ref_count: AtomicU32,
	value: T,
}

/// Allocates `value` as a COM object with a reference count of one.
pub(crate) fn new_object<T: Object>(value: T) -> IUnknown {
	let object = Box::new(Header {
		vtable: T::VTABLE,
		ref_count: AtomicU32::new(1),
		value,
	});
	IUnknown(NonNull::new(Box::into_raw(object) as *mut c_void).unwrap())
}

unsafe fn header<'a, T: Object>(this: *mut c_void) -> &'a Header<T> {
	unsafe { &*(this as *const Header<T>) }
}

fn uuid_eq(a: &UUID, b: &UUID) -> bool {
	a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

fn supports<T: Object>(guid: &UUID) -> bool {
	uuid_eq(guid, &IUnknown::IID) || T::IIDS.iter().any(|iid| uuid_eq(guid, iid))
}

unsafe extern "C" fn query_interface<T: Object>(this: *mut sys::ISlangUnknown, guid: *const UUID, out_object: *mut *mut c_void) -> sys::SlangResult {
	unsafe {
		if supports::<T>(&*guid) {
			add_ref::<T>(this);
			*out_object = this as *mut c_void;
			SLANG_OK
		} else {
			*out_object = null_mut();
			SLANG_E_NO_INTERFACE
		}
	}
}

unsafe extern "C" fn add_ref<T: Object>(this: *mut sys::ISlangUnknown) -> u32 {
	unsafe { header::<T>(this as _).ref_count.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "C" fn release<T: Object>(this: *mut sys::ISlangUnknown) -> u32 {
	unsafe {
		let count = header::<T>(this as _).ref_count.fetch_sub(1, Ordering::Release) - 1;
		if count == 0 {
			fence(Ordering::Acquire);
			drop(Box::from_raw(this as *mut Header<T>));
		}
		count
	}
}

unsafe extern "C" fn cast_as<T: Object>(this: *mut c_void, guid: *const UUID) -> *mut c_void {
	// castAs does not add a reference.
	if unsafe { supports::<T>(&*guid) } { this } else { null_mut() }
}

const fn unknown_vtable<T: Object>() -> sys::ISlangUnknown__bindgen_vtable {
	sys::ISlangUnknown__bindgen_vtable {
		ISlangUnknown_queryInterface: query_interface::<T>,
		ISlangUnknown_addRef: add_ref::<T>,
		ISlangUnknown_release: release::<T>,
	}
}

const fn castable_vtable<T: Object>() -> sys::ICastableVtable {
	sys::ICastableVtable {
		_base: unknown_vtable::<T>(),
		castAs: cast_as::<T>,
	}
}

/// Backing storage for blobs created from Rust data.
pub(crate) struct BlobData(pub(crate) Box<[u8]>);

unsafe impl Object for BlobData {
	type Vtable = sys::IBlobVtable;
	const VTABLE: &'static sys::IBlobVtable = &sys::IBlobVtable {
		_base: unknown_vtable::<Self>(),
		getBufferPointer: blob_buffer_pointer,
		getBufferSize: blob_buffer_size,
	};
	const IIDS: &'static [UUID] = &[Blob::IID];
}

unsafe extern "C" fn blob_buffer_pointer(this: *mut c_void) -> *const c_void {
	unsafe { header::<BlobData>(this).value.0.as_ptr() as *const c_void }
}

unsafe extern "C" fn blob_buffer_size(this: *mut c_void) -> usize {
	unsafe { header::<BlobData>(this).value.0.len() }
}

/// Exposes a Rust [`FileSystemImpl`] to Slang as an `ISlangFileSystem`.
pub(crate) struct RustFileSystem<T: FileSystemImpl>(pub(crate) RefCell<T>);

unsafe impl<T: FileSystemImpl> Object for RustFileSystem<T> {
	type Vtable = sys::IFileSystemVtable;
	const VTABLE: &'static sys::IFileSystemVtable = &sys::IFileSystemVtable {
		_base: castable_vtable::<Self>(),
		loadFile: file_system_load_file::<T>,
	};
	const IIDS: &'static [UUID] = &[ICASTABLE_IID, crate::FileSystem::IID];
}

unsafe extern "C" fn file_system_load_file<T: FileSystemImpl>(this: *mut c_void, path: *const c_char, out_blob: *mut *mut sys::ISlangBlob) -> sys::SlangResult {
	unsafe {
		*out_blob = null_mut();

		let Ok(path) = CStr::from_ptr(path).to_str() else {
			return SLANG_E_NOT_FOUND;
		};

		// Fails instead of panicking if Slang re-enters loadFile from within load_file.
		let Ok(mut file_system) = header::<RustFileSystem<T>>(this).value.0.try_borrow_mut() else {
			return SLANG_FAIL;
		};

		// Unwinding across the FFI boundary would abort, so report panics as failures.
		match catch_unwind(AssertUnwindSafe(|| file_system.load_file(path))) {
			Ok(Some(blob)) => {
				// Ownership of our reference is transferred to the caller.
				*out_blob = blob.as_raw();
				std::mem::forget(blob);
				SLANG_OK
			}
			Ok(None) => SLANG_E_NOT_FOUND,
			Err(_) => SLANG_FAIL,
		}
	}
}
