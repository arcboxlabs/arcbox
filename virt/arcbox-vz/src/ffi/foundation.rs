//! Foundation framework helpers (`NSString`, NSURL, `NSArray`, etc.)

use objc2::runtime::AnyObject;
use std::ffi::c_void;

use super::runtime::get_class;
use crate::msg_send;

/// Creates an `NSString` from a Rust string.
pub fn nsstring(s: &str) -> *mut AnyObject {
    // SAFETY: NSString class is always available in the ObjC runtime. initWithBytes:length:encoding: is called with valid UTF-8 pointer/length from the Rust str, and encoding 4 (NSUTF8StringEncoding).
    // SAFETY: Receiver is a valid Objective-C object; selector and arguments match the method signature.
    unsafe {
        let cls = get_class("NSString").expect("NSString class not found");
        let alloc = msg_send!(cls, alloc);

        let sel = objc2::sel!(initWithBytes:length:encoding:);
        let func: unsafe extern "C" fn(
            *mut AnyObject,
            objc2::runtime::Sel,
            *const u8,
            usize,
            u64,
        ) -> *mut AnyObject = std::mem::transmute(super::runtime::objc_msgSend as *const c_void);
        func(alloc, sel, s.as_ptr(), s.len(), 4) // 4 = NSUTF8StringEncoding
    }
}

/// Gets `NSString` contents as a Rust String.
pub fn nsstring_to_string(obj: *mut AnyObject) -> String {
    if obj.is_null() {
        return String::new();
    }
    // SAFETY: obj is checked non-null above. UTF8String returns a null-terminated C string valid for the lifetime of the NSString.
    // SAFETY: Caller/context ensures the preconditions for this unsafe operation are met.
    unsafe {
        let sel_utf8 = objc2::sel!(UTF8String);
        let func: unsafe extern "C" fn(*const AnyObject, objc2::runtime::Sel) -> *const i8 =
            std::mem::transmute(super::runtime::objc_msgSend as *const c_void);
        let cstr = func(obj as *const AnyObject, sel_utf8);
        if cstr.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(cstr)
                .to_string_lossy()
                .into_owned()
        }
    }
}

/// Creates an NSURL from a file path.
///
/// Converts relative paths to absolute paths to ensure correct URL creation.
pub fn nsurl_file_path(path: &str) -> *mut AnyObject {
    // Convert relative paths to absolute paths
    let abs_path = if std::path::Path::new(path).is_absolute() {
        path.to_string()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path).to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string())
    };

    // SAFETY: NSURL and NSString classes are always available. fileURLWithPath: receives a valid NSString. retain prevents premature autorelease.
    // SAFETY: Receiver is a valid Objective-C object; selector and arguments match the method signature.
    unsafe {
        let cls = get_class("NSURL").expect("NSURL class not found");
        let path_str = nsstring(&abs_path);
        let url: *mut AnyObject = msg_send!(cls, fileURLWithPath: path_str);
        // Retain to prevent autorelease
        let _: *mut AnyObject = msg_send!(url, retain);
        url
    }
}

/// Creates an `NSArray` from raw pointers.
pub fn nsarray(objects: &[*mut AnyObject]) -> *mut AnyObject {
    // SAFETY: NSArray class is always available. arrayWithObjects:count: receives a valid pointer to the objects slice and its length.
    // SAFETY: Caller/context ensures the preconditions for this unsafe operation are met.
    unsafe {
        let cls = get_class("NSArray").expect("NSArray class not found");
        let sel = objc2::sel!(arrayWithObjects:count:);
        let func: unsafe extern "C" fn(
            *const objc2::runtime::AnyClass,
            objc2::runtime::Sel,
            *const *mut AnyObject,
            usize,
        ) -> *mut AnyObject = std::mem::transmute(super::runtime::objc_msgSend as *const c_void);
        func(cls, sel, objects.as_ptr(), objects.len())
    }
}

/// Gets the count of an `NSArray`.
pub fn nsarray_count(array: *mut AnyObject) -> usize {
    if array.is_null() {
        return 0;
    }
    // SAFETY: array is checked non-null above. Sending count to a valid NSArray object.
    // SAFETY: Caller/context ensures the preconditions for this unsafe operation are met.
    unsafe {
        let sel = objc2::sel!(count);
        let func: unsafe extern "C" fn(*const AnyObject, objc2::runtime::Sel) -> usize =
            std::mem::transmute(super::runtime::objc_msgSend as *const c_void);
        func(array as *const AnyObject, sel)
    }
}

/// Gets an object from an `NSArray` at index.
pub fn nsarray_object_at_index(array: *mut AnyObject, index: usize) -> *mut AnyObject {
    if array.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: array is checked non-null above. Caller must ensure index is within bounds.
    // SAFETY: Caller/context ensures the preconditions for this unsafe operation are met.
    unsafe {
        let sel = objc2::sel!(objectAtIndex:);
        let func: unsafe extern "C" fn(
            *const AnyObject,
            objc2::runtime::Sel,
            usize,
        ) -> *mut AnyObject = std::mem::transmute(super::runtime::objc_msgSend as *const c_void);
        func(array as *const AnyObject, sel, index)
    }
}

/// Creates a file handle for a file descriptor.
///
/// Uses `initWithFileDescriptor:closeOnDealloc:NO` to prevent `NSFileHandle`
/// from closing the fd when deallocated.
pub fn file_handle_for_fd(fd: i32) -> *mut AnyObject {
    // SAFETY: NSFileHandle class is always available. initWithFileDescriptor:closeOnDealloc: is called with a caller-provided fd; closeOnDealloc:false prevents NSFileHandle from closing the fd.
    // SAFETY: Receiver is a valid Objective-C object; selector and arguments match the method signature.
    unsafe {
        let cls = get_class("NSFileHandle").expect("NSFileHandle class not found");
        let obj = msg_send!(cls, alloc);

        let sel = objc2::sel!(initWithFileDescriptor:closeOnDealloc:);
        let func: unsafe extern "C" fn(
            *mut AnyObject,
            objc2::runtime::Sel,
            i32,
            bool,
        ) -> *mut AnyObject = std::mem::transmute(super::runtime::objc_msgSend as *const c_void);
        func(obj, sel, fd, false)
    }
}

/// Retains an Objective-C object.
pub fn retain(obj: *mut AnyObject) -> *mut AnyObject {
    if obj.is_null() {
        return obj;
    }
    // SAFETY: obj is checked non-null above. Sending retain to a valid ObjC object.
    // SAFETY: Receiver is a valid Objective-C object; selector and arguments match the method signature.
    unsafe { msg_send!(obj, retain) }
}

/// Releases an Objective-C object.
pub fn release(obj: *mut AnyObject) {
    if !obj.is_null() {
        // SAFETY: obj is checked non-null above. Sending release to a valid ObjC object.
        // SAFETY: Receiver is a valid Objective-C object; selector and arguments match the method signature.
        unsafe {
            let _: *mut AnyObject = msg_send!(obj, release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nsstring_roundtrip() {
        let original = "Hello, 世界! 🌍";
        let ns = nsstring(original);
        assert!(!ns.is_null());
        let back = nsstring_to_string(ns);
        assert_eq!(original, back);
    }

    #[test]
    fn test_nsarray() {
        let s1 = nsstring("one");
        let s2 = nsstring("two");
        let s3 = nsstring("three");
        let array = nsarray(&[s1, s2, s3]);
        assert!(!array.is_null());
        assert_eq!(nsarray_count(array), 3);
    }
}
