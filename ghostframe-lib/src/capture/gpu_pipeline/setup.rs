//! Construction of [`GpuFrameProcessor`].
//!
//! Holds the giant `unsafe fn new_inner` that allocates the Vulkan device,
//! the descriptor pool, all 9 compute pipelines, and the persistent buffers
//! used across frames. Helpers for individual buffers and pipelines live in
//! [`super::super::pipeline_builder`].

use ash::vk;
use std::ffi::CStr;

use super::super::pipeline_builder::{self, alloc_host_buffer, alloc_host_buffer_mapped, BindingSpec};
use super::*;

impl GpuFrameProcessor {
    pub(super) unsafe fn new_inner(max_tiles: u32) -> Result<Self, Box<dyn std::error::Error>> {
        // --- Entry ---
        let entry = ash::Entry::load()?;

        // --- Instance ---
        let app_info = vk::ApplicationInfo::default()
            .application_name(c"ghostframe")
            .application_version(vk::make_api_version(0, 0, 1, 0))
            .engine_name(c"ghostframe")
            .api_version(vk::make_api_version(0, 1, 1, 0));

        let instance_ci = vk::InstanceCreateInfo::default().application_info(&app_info);
        let instance = entry.create_instance(&instance_ci, None)?;

        // --- Physical device ---
        let phys_devices = instance.enumerate_physical_devices()?;
        if phys_devices.is_empty() {
            return Err("no Vulkan physical devices found".into());
        }

        let required_device_exts: [&CStr; 2] = [
            ash::khr::external_memory_fd::NAME,
            ash::ext::external_memory_dma_buf::NAME,
        ];

        let mut chosen = None;
        for &pdev in &phys_devices {
            let supported = instance.enumerate_device_extension_properties(pdev)?;
            let has_all = required_device_exts.iter().all(|req| {
                supported
                    .iter()
                    .any(|ext| CStr::from_ptr(ext.extension_name.as_ptr()) == *req)
            });
            if has_all {
                chosen = Some(pdev);
                break;
            }
        }

        let physical_device =
            chosen.ok_or("no Vulkan device supports DMA-BUF import extensions")?;

        // --- Queue family: prefer COMPUTE-only, fall back to GRAPHICS|COMPUTE ---
        let queue_families = instance.get_physical_device_queue_family_properties(physical_device);

        let queue_family_index = queue_families
            .iter()
            .position(|qf| {
                qf.queue_flags.contains(vk::QueueFlags::COMPUTE)
                    && !qf.queue_flags.contains(vk::QueueFlags::GRAPHICS)
            })
            .or_else(|| {
                queue_families
                    .iter()
                    .position(|qf| qf.queue_flags.contains(vk::QueueFlags::COMPUTE))
            })
            .ok_or("no compute-capable queue family")? as u32;

        // --- Logical device ---
        let queue_priorities = [1.0_f32];
        let queue_ci = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&queue_priorities)];

        let ext_names_raw: Vec<*const i8> =
            required_device_exts.iter().map(|e| e.as_ptr()).collect();

        let device_ci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_ci)
            .enabled_extension_names(&ext_names_raw);

        let device = instance.create_device(physical_device, &device_ci, None)?;
        let queue = device.get_device_queue(queue_family_index, 0);

        // --- Command pool ---
        let pool_ci = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family_index)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = device.create_command_pool(&pool_ci, None)?;

        // --- SAD Shader module ---
        let sad_spv = include_bytes!("../shaders/tile_sad.spv");
        let sad_spv_words = ash::util::read_spv(&mut std::io::Cursor::new(sad_spv.as_slice()))?;
        let shader_module_ci = vk::ShaderModuleCreateInfo::default().code(&sad_spv_words);
        let shader_module = device.create_shader_module(&shader_module_ci, None)?;

        // --- NV12 Shader module ---
        let nv12_spv = include_bytes!("../shaders/bgra_to_nv12.spv");
        let nv12_spv_words = ash::util::read_spv(&mut std::io::Cursor::new(nv12_spv.as_slice()))?;
        let nv12_shader_ci = vk::ShaderModuleCreateInfo::default().code(&nv12_spv_words);
        let nv12_shader_module = device.create_shader_module(&nv12_shader_ci, None)?;

        // --- Analysis Shader module ---
        let analysis_spv = include_bytes!("../shaders/tile_analysis.spv");
        let analysis_spv_words =
            ash::util::read_spv(&mut std::io::Cursor::new(analysis_spv.as_slice()))?;
        let analysis_shader_ci = vk::ShaderModuleCreateInfo::default().code(&analysis_spv_words);
        let analysis_shader_module = device.create_shader_module(&analysis_shader_ci, None)?;

        // --- SAD compute pipeline ---
        // Bindings:
        //   0: STORAGE_IMAGE (current frame)
        //   1: STORAGE_IMAGE (prev frame)
        //   2: STORAGE_BUFFER (SAD output)
        // Push constants: 3 × u32 = 12 bytes (frame_width, frame_height, cols).
        let (descriptor_set_layout, pipeline_layout, pipeline) =
            pipeline_builder::build_compute_pipeline(
                &device,
                &[
                    BindingSpec::storage_image(0),
                    BindingSpec::storage_image(1),
                    BindingSpec::storage_buffer(2),
                ],
                12,
                shader_module,
            )?;

        // --- NV12 compute pipeline ---
        // Bindings:
        //   0: STORAGE_IMAGE (BGRA input)
        //   1: STORAGE_BUFFER (NV12 output)
        // Push constants: 5 × u32 = 20 bytes (width, height, y_stride, uv_offset, uv_stride).
        let (nv12_descriptor_set_layout, nv12_pipeline_layout, nv12_pipeline) =
            pipeline_builder::build_compute_pipeline(
                &device,
                &[
                    BindingSpec::storage_image(0),
                    BindingSpec::storage_buffer(1),
                ],
                20,
                nv12_shader_module,
            )?;

        // --- Analysis compute pipeline ---
        // Bindings:
        //   0: STORAGE_IMAGE (current frame, read-only in shader)
        //   1: STORAGE_BUFFER (analysis output)
        // Push constants: 3 × u32 = 12 bytes (frame_width, frame_height, cols) — same
        // shape as SAD's, but a distinct pipeline-layout object is required because
        // descriptor sets differ.
        let (analysis_descriptor_set_layout, analysis_pipeline_layout, analysis_pipeline) =
            pipeline_builder::build_compute_pipeline(
                &device,
                &[
                    BindingSpec::storage_image(0),
                    BindingSpec::storage_buffer(1),
                ],
                12,
                analysis_shader_module,
            )?;

        // --- PalRLE compact shader module ---
        let palrle_compact_spv = include_bytes!("../shaders/palrle_compact.spv");
        let palrle_compact_spv_words =
            ash::util::read_spv(&mut std::io::Cursor::new(palrle_compact_spv.as_slice()))?;
        let palrle_compact_shader_ci =
            vk::ShaderModuleCreateInfo::default().code(&palrle_compact_spv_words);
        let palrle_compact_shader_module =
            device.create_shader_module(&palrle_compact_shader_ci, None)?;

        // --- PalRLE compact compute pipeline ---
        // Bindings:
        //   0: STORAGE_BUFFER (SAD output, read-only)
        //   1: STORAGE_BUFFER (tile analysis, read-only)
        //   2: STORAGE_BUFFER (compact list output)
        //   3: STORAGE_BUFFER (compact count output)
        // Push constants: 3 × u32 = 12 bytes (cols, rows, dirty_threshold).
        let (
            palrle_compact_descriptor_set_layout,
            palrle_compact_pipeline_layout,
            palrle_compact_pipeline,
        ) = pipeline_builder::build_compute_pipeline(
            &device,
            &[
                BindingSpec::storage_buffer(0),
                BindingSpec::storage_buffer(1),
                BindingSpec::storage_buffer(2),
                BindingSpec::storage_buffer(3),
            ],
            12,
            palrle_compact_shader_module,
        )?;

        // --- Cdf53 compact shader module (Stage 4a) ---
        let cdf53_compact_spv = include_bytes!("../shaders/cdf53_compact.spv");
        let cdf53_compact_spv_words =
            ash::util::read_spv(&mut std::io::Cursor::new(cdf53_compact_spv.as_slice()))?;
        let cdf53_compact_shader_ci =
            vk::ShaderModuleCreateInfo::default().code(&cdf53_compact_spv_words);
        let cdf53_compact_shader_module =
            device.create_shader_module(&cdf53_compact_shader_ci, None)?;

        // --- Cdf53 compact compute pipeline ---
        // Bindings:
        //   0: STORAGE_BUFFER (SAD output, read-only)
        //   1: STORAGE_BUFFER (tile analysis, read-only)
        //   2: STORAGE_BUFFER (cdf53 compact list output)
        //   3: STORAGE_BUFFER (cdf53 compact count output)
        // Push constants: 4 × u32 = 16 bytes
        //   (cols, rows, dirty_threshold, unique_colors_unknown_sentinel).
        let (
            cdf53_compact_descriptor_set_layout,
            cdf53_compact_pipeline_layout,
            cdf53_compact_pipeline,
        ) = pipeline_builder::build_compute_pipeline(
            &device,
            &[
                BindingSpec::storage_buffer(0),
                BindingSpec::storage_buffer(1),
                BindingSpec::storage_buffer(2),
                BindingSpec::storage_buffer(3),
            ],
            16,
            cdf53_compact_shader_module,
        )?;

        // --- PalRLE indirect-args shader module ---
        let palrle_indirect_args_spv = include_bytes!("../shaders/palrle_indirect_args.spv");
        let palrle_indirect_args_spv_words =
            ash::util::read_spv(&mut std::io::Cursor::new(palrle_indirect_args_spv.as_slice()))?;
        let palrle_indirect_args_shader_ci =
            vk::ShaderModuleCreateInfo::default().code(&palrle_indirect_args_spv_words);
        let palrle_indirect_args_shader_module =
            device.create_shader_module(&palrle_indirect_args_shader_ci, None)?;

        // --- PalRLE indirect-args compute pipeline (no push constants) ---
        // Bindings:
        //   0: STORAGE_BUFFER (compact count, read-only)
        //   1: STORAGE_BUFFER (indirect args output)
        let (
            palrle_indirect_args_descriptor_set_layout,
            palrle_indirect_args_pipeline_layout,
            palrle_indirect_args_pipeline,
        ) = pipeline_builder::build_compute_pipeline(
            &device,
            &[
                BindingSpec::storage_buffer(0),
                BindingSpec::storage_buffer(1),
            ],
            0,
            palrle_indirect_args_shader_module,
        )?;

        // --- pal_rle_index shader module (Stage 3) ---
        let pal_rle_index_spv = include_bytes!("../shaders/pal_rle_index.spv");
        let pal_rle_index_spv_words =
            ash::util::read_spv(&mut std::io::Cursor::new(pal_rle_index_spv.as_slice()))?;
        let pal_rle_index_shader_ci =
            vk::ShaderModuleCreateInfo::default().code(&pal_rle_index_spv_words);
        let pal_rle_index_shader_module =
            device.create_shader_module(&pal_rle_index_shader_ci, None)?;

        // --- pal_rle_index compute pipeline ---
        // Bindings:
        //   0: STORAGE_IMAGE (current_frame, read-only)
        //   1: STORAGE_BUFFER (compact_list, read-only)
        //   2: STORAGE_BUFFER (frame_palette_set, read-only)
        //   3: STORAGE_BUFFER (per_tile_frame_palette_id, read-only)
        //   4: STORAGE_BUFFER (folded_into, read-only)
        //   5: STORAGE_BUFFER (index_buffer output)
        // Push constants: 1 × u32 = 4 bytes (cols).
        let (
            pal_rle_index_descriptor_set_layout,
            pal_rle_index_pipeline_layout,
            pal_rle_index_pipeline,
        ) = pipeline_builder::build_compute_pipeline(
            &device,
            &[
                BindingSpec::storage_image(0),
                BindingSpec::storage_buffer(1),
                BindingSpec::storage_buffer(2),
                BindingSpec::storage_buffer(3),
                BindingSpec::storage_buffer(4),
                BindingSpec::storage_buffer(5),
            ],
            4,
            pal_rle_index_shader_module,
        )?;

        // --- Descriptor pool ---
        // 10 sets: SAD (2 STORAGE_IMAGE + 1 STORAGE_BUFFER)
        //        + NV12 (1 STORAGE_IMAGE + 1 STORAGE_BUFFER)
        //        + Analysis (1 STORAGE_IMAGE + 1 STORAGE_BUFFER)
        //        + palrle_compact (4 STORAGE_BUFFER)
        //        + palrle_indirect_args (2 STORAGE_BUFFER)
        //        + palette_fold (6 STORAGE_BUFFER)
        //        + palette_subset_fold_init (1 STORAGE_BUFFER)
        //        + palette_subset_fold (2 STORAGE_BUFFER)
        //        + pal_rle_index (1 STORAGE_IMAGE + 5 STORAGE_BUFFER)
        //        + cdf53_compact (4 STORAGE_BUFFER)   ← added M3.3a
        // Total: 5 STORAGE_IMAGE, 27 STORAGE_BUFFER, 10 max_sets.
        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_IMAGE,
                descriptor_count: 5,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 27,
            },
        ];
        let dp_ci = vk::DescriptorPoolCreateInfo::default()
            .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
            .max_sets(10)
            .pool_sizes(&pool_sizes);
        let descriptor_pool = device.create_descriptor_pool(&dp_ci, None)?;

        // --- SAD output buffer ---
        let sad_buf_size = (max_tiles * 4) as vk::DeviceSize;
        let mem_props = instance.get_physical_device_memory_properties(physical_device);
        let (sad_buffer, sad_memory, sad_ptr) = alloc_host_buffer_mapped::<u32>(
            &device,
            &mem_props,
            sad_buf_size,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            "SAD buffer",
        )?;

        // --- Analysis output buffer ---
        let analysis_entry_bytes = std::mem::size_of::<TileAnalysis>() as vk::DeviceSize;
        let analysis_buf_size = (max_tiles as vk::DeviceSize) * analysis_entry_bytes;
        let (analysis_buffer, analysis_memory, analysis_ptr) =
            alloc_host_buffer_mapped::<TileAnalysis>(
                &device,
                &mem_props,
                analysis_buf_size,
                vk::BufferUsageFlags::STORAGE_BUFFER,
                "analysis buffer",
            )?;

        // --- PalRLE compact list buffer ---
        // One u32 per tile slot. HOST_VISIBLE | HOST_COHERENT, STORAGE_BUFFER.
        let palrle_compact_list_size = (max_tiles * 4) as vk::DeviceSize;
        let (palrle_compact_list_buffer, palrle_compact_list_memory, palrle_compact_list_ptr) =
            alloc_host_buffer_mapped::<u32>(
                &device,
                &mem_props,
                palrle_compact_list_size,
                vk::BufferUsageFlags::STORAGE_BUFFER,
                "palrle compact list buffer",
            )?;

        // --- PalRLE compact count buffer ---
        // 4 bytes (one u32). HOST_VISIBLE | HOST_COHERENT, STORAGE_BUFFER | TRANSFER_DST.
        let palrle_compact_count_size = 4_u64;
        let (palrle_compact_count_buffer, palrle_compact_count_memory, palrle_compact_count_ptr) =
            alloc_host_buffer_mapped::<u32>(
                &device,
                &mem_props,
                palrle_compact_count_size,
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
                "palrle compact count buffer",
            )?;

        // --- Cdf53 compact list buffer (Stage 4a) ---
        // One u32 per tile slot. HOST_VISIBLE | HOST_COHERENT, STORAGE_BUFFER.
        let cdf53_compact_list_size = (max_tiles * 4) as vk::DeviceSize;
        let (cdf53_compact_list_buffer, cdf53_compact_list_memory, cdf53_compact_list_ptr) =
            alloc_host_buffer_mapped::<u32>(
                &device,
                &mem_props,
                cdf53_compact_list_size,
                vk::BufferUsageFlags::STORAGE_BUFFER,
                "cdf53 compact list buffer",
            )?;

        // --- Cdf53 compact count buffer (Stage 4a) ---
        // 4 bytes (one u32). HOST_VISIBLE | HOST_COHERENT, STORAGE_BUFFER | TRANSFER_DST.
        let cdf53_compact_count_size = 4_u64;
        let (cdf53_compact_count_buffer, cdf53_compact_count_memory, cdf53_compact_count_ptr) =
            alloc_host_buffer_mapped::<u32>(
                &device,
                &mem_props,
                cdf53_compact_count_size,
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
                "cdf53 compact count buffer",
            )?;

        // --- PalRLE indirect args buffer ---
        // 12 bytes (3 u32s: group_count_x/y/z). Written by shader, read as
        // INDIRECT_BUFFER. HOST_VISIBLE | HOST_COHERENT for simple initialization
        // (no staging buffer needed, and avoids DEVICE_LOCAL complication in Drop).
        // HOST_VISIBLE chosen (deviation from spec D14: DEVICE_LOCAL) for simpler
        // init — vkCmdDispatchIndirect reads correctly from HOST_VISIBLE memory.
        // PCIe-bar read cost is acceptable for 12 B per frame.
        let palrle_indirect_args_size = 12_u64;
        let (palrle_indirect_args_buffer, palrle_indirect_args_memory, init_ptr) =
            alloc_host_buffer_mapped::<u32>(
                &device,
                &mem_props,
                palrle_indirect_args_size,
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::INDIRECT_BUFFER,
                "palrle indirect args buffer",
            )?;
        // Initialize to (0, 1, 1) so a stale dispatch is a safe no-op.
        init_ptr.write(0);
        init_ptr.add(1).write(1);
        init_ptr.add(2).write(1);
        device.unmap_memory(palrle_indirect_args_memory);

        // --- palette_fold shader module ---
        let palette_fold_spv = include_bytes!("../shaders/palette_fold.spv");
        let palette_fold_spv_words =
            ash::util::read_spv(&mut std::io::Cursor::new(palette_fold_spv.as_slice()))?;
        let palette_fold_shader_ci =
            vk::ShaderModuleCreateInfo::default().code(&palette_fold_spv_words);
        let palette_fold_shader_module =
            device.create_shader_module(&palette_fold_shader_ci, None)?;

        // --- palette_fold compute pipeline (no push constants) ---
        // Bindings:
        //   0: STORAGE_BUFFER (tile analysis input, read-only)
        //   1: STORAGE_BUFFER (compact list input, read-only)
        //   2: STORAGE_BUFFER (frame_palette_set output)
        //   3: STORAGE_BUFFER (frame_palette_count output)
        //   4: STORAGE_BUFFER (hash_table, per-frame scratch)
        //   5: STORAGE_BUFFER (per_tile_frame_palette_id output)
        let (
            palette_fold_descriptor_set_layout,
            palette_fold_pipeline_layout,
            palette_fold_pipeline,
        ) = pipeline_builder::build_compute_pipeline(
            &device,
            &[
                BindingSpec::storage_buffer(0),
                BindingSpec::storage_buffer(1),
                BindingSpec::storage_buffer(2),
                BindingSpec::storage_buffer(3),
                BindingSpec::storage_buffer(4),
                BindingSpec::storage_buffer(5),
            ],
            0,
            palette_fold_shader_module,
        )?;

        // --- frame_palette_set buffer ---
        // 256 PaletteEntry slots × 80 bytes = 20480 bytes.
        // HOST_VISIBLE | HOST_COHERENT, STORAGE_BUFFER | TRANSFER_DST.
        // TRANSFER_DST allows Task 12b to zero-fill between frames via
        // cmd_fill_buffer so Stage 2b can safely check count == 0 per slot.
        let frame_palette_set_size =
            (PALETTE_HASH_SLOTS * std::mem::size_of::<FramePaletteEntryRaw>()) as vk::DeviceSize;
        let (frame_palette_set_buffer, frame_palette_set_memory, frame_palette_set_ptr) =
            alloc_host_buffer_mapped::<FramePaletteEntryRaw>(
                &device,
                &mem_props,
                frame_palette_set_size,
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
                "frame_palette_set buffer",
            )?;

        // --- frame_palette_count buffer ---
        // 4 bytes. HOST_VISIBLE | HOST_COHERENT | TRANSFER_DST (for cmd_fill_buffer zero between frames).
        let frame_palette_count_size = 4_u64;
        let (frame_palette_count_buffer, frame_palette_count_memory, frame_palette_count_ptr) =
            alloc_host_buffer_mapped::<u32>(
                &device,
                &mem_props,
                frame_palette_count_size,
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
                "frame_palette_count buffer",
            )?;

        // --- hash_table buffer ---
        // 256 slots × 4 bytes = 1024 bytes. HOST_VISIBLE | HOST_COHERENT | TRANSFER_DST.
        // No persistent CPU mapping needed.
        let (hash_table_buffer, hash_table_memory) = alloc_host_buffer(
            &device,
            &mem_props,
            PALETTE_SLOT_U32_BYTES,
            vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            "hash_table buffer",
        )?;

        // --- per_tile_frame_palette_id buffer ---
        // (max_tiles + 3) / 4 * 4 bytes (round up to u32 alignment).
        // HOST_VISIBLE | HOST_COHERENT | TRANSFER_DST, STORAGE_BUFFER.
        let per_tile_id_size = (((max_tiles + 3) / 4 * 4) as vk::DeviceSize).max(4);
        let (
            per_tile_frame_palette_id_buffer,
            per_tile_frame_palette_id_memory,
            per_tile_frame_palette_id_ptr,
        ) = alloc_host_buffer_mapped::<u8>(
            &device,
            &mem_props,
            per_tile_id_size,
            vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            "per_tile_frame_palette_id buffer",
        )?;

        // --- palette_subset_fold_init shader module ---
        let palette_subset_fold_init_spv =
            include_bytes!("../shaders/palette_subset_fold_init.spv");
        let palette_subset_fold_init_spv_words = ash::util::read_spv(&mut std::io::Cursor::new(
            palette_subset_fold_init_spv.as_slice(),
        ))?;
        let palette_subset_fold_init_shader_ci =
            vk::ShaderModuleCreateInfo::default().code(&palette_subset_fold_init_spv_words);
        let palette_subset_fold_init_shader_module =
            device.create_shader_module(&palette_subset_fold_init_shader_ci, None)?;

        // --- palette_subset_fold_init compute pipeline (no push constants) ---
        // Bindings:
        //   0: STORAGE_BUFFER (FoldedInto)
        let (
            palette_subset_fold_init_descriptor_set_layout,
            palette_subset_fold_init_pipeline_layout,
            palette_subset_fold_init_pipeline,
        ) = pipeline_builder::build_compute_pipeline(
            &device,
            &[BindingSpec::storage_buffer(0)],
            0,
            palette_subset_fold_init_shader_module,
        )?;

        // --- palette_subset_fold shader module ---
        let palette_subset_fold_spv = include_bytes!("../shaders/palette_subset_fold.spv");
        let palette_subset_fold_spv_words = ash::util::read_spv(&mut std::io::Cursor::new(
            palette_subset_fold_spv.as_slice(),
        ))?;
        let palette_subset_fold_shader_ci =
            vk::ShaderModuleCreateInfo::default().code(&palette_subset_fold_spv_words);
        let palette_subset_fold_shader_module =
            device.create_shader_module(&palette_subset_fold_shader_ci, None)?;

        // --- palette_subset_fold compute pipeline (no push constants) ---
        // Bindings:
        //   0: STORAGE_BUFFER (FramePaletteSet, read-only)
        //   1: STORAGE_BUFFER (FoldedInto, read-write)
        let (
            palette_subset_fold_descriptor_set_layout,
            palette_subset_fold_pipeline_layout,
            palette_subset_fold_pipeline,
        ) = pipeline_builder::build_compute_pipeline(
            &device,
            &[
                BindingSpec::storage_buffer(0),
                BindingSpec::storage_buffer(1),
            ],
            0,
            palette_subset_fold_shader_module,
        )?;

        // --- folded_into buffer ---
        // folded_into_buffer: each frame is reset by palette_subset_fold_init.comp
        // (the init shader writes the default-self sentinel). TRANSFER_DST is kept
        // for symmetry with other Stage 2 buffers and to permit a future fallback
        // to vkCmdFillBuffer if the init dispatch is ever replaced.
        // 256 slots × 4 bytes = 1024 bytes. HOST_VISIBLE | HOST_COHERENT,
        // persistently mapped for CPU readback (Task 12b+).
        let (folded_into_buffer, folded_into_memory, folded_into_ptr) =
            alloc_host_buffer_mapped::<u32>(
                &device,
                &mem_props,
                PALETTE_SLOT_U32_BYTES,
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
                "folded_into buffer",
            )?;

        // --- index_buffer (Stage 3 output) ---
        // max_tiles * 512 bytes: each tile has 128 u32 entries of packed 4-bit indices.
        // HOST_VISIBLE | HOST_COHERENT, STORAGE_BUFFER, persistently mapped.
        let index_buffer_size =
            ((max_tiles as vk::DeviceSize) * PER_TILE_INDEX_BYTES).max(PER_TILE_INDEX_BYTES);
        let (index_buffer, index_buffer_memory, index_buffer_ptr) =
            alloc_host_buffer_mapped::<u8>(
                &device,
                &mem_props,
                index_buffer_size,
                vk::BufferUsageFlags::STORAGE_BUFFER,
                "index buffer",
            )?;

        Ok(Self {
            _entry: entry,
            instance,
            physical_device,
            device,
            queue,
            queue_family_index,
            command_pool,
            shader_module,
            pipeline,
            pipeline_layout,
            descriptor_set_layout,
            nv12_shader_module,
            nv12_pipeline,
            nv12_pipeline_layout,
            nv12_descriptor_set_layout,
            analysis_shader_module,
            analysis_pipeline,
            analysis_pipeline_layout,
            analysis_descriptor_set_layout,
            analysis_buffer,
            analysis_memory,
            analysis_ptr,
            palrle_compact_shader_module,
            palrle_compact_pipeline,
            palrle_compact_pipeline_layout,
            palrle_compact_descriptor_set_layout,
            palrle_compact_list_buffer,
            palrle_compact_list_memory,
            palrle_compact_list_ptr,
            palrle_compact_count_buffer,
            palrle_compact_count_memory,
            palrle_compact_count_ptr,
            cdf53_compact_shader_module,
            cdf53_compact_pipeline,
            cdf53_compact_pipeline_layout,
            cdf53_compact_descriptor_set_layout,
            cdf53_compact_list_buffer,
            cdf53_compact_list_memory,
            cdf53_compact_list_ptr: cdf53_compact_list_ptr as *const u32,
            cdf53_compact_count_buffer,
            cdf53_compact_count_memory,
            cdf53_compact_count_ptr: cdf53_compact_count_ptr as *const u32,
            palrle_indirect_args_shader_module,
            palrle_indirect_args_pipeline,
            palrle_indirect_args_pipeline_layout,
            palrle_indirect_args_descriptor_set_layout,
            palrle_indirect_args_buffer,
            palrle_indirect_args_memory,
            palette_fold_shader_module,
            palette_fold_pipeline,
            palette_fold_pipeline_layout,
            palette_fold_descriptor_set_layout,
            frame_palette_set_buffer,
            frame_palette_set_memory,
            frame_palette_set_ptr,
            frame_palette_count_buffer,
            frame_palette_count_memory,
            frame_palette_count_ptr,
            hash_table_buffer,
            hash_table_memory,
            per_tile_frame_palette_id_buffer,
            per_tile_frame_palette_id_memory,
            per_tile_frame_palette_id_ptr,
            palette_subset_fold_init_shader_module,
            palette_subset_fold_init_pipeline,
            palette_subset_fold_init_pipeline_layout,
            palette_subset_fold_init_descriptor_set_layout,
            palette_subset_fold_shader_module,
            palette_subset_fold_pipeline,
            palette_subset_fold_pipeline_layout,
            palette_subset_fold_descriptor_set_layout,
            folded_into_buffer,
            folded_into_memory,
            folded_into_ptr,
            pal_rle_index_shader_module,
            pal_rle_index_pipeline,
            pal_rle_index_pipeline_layout,
            pal_rle_index_descriptor_set_layout,
            index_buffer,
            index_buffer_memory,
            index_buffer_ptr,
            descriptor_pool,
            prev_image: None,
            sad_buffer,
            sad_memory,
            sad_ptr,
            max_tiles,
            nv12_buffer: None,
            last_width: 0,
            last_height: 0,
            force_all_dirty_remaining: 0,
        })
    }
}
