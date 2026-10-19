//! Headless Vulkan 1.0 identity query. No surface, device or window is created.
//! Only the fixed prefix of VkPhysicalDeviceProperties is hashed (API/driver,
//! vendor/device/type/name and pipelineCacheUUID). Keep this local identity
//! separate from Valve's GPU-independent SteamSwarm bucket descriptor.
use super::*;
use std::ffi::c_void;
#[repr(C)]
struct InstanceInfo {
    kind: u32,
    next: *const c_void,
    flags: u32,
    application: *const c_void,
    layers: u32,
    layer_names: *const c_void,
    extensions: u32,
    extension_names: *const c_void,
}
pub(super) fn identity() -> Result<Vec<u8>> {
    // All entry points have Vulkan's Linux C ABI. The loader remains open until
    // every instance is destroyed. Properties uses an aligned, oversized output
    // buffer; the Vulkan 1.0 structure is smaller than 8192 bytes.
    unsafe {
        let library = libc::dlopen(
            c"libvulkan.so.1".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        );
        if library.is_null() {
            return Err(failure(
                "Vulkan loader unavailable; shader processing skipped",
            ));
        }
        struct Library(*mut c_void);
        impl Drop for Library {
            fn drop(&mut self) {
                unsafe {
                    libc::dlclose(self.0);
                }
            }
        }
        let _library = Library(library);
        let create = libc::dlsym(library, c"vkCreateInstance".as_ptr());
        let destroy = libc::dlsym(library, c"vkDestroyInstance".as_ptr());
        let enumerate = libc::dlsym(library, c"vkEnumeratePhysicalDevices".as_ptr());
        let properties = libc::dlsym(library, c"vkGetPhysicalDeviceProperties".as_ptr());
        if [create, destroy, enumerate, properties]
            .iter()
            .any(|p| p.is_null())
        {
            return Err(failure("Incomplete Vulkan loader"));
        }
        let create: unsafe extern "C" fn(
            *const InstanceInfo,
            *const c_void,
            *mut *mut c_void,
        ) -> i32 = std::mem::transmute(create);
        let destroy: unsafe extern "C" fn(*mut c_void, *const c_void) =
            std::mem::transmute(destroy);
        let enumerate: unsafe extern "C" fn(*mut c_void, *mut u32, *mut *mut c_void) -> i32 =
            std::mem::transmute(enumerate);
        let properties: unsafe extern "C" fn(*mut c_void, *mut c_void) =
            std::mem::transmute(properties);
        let info = InstanceInfo {
            kind: 1,
            next: std::ptr::null(),
            flags: 0,
            application: std::ptr::null(),
            layers: 0,
            layer_names: std::ptr::null(),
            extensions: 0,
            extension_names: std::ptr::null(),
        };
        let mut instance = std::ptr::null_mut();
        if create(&info, std::ptr::null(), &mut instance) != 0 || instance.is_null() {
            return Err(failure("Cannot create headless Vulkan instance"));
        }
        struct Instance(
            *mut c_void,
            unsafe extern "C" fn(*mut c_void, *const c_void),
        );
        impl Drop for Instance {
            fn drop(&mut self) {
                unsafe {
                    (self.1)(self.0, std::ptr::null());
                }
            }
        }
        let _instance = Instance(instance, destroy);
        let mut count = 0;
        if enumerate(instance, &mut count, std::ptr::null_mut()) != 0 || count == 0 || count > 64 {
            return Err(failure("No bounded Vulkan device list available"));
        }
        let mut devices = vec![std::ptr::null_mut(); count as usize];
        if enumerate(instance, &mut count, devices.as_mut_ptr()) != 0
            || count as usize > devices.len()
        {
            return Err(failure("Vulkan devices changed during enumeration"));
        }
        let mut identity = Vec::new();
        for device in devices.into_iter().take(count as usize) {
            if device.is_null() {
                return Err(failure("Invalid Vulkan physical device"));
            }
            let mut output = [0u64; 1024];
            properties(device, output.as_mut_ptr().cast());
            let bytes = std::slice::from_raw_parts(output.as_ptr().cast::<u8>(), 292);
            identity.extend_from_slice(bytes);
        }
        Ok(identity)
    }
}
