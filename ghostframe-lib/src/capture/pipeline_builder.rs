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
    let compute_ci = vk::ComputePipelineCreateInfo::default()
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
    let buf_ci = vk::BufferCreateInfo::default()
        .size(size)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buffer = device.create_buffer(&buf_ci, None)?;
    let reqs = device.get_buffer_memory_requirements(buffer);
    let mem_type = find_memory_type(
        mem_props,
        reqs.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    )
    .ok_or_else(|| format!("no host-visible memory type for {label}"))?;
    let alloc_ci = vk::MemoryAllocateInfo::default()
        .allocation_size(reqs.size)
        .memory_type_index(mem_type);
    let memory = device.allocate_memory(&alloc_ci, None)?;
    device.bind_buffer_memory(buffer, memory, 0)?;
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
    let ptr = device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty())? as *mut T;
    Ok((buffer, memory, ptr))
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
