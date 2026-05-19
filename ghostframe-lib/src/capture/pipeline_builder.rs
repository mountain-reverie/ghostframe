//! Helpers for the Vulkan compute-pipeline boilerplate.
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
