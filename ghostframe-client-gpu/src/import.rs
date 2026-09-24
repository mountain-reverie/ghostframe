//! Import an externally produced dmabuf as wgpu textures.
//!
//! The mirror of [`crate::export`], and subject to one constraint that
//! shapes everything here: **wgpu-hal 30 has `texture_from_raw` but no
//! `buffer_from_raw`**, so an imported dmabuf must become a `VkImage`. For a
//! `VK_IMAGE_TILING_LINEAR` image the driver picks the row pitch, and
//! without `VK_EXT_image_drm_format_modifier` -- which this hardware does
//! not expose -- there is no way to tell Vulkan the pitch the producer used.
//!
//! So the pitch is *checked*, not assumed: `vkGetImageSubresourceLayout`
//! reports what the driver chose, and a disagreement fails the import. A
//! wrong pitch does not produce obviously broken output; it produces a
//! sheared image that looks like a decode bug and costs a day. That check
//! runs before anything else costly: the subresource layout is a property
//! of the image alone (see [`check_pitch`]'s SAFETY note), so both planes
//! are checked immediately after `vkCreateImage`, before any fd is
//! duplicated or any memory imported. A rejected frame then costs exactly
//! two `vkCreateImage` calls, not a wasted import.
//!
//! ## The imported fd is not ours to close
//!
//! `vkAllocateMemory` with `VkImportMemoryFdInfoKHR` "transfers ownership of
//! the file descriptor from the application to the Vulkan implementation"
//! (`VK_KHR_external_memory_fd`). That is the whole rule, unconditionally,
//! for every driver: once that call returns success, this process must never
//! call `close()` on that fd again. *When* the implementation actually
//! closes it -- synchronously inside the call, lazily at `vkFreeMemory`, or
//! never observably at all -- is not specified and not ours to depend on.
//! Code here must treat the fd as gone the instant the call succeeds, full
//! stop, regardless of what any particular driver happens to do.
//!
//! This was found on RADV, where the close is synchronous inside
//! `vkAllocateMemory`, so holding the duplicated fd in an `OwnedFd` field --
//! as if `ImportedNv12` still owned it -- made its `Drop` call `close()` on
//! an fd already closed, which Rust's `OwnedFd` treats as a double-close and
//! aborts the process (`std::os::fd::owned`'s `debug_assert_fd_is_open`).
//! On a driver that defers the close, the same bug would be silent -- which
//! is worse. So the dup'd fd's ownership is handed to the Vulkan
//! implementation (`OwnedFd::into_raw_fd`, discarding the raw value)
//! immediately after a successful `allocate_memory`, not stored.
//!
//! ## No foreign-queue-family acquire, and why that is fine here
//!
//! The dmabuf's contents were written by VA-API, a producer this Vulkan
//! device has no queue-family relationship with. The textbook way to hand
//! off a foreign-written image is a `VK_QUEUE_FAMILY_FOREIGN_EXT` acquire
//! barrier; this module issues none. `wrap_texture` also reports the
//! image's initial state to wgpu as `TextureUses::UNINITIALIZED`, so wgpu's
//! first transition uses `oldLayout = VK_IMAGE_LAYOUT_UNDEFINED` -- which
//! the Vulkan spec explicitly permits an implementation to treat as license
//! to discard the image's contents. Skipping both would be a real hazard on
//! an implementation that relies on the acquire barrier to make the
//! producer's writes visible, or that acts on `UNDEFINED`'s permission to
//! discard.
//!
//! What licenses skipping them here is the same measurement the modifier
//! decision below leans on: spec §7.2 read the dmabuf back with a plain CPU
//! `mmap` -- no Vulkan queue-family transition anywhere in that path -- and
//! found it byte-for-byte identical to `av_hwframe_transfer_data`'s output,
//! at both 640x480 and 1920x1080, on this RADV/amdgpu driver. `tests/gpu_import.rs`'s
//! `imports_a_linear_dmabuf_and_reads_the_bytes_back` is now stronger
//! evidence than that CPU-only measurement: it writes a pattern into a
//! dmabuf with a plain CPU `mmap`, imports it with no acquire barrier, and
//! reads it back with an actual GPU `copy_texture_to_buffer` -- proving the
//! GPU sees the CPU's writes, not just that a second CPU `mmap` does. Both
//! are evidence about *this* driver, not a portable guarantee -- hardware
//! that actually needs the barrier would silently read garbage or stale
//! data here, not fail loudly, so this is worth re-checking on new hardware
//! rather than trusted by extension.

use crate::wgpu_ctx::WgpuContext;
use crate::GpuError;
use ash::vk;
use ghostframe_client_h264::{DmabufPlanes, PlaneDesc};
use std::os::fd::{AsRawFd, BorrowedFd, IntoRawFd};
use std::sync::Arc;

/// Shared ownership of the imported `VkDeviceMemory`.
///
/// Both plane textures' drop callbacks (see [`wrap_texture`]) hold an
/// `Arc<ImportOwner>` clone, so this is freed only once the *last* of them
/// drops -- which `Arc` guarantees cannot happen before both callbacks have
/// already run `destroy_image` (each callback destroys its image, then lets
/// its own clone drop at the end of the closure invocation). That gives the
/// order Vulkan requires -- images destroyed before the memory they were
/// bound to -- without this module needing to track or sequence it by hand.
struct ImportOwner {
    device: ash::Device,
    memory: vk::DeviceMemory,
}

impl Drop for ImportOwner {
    fn drop(&mut self) {
        // SAFETY: `memory` was allocated by this module, bound to both
        // plane images, and is freed exactly once here. See the struct doc:
        // by the time an `Arc<ImportOwner>`'s last reference drops, both
        // images' `destroy_image` calls have already completed, which is
        // the order Vulkan requires.
        unsafe {
            self.device.free_memory(self.memory, None);
        }
    }
}

/// The two textures an NV12 dmabuf becomes: luma as `R8Unorm`, chroma as
/// `Rg8Unorm` at half resolution.
///
/// # Ownership
///
/// This struct has no `Drop` of its own, and needs none: [`wrap_texture`]
/// hands each `VkImage` to wgpu with a real drop callback (unlike the no-op
/// callback `export.rs` uses, which is fine there because `ExportedImage`
/// is long-lived and owns its image directly), so wgpu's own resource
/// tracking becomes the sole owner of both images and, transitively
/// (through [`ImportOwner`]), the memory they are bound to. wgpu-core
/// defers a texture's actual destruction until the GPU work that
/// references it has retired, and keeps it alive for as long as anything
/// -- a clone of [`Self::luma`]/[`Self::chroma`], a `TextureView`, a bind
/// group -- holds a reference to it. Concretely:
///
/// - cloning either texture, building a view from it, or putting it in a
///   bind group and using that well after this value is dropped is all
///   sound: the clone/view/bind group keeps the real resource (and, through
///   it, the shared `VkDeviceMemory`) alive on its own;
/// - dropping `self` does not stall the caller and needs no
///   `device.poll(...)` first, unlike a type whose `Drop` destroys Vulkan
///   objects synchronously.
///
/// An earlier revision of this type destroyed both `VkImage`s and freed
/// their memory synchronously in its own `Drop`, with no fence and no wait
/// -- sound only if the caller held it until GPU work referencing its
/// textures had been submitted *and* waited on, a contract the plan's own
/// Task 9 reference snippet violated (it dropped the import right after
/// `draw`, which only submits). That would have been a use-after-destroy in
/// release, on a machine with no validation layers to surface it as
/// anything other than an intermittent GPU hang. This design closes that
/// class of bug structurally rather than documenting around it --
/// `tests/gpu_import.rs`'s `a_cloned_texture_outlives_the_import_and_stays_usable`
/// exercises exactly the hazard the old design had.
#[derive(Debug)]
pub struct ImportedNv12 {
    luma: wgpu::Texture,
    chroma: wgpu::Texture,
}

impl ImportedNv12 {
    pub fn luma(&self) -> &wgpu::Texture {
        &self.luma
    }

    pub fn chroma(&self) -> &wgpu::Texture {
        &self.chroma
    }
}

/// RAII cleanup for a partially-constructed import, covering the window
/// before each `vk::Image` has been handed to wgpu (see [`ImportedNv12`]'s
/// ownership doc for what takes over after that).
///
/// Exists because hand-written cleanup on each error path used to get this
/// wrong two different ways (spec review, 2026-09-23): `luma` destroyed
/// twice on one path, and the imported memory never freed on four others --
/// leaking both a dmabuf fd and a decoded surface's memory on what is the
/// *expected* path for a pitch mismatch, at 60fps. A single owner that
/// tracks exactly what has been created so far, and is cleared field by
/// field the instant ownership of that field moves elsewhere, removes the
/// need to get that bookkeeping right by hand at every `?`.
///
/// Only images, not memory: once `ImportOwner` exists it manages its own
/// lifetime via `Arc`, so a `?` before either image is wrapped just drops
/// the local `Arc<ImportOwner>` normally, which is enough.
struct PartialImport<'a> {
    device: &'a ash::Device,
    luma: Option<vk::Image>,
    chroma: Option<vk::Image>,
}

impl Drop for PartialImport<'_> {
    fn drop(&mut self) {
        // SAFETY: each handle here was created by this module; the caller
        // clears a field to `None` the instant that image is handed to
        // wgpu, so this only ever runs against images nothing else owns
        // yet.
        unsafe {
            if let Some(luma) = self.luma.take() {
                self.device.destroy_image(luma, None);
            }
            if let Some(chroma) = self.chroma.take() {
                self.device.destroy_image(chroma, None);
            }
        }
    }
}

/// Import `planes` as two single-plane textures: luma as `R8Unorm`, chroma as
/// `Rg8Unorm` at half resolution.
///
/// The fd is duplicated, so the caller keeps ownership of theirs and may drop
/// the mapped frame as soon as this returns.
pub fn import_nv12(ctx: &WgpuContext, planes: &DmabufPlanes) -> Result<ImportedNv12, GpuError> {
    // Three cases, and the middle one is the one that matters here.
    //
    // LINEAR: import, nothing to check.
    //
    // INVALID: the producer declined to say. On AMD pre-GFX9 that is the ONLY
    // value the driver can report -- modifiers begin at GFX9, so radeonsi has
    // no vocabulary for "linear" either (spec §7.1). Refusing on INVALID would
    // make this whole function dead code on exactly the hardware it was
    // written for. Spec §7.2 measured those surfaces byte-for-byte against
    // `av_hwframe_transfer_data` at 640x480 and 1920x1080 and found them
    // linear, so we proceed -- and the `check_pitch` below is what keeps that
    // from being an assumption: it compares the pitch the driver actually
    // chose against the descriptor's, and refuses the import on disagreement.
    //
    // Any other modifier is a real tiled layout, which needs
    // VK_EXT_image_drm_format_modifier to import. Say so precisely rather than
    // failing deeper in with a confusing Vulkan error.
    const DRM_FORMAT_MOD_LINEAR: u64 = 0;
    const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
    match planes.modifier {
        DRM_FORMAT_MOD_LINEAR => {}
        DRM_FORMAT_MOD_INVALID => {
            tracing::debug!(
                "dmabuf reports no modifier; attempting a linear import, \
                 guarded by the pitch check"
            );
        }
        m if !ctx.explicit_modifiers => {
            return Err(GpuError::Vulkan(format!(
                "dmabuf has modifier 0x{m:016x} but this adapter lacks \
                 VK_EXT_image_drm_format_modifier, so only LINEAR (0) or \
                 INVALID (unspecified) can be imported"
            )));
        }
        // KNOWN GAP, not yet closed: a real tiled modifier on an adapter
        // that DOES have `VK_EXT_image_drm_format_modifier` falls through to
        // here and takes the same `create_linear_image` path as LINEAR/
        // INVALID below -- there is no
        // `VkImageDrmFormatModifierExplicitCreateInfoEXT` branch that would
        // create a `DRM_FORMAT_MODIFIER_EXT`-tiled image for it. A tiled
        // buffer would therefore be misimported as linear, and `check_pitch`
        // may or may not catch it (a tiled layout's per-subresource layout
        // query has different semantics than a linear one's). Inherited
        // verbatim from the plan; no hardware in this project exposes the
        // extension, so it has never been exercised. A future milestone that
        // targets such hardware must add that branch before trusting this
        // path.
        _ => {}
    }

    ctx.with_raw(|instance, device, phys| import_inner(ctx, instance, device, phys, planes))
        .ok_or_else(|| GpuError::Vulkan("wgpu is not running on the Vulkan backend".to_string()))?
}

fn import_inner(
    ctx: &WgpuContext,
    instance: &ash::Instance,
    device: &ash::Device,
    phys: vk::PhysicalDevice,
    planes: &DmabufPlanes,
) -> Result<ImportedNv12, GpuError> {
    let luma = create_linear_image(device, planes.width, planes.height, vk::Format::R8_UNORM)?;
    let mut guard = PartialImport {
        device,
        luma: Some(luma),
        chroma: None,
    };

    let chroma = create_linear_image(
        device,
        planes.chroma_width(),
        planes.chroma_height(),
        vk::Format::R8G8_UNORM,
    )?;
    guard.chroma = Some(chroma);

    // THE CHECK, run first (see module doc). The driver chose these
    // pitches when it created each (still-unbound) image; the producer
    // chose the ones in `planes`. If they differ, every row after the
    // first would read from the wrong offset and the image would shear.
    check_pitch(device, luma, "luma", planes.luma)?;
    check_pitch(device, chroma, "chroma", planes.chroma)?;

    // SAFETY: both images are live, just created above.
    let luma_req = unsafe { device.get_image_memory_requirements(luma) };
    // SAFETY: as above.
    let chroma_req = unsafe { device.get_image_memory_requirements(chroma) };

    // Single-sourced: `total` is computed once, here, and is the ONLY value
    // used both to validate the planes below and to size the allocation
    // further down -- never `planes.size` read a second time at the
    // allocation site. That is deliberate, not stylistic: an earlier
    // revision read `planes.size` directly in `check_plane_fits` but
    // through a separately-initialised `total` binding at the
    // `.allocation_size(...)` call, and a `debug_assert_eq!` between them
    // to guard against the two drifting apart. Measured directly
    // (2026-09-23, mutation testing on real hardware): a mutation that
    // corrupted only the `total` initialiser (e.g. `planes.size / 2`) sailed
    // straight past that assert-based guard in every profile that disables
    // debug assertions, AND was never observable through Vulkan itself --
    // RADV's dma-buf import uses the fd's own real backing size regardless
    // of what `allocationSize` claims, tried at both a generous and a
    // byte-tight backing buffer. Single-sourcing removes the seam a
    // mutation could exploit: any wrong value here also feeds
    // `check_plane_fits`, which then genuinely fails in every build
    // profile, because the planes really would no longer fit the size
    // being claimed -- a real rejection, not an assertion hoping to catch
    // a hypothetical drift.
    let total = planes.size;

    // `lseek(fd, 0, SEEK_END)` is the fd's OWN account of its size, not a
    // second read of `planes.size` -- the dmabuf's producer/kernel, not
    // this module's caller. `check_plane_fits` below can only validate the
    // descriptor against itself (both `total` and the per-plane
    // requirements ultimately trace back to what the caller handed in); if
    // `planes.size` overstates the fd's real size, self-consistent
    // arithmetic never catches that, only an external source of truth does.
    // Position is restored afterward defensively, though nothing else in
    // this path performs sequential I/O on this fd.
    // SAFETY: `planes.fd` is a live fd owned by the caller for the duration
    // of this call (the same fd `BorrowedFd::borrow_raw` below relies on);
    // `lseek` only queries/repositions the file offset and touches no data.
    let real_size = unsafe { libc::lseek(planes.fd, 0, libc::SEEK_END) };
    if real_size < 0 {
        return Err(GpuError::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: as above; restoring the offset is a courtesy to the caller,
    // who still owns this fd after this function returns.
    unsafe {
        libc::lseek(planes.fd, 0, libc::SEEK_SET);
    }
    let real_size = real_size as u64;
    if real_size < total {
        return Err(GpuError::Vulkan(format!(
            "dmabuf reports {real_size} bytes via lseek, but the descriptor \
             claims {total}; importing would let the allocation exceed the \
             fd's real backing size"
        )));
    }

    // The allocation is sized to `total` (== `planes.size`, the driver's own
    // figure for the dmabuf object, now cross-checked against the fd's real
    // size above), not to either image's `memoryRequirements.size` --
    // verify each plane actually fits inside it before binding anything.
    // Validation layers catch an out-of-bounds bind; production drivers
    // often do not (measured: this one doesn't), and the result is the GPU
    // sampling past the end of the dmabuf.
    check_plane_fits("luma", planes.luma.offset, luma_req.size, total)?;
    check_plane_fits("chroma", planes.chroma.offset, chroma_req.size, total)?;

    // Both planes bind into the same allocation at their descriptor
    // offsets, which Vulkan only permits at a multiple of each image's own
    // `memoryRequirements.alignment`. Neither offset is hardcoded to 0 by
    // anything here -- `tests/gpu_import.rs` deliberately places both luma
    // and chroma at nonzero offsets so a bug that ignored either offset
    // (e.g. a bind hardcoded to 0) shows up as wrong pixels, not just as an
    // unexercised alignment check.
    check_offset_alignment("luma", planes.luma.offset, luma_req.alignment)?;
    check_offset_alignment("chroma", planes.chroma.offset, chroma_req.alignment)?;

    // NOT DONE YET: `vkAllocateMemory` importing a dmabuf performs a kernel
    // GEM import (`drmPrimeFDToHandle`) -- hundreds of microseconds on AMD,
    // paid on every call here -- while VA-API actually recycles a small,
    // fixed pool of decode surfaces, so the same few dmabufs get
    // re-imported over and over across frames. A cache keyed on buffer
    // identity (`fstat`'s `st_ino`, NOT the fd number, which gets recycled)
    // could turn that into one import per pool entry instead of one per
    // frame. Left undone: Task 12 measures whether the per-frame cost
    // actually matters before this complexity is worth taking on.

    // Duplicate: vkImportMemoryFdKHR takes ownership of the fd it is given,
    // while the caller's `MappedFrame` still owns the original.
    // SAFETY: `planes.fd` is a live fd owned by the caller for the duration
    // of this call, which is all `borrow_raw`'s contract requires --
    // `try_clone_to_owned` immediately turns it into an independent,
    // freshly-duplicated `OwnedFd` (`F_DUPFD_CLOEXEC`) rather than holding
    // onto the borrow.
    let fd = unsafe { BorrowedFd::borrow_raw(planes.fd) }
        .try_clone_to_owned()
        .map_err(GpuError::Io)?;

    let ext_mem_fd = ctx.ext_memory_fd(instance, device);

    // What memory types can back this fd?
    let mut fd_props = vk::MemoryFdPropertiesKHR::default();
    // SAFETY: `fd` is a live dmabuf fd; `fd_props` is a valid out-param.
    unsafe {
        ext_mem_fd.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            fd.as_raw_fd(),
            &mut fd_props,
        )
    }
    .map_err(|e| GpuError::Vulkan(format!("get_memory_fd_properties: {e}")))?;

    let type_bits =
        fd_props.memory_type_bits & luma_req.memory_type_bits & chroma_req.memory_type_bits;
    let mem_props = ctx.memory_properties(instance, phys);
    let mem_type_index = find_memory_type_index(&mem_props, type_bits).ok_or_else(|| {
        GpuError::Vulkan("no memory type can back this dmabuf and both plane images".to_string())
    })?;

    let mut import_info = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(fd.as_raw_fd());
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(total)
        .memory_type_index(mem_type_index)
        .push_next(&mut import_info);

    // SAFETY: `alloc_info` imports exactly the fd above, sized to cover both
    // planes (checked above), at a memory type the fd itself reported as
    // compatible. On success Vulkan owns the fd (module doc: "the imported
    // fd is not ours to close").
    let memory = unsafe { device.allocate_memory(&alloc_info, None) }
        .map_err(|e| GpuError::Vulkan(format!("allocate_memory (import): {e}")))?;

    // Ownership of `fd` just passed to the Vulkan implementation.
    // `into_raw_fd` names that handover directly -- "transfers ownership of
    // the underlying file descriptor to the caller" -- where `mem::forget`
    // would only relinquish it as a side effect of skipping `OwnedFd`'s
    // destructor. Discarding the return value is deliberate: nothing here
    // needs the raw number again.
    let _ = fd.into_raw_fd();

    // From here, `memory`'s lifetime is `owner`'s: a plain `Arc::new`, no
    // manual bookkeeping needed. If this function returns `Err` before
    // either image is wrapped, the local `owner` binding (and the clone
    // handed to whichever `wrap_texture` call is in flight) just drops
    // normally, running `ImportOwner::drop` once the refcount reaches zero.
    let owner = Arc::new(ImportOwner {
        device: device.clone(),
        memory,
    });

    // SAFETY: images and memory are live; both planes were checked above
    // (`check_plane_fits`, `check_offset_alignment`) to fit inside `memory`
    // at their respective offsets.
    unsafe { device.bind_image_memory(luma, memory, planes.luma.offset) }
        .map_err(|e| GpuError::Vulkan(format!("bind_image_memory (luma): {e}")))?;
    // SAFETY: as above.
    unsafe { device.bind_image_memory(chroma, memory, planes.chroma.offset) }
        .map_err(|e| GpuError::Vulkan(format!("bind_image_memory (chroma): {e}")))?;

    // Both plane images are bound into the same allocation, and their byte
    // ranges may overlap in principle -- nothing here re-derives one
    // plane's extent from the other's offset to prove they don't. That is
    // sound only because both images are read-only from the GPU's
    // perspective (`SAMPLED | TRANSFER_SRC` below, never a render or copy
    // target): two textures may safely alias memory they only ever read.
    // Writing to either would turn the aliasing into a real hazard.

    let luma_tex = wrap_texture(
        &ctx.device,
        luma,
        Arc::clone(&owner),
        planes.width,
        planes.height,
        wgpu::TextureFormat::R8Unorm,
        "ghostframe-imported-luma",
    )?;
    // Ownership of `luma` (the raw handle) has moved into `luma_tex`'s drop
    // callback; `guard` must stop tracking it so it is not destroyed a
    // second time if the chroma wrap below fails.
    guard.luma = None;

    let chroma_tex = wrap_texture(
        &ctx.device,
        chroma,
        Arc::clone(&owner),
        planes.chroma_width(),
        planes.chroma_height(),
        wgpu::TextureFormat::Rg8Unorm,
        "ghostframe-imported-chroma",
    )?;
    guard.chroma = None;

    Ok(ImportedNv12 {
        luma: luma_tex,
        chroma: chroma_tex,
    })
}

fn create_linear_image(
    device: &ash::Device,
    width: u32,
    height: u32,
    format: vk::Format,
) -> Result<vk::Image, GpuError> {
    let mut external = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::LINEAR)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external);

    // SAFETY: `info` is fully populated and `device` is live.
    unsafe { device.create_image(&info, None) }
        .map_err(|e| GpuError::Vulkan(format!("create_image (import, {format:?}): {e}")))
}

/// The pitch check that makes an un-promised linear import safe (see module
/// doc). Run against a freshly created, still-unbound image -- deliberately,
/// see the SAFETY note below.
fn check_pitch(
    device: &ash::Device,
    image: vk::Image,
    what: &'static str,
    plane: PlaneDesc,
) -> Result<(), GpuError> {
    let subresource = vk::ImageSubresource {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        array_layer: 0,
    };
    // SAFETY: `image` is live and was created with
    // `vk::ImageTiling::LINEAR`, which is one of the two tilings
    // `vkGetImageSubresourceLayout` is defined for (the other is
    // `DRM_FORMAT_MODIFIER_EXT`, which `export.rs` queries the same way).
    // The image need not be bound to memory: the subresource layout is a
    // property of the image alone, fixed at `vkCreateImage` time, not of
    // memory bound to it afterwards -- there is no valid-usage requirement
    // to the contrary. (An earlier revision of this function claimed both
    // "must be bound" and "LINEAR is the only tiling this is defined for";
    // both were wrong, and that false invariant is why this check used to
    // run after binding and importing instead of before either.) Calling it
    // here, before any fd is duplicated or memory imported, means a
    // rejected frame costs exactly the two `vkCreateImage` calls already
    // made and nothing else.
    let layout = unsafe { device.get_image_subresource_layout(image, subresource) };

    // `layout.offset` is where this subresource starts *within this
    // single-plane image itself* -- not the dmabuf's plane offset, which is
    // carried separately and applied via `bind_image_memory`'s offset
    // argument. Each image here has exactly one subresource, so a nonzero
    // value would mean the driver put unexpected padding before pixel data
    // that the rest of this module's offset arithmetic does not account
    // for.
    if layout.offset != 0 {
        return Err(GpuError::Vulkan(format!(
            "{what} plane: driver's linear image reports a nonzero subresource \
             offset ({}), which this import path does not account for",
            layout.offset
        )));
    }

    // `array_pitch`/`depth_pitch` are deliberately left unchecked: the
    // Vulkan spec leaves both undefined when `arrayLayers == 1` /
    // `depth == 1`, which is exactly what `create_linear_image` requests
    // for every image this module creates.
    if layout.row_pitch != plane.pitch {
        return Err(GpuError::PitchMismatch {
            plane: what,
            dmabuf: plane.pitch,
            driver: layout.row_pitch,
        });
    }
    Ok(())
}

/// `offset + required_size` must not exceed `total` (the dmabuf object's
/// own size) -- otherwise binding this plane at `offset` would let the GPU
/// read past the end of the buffer the driver actually allocated.
fn check_plane_fits(
    what: &'static str,
    offset: u64,
    required_size: u64,
    total: u64,
) -> Result<(), GpuError> {
    match offset.checked_add(required_size) {
        Some(end) if end <= total => Ok(()),
        Some(end) => Err(GpuError::Vulkan(format!(
            "{what} plane does not fit: offset {offset} + the driver's required \
             {required_size} bytes = {end}, but the dmabuf object is only {total} bytes"
        ))),
        None => Err(GpuError::Vulkan(format!(
            "{what} plane offset {offset} plus its required size {required_size} overflows u64"
        ))),
    }
}

/// `offset` must be a multiple of `alignment` -- `vkBindImageMemory`'s
/// valid-usage requirement, checked here so a violation fails with the two
/// numbers in hand rather than as an opaque Vulkan error deeper in.
fn check_offset_alignment(what: &'static str, offset: u64, alignment: u64) -> Result<(), GpuError> {
    if !offset.is_multiple_of(alignment) {
        return Err(GpuError::Vulkan(format!(
            "{what} plane offset {offset} is not a multiple of the required alignment {alignment}"
        )));
    }
    Ok(())
}

/// Wraps `image` as a wgpu texture, transferring its ownership -- and a
/// share of `owner`'s -- to wgpu's own resource tracking. See
/// [`ImportedNv12`]'s ownership doc for why this, not a no-op callback plus
/// a hand-written `Drop`, is what makes the returned texture (and anything
/// cloned or built from it) safe to use after the caller drops
/// [`ImportedNv12`].
fn wrap_texture(
    wgpu_device: &wgpu::Device,
    image: vk::Image,
    owner: Arc<ImportOwner>,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    label: &'static str,
) -> Result<wgpu::Texture, GpuError> {
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };

    // Runs exactly once, whenever wgpu-core decides this texture's
    // underlying resource is no longer referenced by anything (a clone, a
    // view, a bind group) AND the GPU work that used it has retired --
    // `wgpu_hal::vulkan::Device::destroy_texture` only calls this because
    // `memory` is `TextureMemory::External` below, so wgpu-hal itself never
    // touches `image` or the memory it is bound to.
    let drop_callback: wgpu_hal::DropCallback = Box::new(move || {
        // SAFETY: `image` was handed to wgpu-hal below with `Some` drop
        // callback, which per `texture_from_raw`'s contract means "`vk_image`
        // must be valid until the callback is called" -- i.e. this closure,
        // not wgpu-hal, owns destroying it, and wgpu-hal guarantees it is
        // not called before `image` is no longer in use.
        unsafe {
            owner.device.destroy_image(image, None);
        }
        // `owner` (this closure's own `Arc<ImportOwner>` clone) drops here,
        // after `destroy_image` above -- seeing this comment is the whole
        // point of writing the drop explicitly rather than relying on it
        // happening implicitly at the closure's end.
        drop(owner);
    });

    // SAFETY: `image` is a live VkImage bound to memory this `Arc` (moved
    // into `drop_callback` above) keeps alive for exactly as long as
    // needed. `TextureMemory::External` tells wgpu-hal the memory is not
    // its to free -- `drop_callback` does that, transitively, by dropping
    // its `Arc<ImportOwner>` clone. `UNINITIALIZED` is the image's true
    // layout: it was just created (see the module doc's note on why
    // skipping a foreign-queue acquire is safe here).
    let hal_texture = unsafe {
        wgpu_device
            .as_hal::<wgpu_hal::api::Vulkan>()
            .map(|hal_device| {
                hal_device.texture_from_raw(
                    image,
                    &wgpu_hal::TextureDescriptor {
                        label: Some(label),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC,
                        memory_flags: wgpu_hal::MemoryFlags::empty(),
                        view_formats: Vec::new(),
                    },
                    Some(drop_callback),
                    wgpu_hal::vulkan::TextureMemory::External,
                )
            })
    }
    .ok_or_else(|| GpuError::Vulkan("wgpu is not running on the Vulkan backend".to_string()))?;

    // SAFETY: `hal_texture` was just built from a valid VkImage matching this
    // descriptor, and `UNINITIALIZED` reflects its true current layout.
    Ok(unsafe {
        wgpu_device.create_texture_from_hal::<wgpu_hal::api::Vulkan>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            },
            wgpu::TextureUses::UNINITIALIZED,
        )
    })
}

/// Unlike `export.rs`'s namesake, this ignores memory property flags
/// entirely: importing a dmabuf accepts whatever memory type the fd itself
/// reports as compatible (`fd_props.memory_type_bits`, already intersected
/// into `type_bits` by the caller) -- there is no DEVICE_LOCAL-vs.-
/// HOST_VISIBLE choice to make for an import the way there is for a fresh
/// export allocation, so there is nothing to prefer among the candidates.
fn find_memory_type_index(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
) -> Option<u32> {
    (0..props.memory_type_count).find(|i| type_bits & (1 << i) != 0)
}
