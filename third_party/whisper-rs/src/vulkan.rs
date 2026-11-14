use std::ffi::{CStr, CString};
use whisper_rs_sys::{
    ggml_backend_buffer_type_t, ggml_backend_dev_buffer_type, ggml_backend_dev_description,
    ggml_backend_dev_memory, ggml_backend_reg_by_name, ggml_backend_reg_dev_count,
    ggml_backend_reg_dev_get,
};

#[derive(Debug, Clone)]
pub struct VKVram {
    pub free: usize,
    pub total: usize,
}

/// Human-readable device information
#[derive(Debug, Clone)]
pub struct VkDeviceInfo {
    pub id: i32,
    pub name: String,
    pub vram: VKVram,
    /// Buffer type to pass to `whisper::Backend::create_buffer`
    pub buf_type: ggml_backend_buffer_type_t,
}
/// Enumerate every physical GPU ggml can see.
///
/// Note: integrated GPUs are returned *after* discrete ones,
/// mirroring ggml’s C logic.
pub fn list_devices() -> Vec<VkDeviceInfo> {
    unsafe {
        let name = CString::new("Vulkan").expect("static backend name");
        let reg = ggml_backend_reg_by_name(name.as_ptr());
        if reg.is_null() {
            return Vec::new();
        }
        let n = ggml_backend_reg_dev_count(reg);
        (0..n)
            .map(|id| {
                let device = ggml_backend_reg_dev_get(reg, id);
                let ptr = ggml_backend_dev_description(device);
                let name = if ptr.is_null() {
                    "Vulkan device".to_owned()
                } else {
                    CStr::from_ptr(ptr).to_string_lossy().into_owned()
                };
                let mut free = 0usize;
                let mut total = 0usize;
                ggml_backend_dev_memory(device, &mut free, &mut total);
                VkDeviceInfo {
                    id: id as i32,
                    name,
                    vram: VKVram { free, total },
                    buf_type: ggml_backend_dev_buffer_type(device),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod vulkan_tests {
    use super::*;

    #[test]
    fn enumerate_must_not_panic() {
        let _ = list_devices();
    }

    #[test]
    fn sane_device_info() {
        let gpus = list_devices();
        let mut seen = std::collections::HashSet::new();

        for dev in &gpus {
            assert!(seen.insert(dev.id), "duplicated id {}", dev.id);
            assert!(!dev.name.trim().is_empty(), "GPU {} has empty name", dev.id);
            assert!(
                dev.vram.total >= dev.vram.free,
                "GPU {} total < free",
                dev.id
            );
        }
    }
}
