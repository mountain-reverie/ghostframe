//! Helpers for the Vulkan compute-pipeline and buffer-allocation boilerplate.
//!
//! # Compute pipeline helper
//!
//! Each pipeline in `gpu_pipeline.rs` follows the same five-step pattern:
//!   1. Define an array of `DescriptorSetLayoutBinding`s.
//!   2. Create a descriptor set layout from those bindings.
//!   3. Optionally declare a single push-constant range.
//!   4. Create a pipeline layout combining the DSL and the push range.
//!   5. Create a compute pipeline from the layout + shader module.
//!
//! [`build_compute_pipeline`] encapsulates steps 1–5. Pass `push_constant_size = 0`
//! to omit the push-constant range entirely.
//!
//! # Buffer allocation helpers
//!
//! Every `HOST_VISIBLE | HOST_COHERENT` buffer in `gpu_pipeline.rs` follows the
//! same six-step pattern: create, query requirements, find memory type, allocate,
//! bind, and optionally map. [`alloc_host_buffer`] encapsulates steps 1–5;
//! [`alloc_host_buffer_mapped`] adds the map and returns a typed pointer.

use ash::vk;

// ---------------------------------------------------------------------------
// Error-path cleanup
// ---------------------------------------------------------------------------

/// Runs a cleanup closure on drop unless [`Defer::disarm`]ed. Wraps a freshly
/// created Vulkan object so every `?` after its creation releases it; call
/// `disarm` once ownership has been handed to the caller.
pub(super) struct Defer<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Defer<F> {
    pub(super) fn new(f: F) -> Self {
        Self(Some(f))
    }

    pub(super) fn disarm(mut self) {
        self.0 = None;
    }
}

impl<F: FnOnce()> Drop for Defer<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            note_released();
            f();
        }
    }
}

#[cfg(test)]
thread_local! {
    static RELEASED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FAIL_AT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

/// Count one error-path release (tests assert on the delta).
#[inline]
pub(super) fn note_released() {
    #[cfg(test)]
    RELEASED.with(|c| c.set(c.get() + 1));
}

/// Releases counted on this thread so far (test builds only).
#[cfg(test)]
pub(super) fn released_count() -> usize {
    RELEASED.with(|c| c.get())
}

/// Make the next [`fail_point`] with this name return an error on this thread.
#[cfg(test)]
pub(super) fn inject_failure(name: &'static str) {
    FAIL_AT.with(|c| c.set(Some(name)));
}

/// A named place where tests can force an error right after a resource was
/// created, to exercise the cleanup of everything made so far. Compiles to
/// `Ok(())` outside test builds.
#[inline]
pub(super) fn fail_point(name: &'static str) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(test)]
    if FAIL_AT.with(|c| c.get()) == Some(name) {
        FAIL_AT.with(|c| c.set(None));
        return Err(format!("injected failure at {name}").into());
    }
    let _ = name;
    Ok(())
}

/// A descriptor binding spec used by [`build_compute_pipeline`].
///
/// `descriptor_count` is implicitly 1 and stage flags are implicitly
/// [`vk::ShaderStageFlags::COMPUTE`], which matches every binding in
/// `gpu_pipeline.rs`.
#[derive(Clone, Copy)]
pub struct BindingSpec {
    pub binding: u32,
    pub descriptor_type: vk::DescriptorType,
}

impl BindingSpec {
    pub const fn storage_image(binding: u32) -> Self {
        Self {
            binding,
            descriptor_type: vk::DescriptorType::STORAGE_IMAGE,
        }
    }

    pub const fn storage_buffer(binding: u32) -> Self {
        Self {
            binding,
            descriptor_type: vk::DescriptorType::STORAGE_BUFFER,
        }
    }
}

/// Build a single compute pipeline from a binding spec, push-constant size,
/// and shader module.
///
/// Returns `(descriptor_set_layout, pipeline_layout, pipeline)` in the same
/// order they need to be destroyed in `Drop`.
///
/// # Safety
///
/// Caller must ensure `device` is a valid `ash::Device` and `shader_module`
/// is a valid compute shader module created against the same device.
pub unsafe fn build_compute_pipeline(
    device: &ash::Device,
    bindings: &[BindingSpec],
    push_constant_size: u32,
    shader_module: vk::ShaderModule,
) -> Result<(vk::DescriptorSetLayout, vk::PipelineLayout, vk::Pipeline), vk::Result> {
    let layout_bindings: Vec<vk::DescriptorSetLayoutBinding> = bindings
        .iter()
        .map(|b| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b.binding)
                .descriptor_type(b.descriptor_type)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        })
        .collect();

    let dsl_ci = vk::DescriptorSetLayoutCreateInfo::default().bindings(&layout_bindings);
    let descriptor_set_layout = device.create_descriptor_set_layout(&dsl_ci, None)?;

    let push_ranges: [vk::PushConstantRange; 1] = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::COMPUTE)
        .offset(0)
        .size(push_constant_size)];

    let mut layout_ci = vk::PipelineLayoutCreateInfo::default()
        .set_layouts(std::slice::from_ref(&descriptor_set_layout));
    if push_constant_size > 0 {
        layout_ci = layout_ci.push_constant_ranges(&push_ranges);
    }
    let pipeline_layout = device.create_pipeline_layout(&layout_ci, None)?;

    let entry_name = c"main";
    let stage = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::COMPUTE)
        .module(shader_module)
        .name(entry_name);
    // DISPATCH_BASE lets a stage run over a band of workgroups
    // (`vkCmdDispatchBase`) with `gl_WorkGroupID` already offset, which is how
    // a damaged frame compares only the tile rows it uploaded.
    let compute_ci = vk::ComputePipelineCreateInfo::default()
        .flags(vk::PipelineCreateFlags::DISPATCH_BASE)
        .stage(stage)
        .layout(pipeline_layout);
    let pipelines = device
        .create_compute_pipelines(vk::PipelineCache::null(), &[compute_ci], None)
        .map_err(|(_, e)| e)?;

    Ok((descriptor_set_layout, pipeline_layout, pipelines[0]))
}

// ---------------------------------------------------------------------------
// Buffer-allocation helpers
// ---------------------------------------------------------------------------

/// Allocate a `HOST_VISIBLE | HOST_COHERENT` Vulkan buffer.
///
/// Encapsulates the six-step create → requirements → find-type → allocate →
/// bind pattern that repeats for every persistent buffer in `gpu_pipeline.rs`.
///
/// Returns `(buffer, memory)`. The memory is **not** mapped; call
/// [`alloc_host_buffer_mapped`] if you need a CPU pointer.
///
/// # Safety
///
/// Caller must ensure `device` and `mem_props` were obtained from the same
/// physical device, and that `size > 0`.
pub unsafe fn alloc_host_buffer(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    size: vk::DeviceSize,
    usage: vk::BufferUsageFlags,
    label: &str,
) -> Result<(vk::Buffer, vk::DeviceMemory), Box<dyn std::error::Error>> {
    alloc_host_buffer_inner(device, mem_props, size, usage, label, false)
}

/// [`alloc_host_buffer`], preferring a `HOST_CACHED` memory type when
/// `prefer_cached` is set (and one that is also `HOST_COHERENT` exists).
///
/// The first host-visible coherent type is often write-combined on a discrete
/// GPU: fast for the CPU to write, but a CPU *read* is uncached and runs near
/// 30 MB/s. A buffer the CPU reads back wants the cached type.
unsafe fn alloc_host_buffer_inner(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    size: vk::DeviceSize,
    usage: vk::BufferUsageFlags,
    label: &str,
    prefer_cached: bool,
) -> Result<(vk::Buffer, vk::DeviceMemory), Box<dyn std::error::Error>> {
    let buf_ci = vk::BufferCreateInfo::default()
        .size(size)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buffer = device.create_buffer(&buf_ci, None)?;
    let buffer_guard = Defer::new(|| device.destroy_buffer(buffer, None));
    let reqs = device.get_buffer_memory_requirements(buffer);
    let mem_type = choose_host_memory_type(mem_props, reqs.memory_type_bits, prefer_cached)
        .ok_or_else(|| format!("no host-visible memory type for {label}"))?;
    let alloc_ci = vk::MemoryAllocateInfo::default()
        .allocation_size(reqs.size)
        .memory_type_index(mem_type);
    let memory = device.allocate_memory(&alloc_ci, None)?;
    let memory_guard = Defer::new(|| device.free_memory(memory, None));
    fail_point("alloc_host_buffer:bind")?;
    device.bind_buffer_memory(buffer, memory, 0)?;
    buffer_guard.disarm();
    memory_guard.disarm();
    Ok((buffer, memory))
}

/// Allocate a `HOST_VISIBLE | HOST_COHERENT` Vulkan buffer and persistently map it.
///
/// Calls [`alloc_host_buffer`] then `vkMapMemory`, returning the mapped pointer
/// cast to `*mut T`.
///
/// # Safety
///
/// Same requirements as [`alloc_host_buffer`]. The returned pointer remains
/// valid until `vkUnmapMemory` is called or the memory is freed. The returned
/// pointer is only valid to dereference as `*mut T` if `T` matches the type
/// backing the buffer and the buffer has at least `size_of::<T>()` bytes.
/// The pointer alignment is whatever `vkMapMemory` provides (typically
/// page-aligned).
pub unsafe fn alloc_host_buffer_mapped<T>(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    size: vk::DeviceSize,
    usage: vk::BufferUsageFlags,
    label: &str,
) -> Result<(vk::Buffer, vk::DeviceMemory, *mut T), Box<dyn std::error::Error>> {
    let (buffer, memory) = alloc_host_buffer(device, mem_props, size, usage, label)?;
    let buffer_guard = Defer::new(|| device.destroy_buffer(buffer, None));
    let memory_guard = Defer::new(|| device.free_memory(memory, None));
    fail_point("alloc_host_buffer:map")?;
    let ptr = device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty())? as *mut T;
    buffer_guard.disarm();
    memory_guard.disarm();
    Ok((buffer, memory, ptr))
}

/// [`alloc_host_buffer_mapped`] for a buffer the CPU reads back in bulk:
/// prefers a cached memory type. See [`alloc_host_buffer_inner`].
///
/// # Safety
///
/// Same as [`alloc_host_buffer_mapped`].
pub unsafe fn alloc_host_readback_buffer_mapped<T>(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    size: vk::DeviceSize,
    usage: vk::BufferUsageFlags,
    label: &str,
) -> Result<(vk::Buffer, vk::DeviceMemory, *mut T), Box<dyn std::error::Error>> {
    let (buffer, memory) = alloc_host_buffer_inner(device, mem_props, size, usage, label, true)?;
    let buffer_guard = Defer::new(|| device.destroy_buffer(buffer, None));
    let memory_guard = Defer::new(|| device.free_memory(memory, None));
    let ptr = device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty())? as *mut T;
    buffer_guard.disarm();
    memory_guard.disarm();
    Ok((buffer, memory, ptr))
}

/// The memory type for a host-visible coherent buffer: a `HOST_CACHED` one
/// first when `prefer_cached`, otherwise (or when the device has none that is
/// also coherent) the first host-visible coherent type. Coherent either way,
/// so no explicit flush or invalidate is ever needed.
pub(super) fn choose_host_memory_type(
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    type_filter: u32,
    prefer_cached: bool,
) -> Option<u32> {
    let base = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    if prefer_cached {
        if let Some(t) = find_memory_type(
            mem_props,
            type_filter,
            base | vk::MemoryPropertyFlags::HOST_CACHED,
        ) {
            return Some(t);
        }
    }
    find_memory_type(mem_props, type_filter, base)
}

pub(super) fn find_memory_type(
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    type_filter: u32,
    required_flags: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..mem_props.memory_type_count).find(|&i| {
        let suitable = (type_filter & (1 << i)) != 0;
        let has_flags = mem_props.memory_types[i as usize]
            .property_flags
            .contains(required_flags);
        suitable && has_flags
    })
}

#[cfg(test)]
mod memory_type_tests {
    use super::*;

    fn props(types: &[vk::MemoryPropertyFlags]) -> vk::PhysicalDeviceMemoryProperties {
        let mut p = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: types.len() as u32,
            ..Default::default()
        };
        for (i, f) in types.iter().enumerate() {
            p.memory_types[i].property_flags = *f;
        }
        p
    }

    const HV: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::HOST_VISIBLE;
    const HC: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::HOST_COHERENT;
    const CACHED: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::HOST_CACHED;

    /// The layout of the dGPU this was measured on: a write-combined coherent
    /// type comes first, a cached coherent one later.
    fn dgpu() -> vk::PhysicalDeviceMemoryProperties {
        props(&[
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
            HV | HC,
            HV | HC | CACHED,
        ])
    }

    #[test]
    fn a_readback_buffer_takes_the_cached_type_over_the_first_coherent_one() {
        assert_eq!(choose_host_memory_type(&dgpu(), !0, true), Some(2));
    }

    #[test]
    fn an_upload_buffer_keeps_the_first_coherent_type() {
        assert_eq!(choose_host_memory_type(&dgpu(), !0, false), Some(1));
    }

    #[test]
    fn without_a_cached_coherent_type_the_first_coherent_one_is_used() {
        let p = props(&[HV | HC, HV | CACHED]);
        assert_eq!(choose_host_memory_type(&p, !0, true), Some(0));
    }

    #[test]
    fn the_buffers_own_type_mask_is_respected() {
        assert_eq!(choose_host_memory_type(&dgpu(), 0b010, true), Some(1));
        assert_eq!(choose_host_memory_type(&dgpu(), 0b001, true), None);
    }
}
