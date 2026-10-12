//! DMA-BUF pixel readback via mmap and Vulkan DMA-BUF import.
//!
//! M1: simple mmap of the DMA-BUF fd to read raw pixels.
//! VulkanReadback: imports DMA-BUF fd via Vulkan extensions for GPU-accelerated readback.

use std::io;
use std::ops::{Deref, Range};
use std::os::unix::io::RawFd;
use std::sync::{Mutex, MutexGuard};

// ---------------------------------------------------------------------------
// mmap readback with a persistent mapping and recycled buffers
// ---------------------------------------------------------------------------

/// Identity of what is mapped. Keyed by the file's `(st_dev, st_ino)`, never
/// by the fd number: an fd number can be closed and reused for a different
/// dma-buf, an inode cannot be recycled while the cached mapping holds a
/// reference to the underlying file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct MapKey {
    dev: u64,
    ino: u64,
    stride: u32,
    height: u32,
}

struct Mapping {
    ptr: *const u8,
    len: usize,
    key: MapKey,
}

// SAFETY: the mapping is a plain read-only region owned by this value; it is
// only ever touched through `&self`/`Drop`, and the cache holding it sits
// behind a mutex.
unsafe impl Send for Mapping {}

impl Mapping {
    fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` is a live PROT_READ mapping of `len` bytes owned by
        // `self` and unmapped only in `Drop`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: ptr/len are exactly what mmap returned/was given.
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

/// A recycled pixel buffer. `stamp` says "my bytes are a complete copy of the
/// mapping for `key` as of readback call number `seq`"; it is what lets the
/// row-mode API prove continuity.
struct Buf {
    data: Vec<u8>,
    stamp: Option<(MapKey, u64)>,
}

/// Maximum number of idle buffers kept in the process-wide pool.
const POOL_CAP: usize = 2;

struct State {
    /// Single-entry mapping cache. Keeps the dma-buf alive (and therefore its
    /// GPU memory) until it is replaced, [`release_cached_mapping`] is
    /// called, or a mapping attempt for a cacheable buffer fails, even if the
    /// caller has long since closed its fd.
    map: Option<Mapping>,
    pool: Vec<Buf>,
    /// Incremented by every readback call (success or not, including early
    /// failures) and by [`invalidate`]. A buffer is continuous with the next
    /// call only if its stamp carries the current value.
    seq: u64,
}

/// Process-wide state. One mapping and one small pool for the whole process,
/// not per thread: the bridge runs on a multi-thread runtime, and a
/// thread-local would pin a mapping and buffers on every worker that ever
/// read. Readbacks are serial per bridge, so the lock is uncontended; it is
/// held across the copy. Nothing in the locked regions drops a
/// [`ReadbackFrame`] (its `Drop` takes this lock too).
static STATE: Mutex<State> = Mutex::new(State {
    map: None,
    pool: Vec::new(),
    seq: 0,
});

fn lock_state() -> MutexGuard<'static, State> {
    // A panic while holding the lock cannot leave the state inconsistent
    // enough to matter (worst case a stale cache entry that the key check
    // rejects, and continuity stamps that fail to match), so recover from
    // poisoning.
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Drop the pinned mapping (releasing the dma-buf reference it holds) and
/// the pooled buffers. Call when capture stops or on a mode switch away from
/// the tile codec, so an idle process does not keep a frame's worth of GPU
/// memory pinned and ~2 frames of host memory allocated. The next row-mode
/// readback is a full copy.
pub fn release_cached_mapping() {
    let mut st = lock_state();
    st.map = None;
    st.pool.clear();
    st.seq = st.seq.wrapping_add(1);
}

/// A frame of BGRA pixels read back from a dma-buf: `stride * height` bytes,
/// derefs to `[u8]`. Dropping it returns the allocation to the process-wide
/// pool.
pub struct ReadbackFrame {
    buf: Option<Buf>,
}

impl ReadbackFrame {
    fn new(buf: Buf) -> Self {
        Self { buf: Some(buf) }
    }
}

impl Deref for ReadbackFrame {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match &self.buf {
            Some(b) => &b.data,
            None => &[],
        }
    }
}

impl Drop for ReadbackFrame {
    fn drop(&mut self) {
        if let Some(buf) = self.buf.take() {
            let mut st = lock_state();
            if st.pool.len() >= POOL_CAP {
                st.pool.remove(0);
            }
            st.pool.push(buf);
        }
    }
}

/// Declare that the caller dropped (did not read back) a frame, or otherwise
/// lost continuity with the producer. The next [`readback_dmabuf_rows`] call
/// does a full copy.
pub fn invalidate() {
    let mut st = lock_state();
    st.seq = st.seq.wrapping_add(1);
}

/// `DMA_BUF_IOCTL_SYNC` bracket around CPU reads. Errors are ignored, as in
/// `ghostframe-xdaemon/src/drm_capture.rs`: it is a best-effort cache
/// maintenance step and plain system memory is coherent without it. Returns
/// whether the ioctl succeeded so the caller can keep START/END balanced
/// (no END after a failed START).
/// NOTE: the pre-pooling implementation issued no sync at all; this is new.
fn dmabuf_sync(fd: RawFd, end: bool) -> bool {
    #[repr(C)]
    struct DmaBufSync {
        flags: u64,
    }
    // _IOW('b', 0, struct dma_buf_sync)
    const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x40086200;
    const DMA_BUF_SYNC_READ: u64 = 1 << 0;
    const DMA_BUF_SYNC_START: u64 = 0;
    const DMA_BUF_SYNC_END: u64 = 1 << 2;
    let mut req = DmaBufSync {
        flags: (if end {
            DMA_BUF_SYNC_END
        } else {
            DMA_BUF_SYNC_START
        }) | DMA_BUF_SYNC_READ,
    };
    // SAFETY: plain ioctl with a valid pointer to a 8-byte struct.
    unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &mut req) == 0 }
}

/// `(key, size, cacheable)`. A file that reports `st_size == 0` is not
/// cacheable: some exporters share one inode across distinct buffers, so
/// `(st_dev, st_ino)` is not a unique identity for them and a cached mapping
/// could be served for the wrong buffer. Those are mapped per call.
fn map_key_and_size(fd: RawFd, height: u32, stride: u32) -> io::Result<(MapKey, usize, bool)> {
    // Widened on purpose: the old `(stride * height) as usize` multiplied in
    // u32 and wrapped (release) / panicked (debug) above 4 GiB.
    let size = (stride as u64)
        .checked_mul(height as u64)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame size overflows"))?;
    // SAFETY: zeroed stat is a valid out-parameter.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Best effort: mapping past the end of a file-backed fd succeeds and then
    // SIGBUSes on access, so refuse a file that reports a smaller size. This
    // does not cover a file that reports no size (st_size == 0) or one that
    // is truncated after this check; those can still fault on access.
    if st.st_size > 0 && (st.st_size as u64) < size as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dma-buf smaller than stride * height",
        ));
    }
    Ok((
        MapKey {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            stride,
            height,
        },
        size,
        st.st_size > 0,
    ))
}

/// Sort, clamp to `height`, drop empty and merge overlapping/adjacent ranges.
fn merge_rows(rows: &[Range<u32>], height: u32) -> Vec<(usize, usize)> {
    let mut v: Vec<(usize, usize)> = rows
        .iter()
        .map(|r| (r.start.min(height) as usize, r.end.min(height) as usize))
        .filter(|(a, b)| a < b)
        .collect();
    v.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(v.len());
    for (a, b) in v {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

fn map_ro(fd: RawFd, size: usize, key: MapKey) -> io::Result<Mapping> {
    // SAFETY: fresh read-only shared mapping of `size` bytes.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(Mapping {
        ptr: ptr as *const u8,
        len: size,
        key,
    })
}

/// Copy `src` (all of it, or only `rows` of `stride`-byte pixel rows when
/// `row_mode`) into `buf`, bracketed by DMA_BUF_IOCTL_SYNC. No END is issued
/// if START failed.
fn copy_synced(fd: RawFd, src: &[u8], buf: &mut Buf, row_mode: Option<(&[Range<u32>], u32, u32)>) {
    let started = dmabuf_sync(fd, false);
    match row_mode {
        Some((rows, height, stride)) => {
            debug_assert_eq!(buf.data.len(), src.len());
            for (a, b) in merge_rows(rows, height) {
                let (lo, hi) = (a * stride as usize, b * stride as usize);
                buf.data[lo..hi].copy_from_slice(&src[lo..hi]);
            }
        }
        None => {
            // No zero-fill: clear + extend writes each byte once.
            buf.data.clear();
            buf.data.reserve(src.len());
            buf.data.extend_from_slice(src);
        }
    }
    if started {
        dmabuf_sync(fd, true);
    }
}

fn readback_impl(
    fd: RawFd,
    height: u32,
    stride: u32,
    rows: Option<&[Range<u32>]>,
) -> io::Result<ReadbackFrame> {
    let mut st = lock_state();
    // Bump the sequence before anything can fail: every call, including one
    // that errors out early (bad fd, size overflow, st_size rejection), breaks
    // continuity for the next rows call.
    let prev_seq = st.seq;
    st.seq = prev_seq.wrapping_add(1);
    let new_seq = st.seq;

    let (key, size, cacheable) = map_key_and_size(fd, height, stride)?;
    if size == 0 {
        return Ok(ReadbackFrame::new(Buf {
            data: Vec::new(),
            stamp: None,
        }));
    }

    if !cacheable {
        // Map for this call only; the cache (if any) is left alone, and the
        // result is never stamped, so the next rows call is a full copy.
        let mapping = map_ro(fd, size, key)?;
        let mut buf = Buf {
            data: st.pool.pop().map(|b| b.data).unwrap_or_default(),
            stamp: None,
        };
        copy_synced(fd, mapping.bytes(), &mut buf, None);
        drop(mapping);
        return Ok(ReadbackFrame::new(buf));
    }

    if st.map.as_ref().map(|m| m.key) != Some(key) {
        // Unmap the old one before mapping the new one; on failure the
        // cache stays empty.
        st.map = None;
        st.map = Some(map_ro(fd, size, key)?);
    }

    // Row mode needs the buffer that is a complete copy as of the
    // immediately preceding call for this very key.
    let reuse = match rows {
        Some(_) => st
            .pool
            .iter()
            .position(|b| b.stamp == Some((key, prev_seq)))
            .map(|i| st.pool.swap_remove(i)),
        None => None,
    };
    let row_mode = reuse.is_some();
    let mut buf = reuse.unwrap_or_else(|| Buf {
        data: st.pool.pop().map(|b| b.data).unwrap_or_default(),
        stamp: None,
    });
    buf.stamp = None;

    let src = st.map.as_ref().map(|m| m.bytes()).unwrap_or(&[]);
    let mode = match (row_mode, rows) {
        (true, Some(r)) => Some((r, height, stride)),
        _ => None,
    };
    copy_synced(fd, src, &mut buf, mode);

    buf.stamp = Some((key, new_seq));
    Ok(ReadbackFrame::new(buf))
}

/// Read pixels from a DMA-BUF file descriptor via mmap.
///
/// Returns a [`ReadbackFrame`] (derefs to `[u8]`) of BGRA pixel data,
/// exactly `stride * height` bytes. The caller is responsible for the fd's
/// lifetime (this function does not close it), but note that a process-wide
/// cache keeps the mapping, and with it the dma-buf, alive until a different
/// buffer is read or [`release_cached_mapping`] is called. Every read is bracketed by
/// `DMA_BUF_IOCTL_SYNC` (START|READ .. END|READ).
pub fn readback_dmabuf(
    fd: RawFd,
    _width: u32,
    height: u32,
    stride: u32,
) -> io::Result<ReadbackFrame> {
    readback_impl(fd, height, stride, None)
}

/// Like [`readback_dmabuf`], but copies only `rows` (half-open pixel-row
/// ranges, clamped to `height`, merged) when it can prove that is enough.
///
/// The shortcut is taken only if the recycled buffer being reused is the very
/// buffer returned by the immediately preceding readback call (process-wide)
/// (full or rows), that call covered the whole frame (a full copy, or a rows
/// call that itself satisfied this rule) and the mapping key (inode, stride,
/// height) is unchanged. Otherwise this is a full copy. The previous frame
/// must have been dropped so its buffer is back in the pool; a frame still
/// held forces a full copy into a different buffer. The state is
/// process-wide: there must be one logical reader per process (two readers of
/// the same buffer would each count the other's call as the previous one).
///
/// CONTRACT for the caller: `rows` must cover every pixel row the producer
/// changed since the previous readback call. If the caller
/// skips a readback (drops a frame, errors out, switches fd and back), the
/// next dirty set is relative to the skipped frame and would leave stale
/// rows in the buffer. Call [`invalidate`] whenever a frame was dropped.
/// Failed readbacks and key changes invalidate automatically; rows the caller
/// did not list are NOT refreshed, even if they changed.
pub fn readback_dmabuf_rows(
    fd: RawFd,
    _width: u32,
    height: u32,
    stride: u32,
    rows: &[Range<u32>],
) -> io::Result<ReadbackFrame> {
    readback_impl(fd, height, stride, Some(rows))
}

// ---------------------------------------------------------------------------
// Vulkan DMA-BUF import readback
// ---------------------------------------------------------------------------

use ash::vk;
use std::ffi::CStr;

/// Vulkan-based DMA-BUF pixel readback.
///
/// Holds a Vulkan device with `VK_KHR_external_memory_fd` and
/// `VK_EXT_external_memory_dma_buf` extensions enabled.  The [`readback`]
/// method imports a DMA-BUF fd, copies pixels to a host-visible staging
/// buffer, and returns the pixel data.
pub struct VulkanReadback {
    _entry: ash::Entry,
    instance: ash::Instance,
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    command_pool: vk::CommandPool,
    device_name: String,
}

impl VulkanReadback {
    /// Create a new `VulkanReadback`, initializing Vulkan with the required
    /// DMA-BUF import extensions.
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        unsafe { Self::new_inner() }
    }

    unsafe fn new_inner() -> Result<Self, Box<dyn std::error::Error>> {
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

        // Pick the first device that supports both required extensions.
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

        let props = instance.get_physical_device_properties(physical_device);
        let device_name = CStr::from_ptr(props.device_name.as_ptr())
            .to_string_lossy()
            .into_owned();

        // --- Queue family ---
        let queue_families = instance.get_physical_device_queue_family_properties(physical_device);
        let queue_family_index = queue_families
            .iter()
            .position(|qf| qf.queue_flags.contains(vk::QueueFlags::TRANSFER))
            .or_else(|| {
                queue_families
                    .iter()
                    .position(|qf| qf.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            })
            .ok_or("no suitable queue family")? as u32;

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

        Ok(Self {
            _entry: entry,
            instance,
            physical_device,
            device,
            queue,
            command_pool,
            device_name,
        })
    }

    /// The name of the selected Vulkan physical device.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Import a DMA-BUF fd and read back its pixels into a `Vec<u8>`.
    ///
    /// The returned buffer is `stride * height` bytes of BGRA pixel data.
    /// The fd is NOT consumed (not closed) by this call.
    pub fn readback(
        &self,
        fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        unsafe { self.readback_inner(fd, width, height, stride) }
    }

    unsafe fn readback_inner(
        &self,
        fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let size = (stride * height) as vk::DeviceSize;

        // --- Import DMA-BUF fd as VkDeviceMemory ---
        // We must dup the fd because Vulkan takes ownership of it.
        let dup_fd = libc::dup(fd);
        if dup_fd < 0 {
            return Err(io::Error::last_os_error().into());
        }

        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(dup_fd);

        // Find a memory type suitable for the image.
        let mem_props = self
            .instance
            .get_physical_device_memory_properties(self.physical_device);

        // We need device-local memory for the imported DMA-BUF image.
        let image_mem_type_index = find_memory_type(
            &mem_props,
            u32::MAX, // accept any type
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .or_else(|| {
            // Fall back to any memory type if no device-local found.
            find_memory_type(&mem_props, u32::MAX, vk::MemoryPropertyFlags::empty())
        })
        .ok_or("no suitable memory type for DMA-BUF import")?;

        let mut alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(size)
            .memory_type_index(image_mem_type_index);
        alloc_info = alloc_info.push_next(&mut import_info);

        let imported_memory = self.device.allocate_memory(&alloc_info, None)?;

        // --- Create VkImage backed by imported memory ---
        let mut ext_mem_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::PREINITIALIZED)
            .push_next(&mut ext_mem_info);

        let image = self.device.create_image(&image_ci, None)?;
        self.device.bind_image_memory(image, imported_memory, 0)?;

        // --- Allocate host-visible staging buffer ---
        let buffer_ci = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);

        let staging_buffer = self.device.create_buffer(&buffer_ci, None)?;
        let buf_mem_reqs = self.device.get_buffer_memory_requirements(staging_buffer);

        let staging_mem_type_index = find_memory_type(
            &mem_props,
            buf_mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or("no host-visible memory type for staging buffer")?;

        let staging_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(buf_mem_reqs.size)
            .memory_type_index(staging_mem_type_index);

        let staging_memory = self.device.allocate_memory(&staging_alloc, None)?;
        self.device
            .bind_buffer_memory(staging_buffer, staging_memory, 0)?;

        // --- Record command buffer ---
        let cmd_alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);

        let cmd_bufs = self.device.allocate_command_buffers(&cmd_alloc)?;
        let cmd = cmd_bufs[0];

        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        self.device.begin_command_buffer(cmd, &begin_info)?;

        // Transition image: PREINITIALIZED -> TRANSFER_SRC_OPTIMAL
        let barrier = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::PREINITIALIZED)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            })
            .src_access_mask(vk::AccessFlags::HOST_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ);

        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::HOST,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier],
        );

        // Copy image -> staging buffer
        let region = vk::BufferImageCopy {
            buffer_offset: 0,
            buffer_row_length: 0,   // tightly packed
            buffer_image_height: 0, // tightly packed
            image_subresource: vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            },
            image_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
            image_extent: vk::Extent3D {
                width,
                height,
                depth: 1,
            },
        };

        self.device.cmd_copy_image_to_buffer(
            cmd,
            image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            staging_buffer,
            &[region],
        );

        self.device.end_command_buffer(cmd)?;

        // --- Submit + wait ---
        let submit_info = vk::SubmitInfo::default().command_buffers(&cmd_bufs);
        self.device
            .queue_submit(self.queue, &[submit_info], vk::Fence::null())?;
        self.device.queue_wait_idle(self.queue)?;

        // --- Map staging buffer and copy pixels ---
        let data_ptr =
            self.device
                .map_memory(staging_memory, 0, size, vk::MemoryMapFlags::empty())?;

        let mut pixels = vec![0u8; size as usize];
        std::ptr::copy_nonoverlapping(data_ptr as *const u8, pixels.as_mut_ptr(), size as usize);
        self.device.unmap_memory(staging_memory);

        // --- Cleanup ---
        self.device
            .free_command_buffers(self.command_pool, &cmd_bufs);
        self.device.destroy_buffer(staging_buffer, None);
        self.device.free_memory(staging_memory, None);
        self.device.destroy_image(image, None);
        self.device.free_memory(imported_memory, None);

        Ok(pixels)
    }
}

impl Drop for VulkanReadback {
    fn drop(&mut self) {
        unsafe {
            self.device.device_wait_idle().ok();
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// Find a memory type index matching the given requirements.
fn find_memory_type(
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
mod tests {
    use super::*;

    /// The readback state is process-wide, so tests that read must not
    /// interleave.
    fn serial() -> MutexGuard<'static, ()> {
        static L: Mutex<()> = Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cached_key() -> Option<MapKey> {
        lock_state().map.as_ref().map(|m| m.key)
    }

    #[test]
    fn readback_from_memfd() {
        let _g = serial();
        let width = 64u32;
        let height = 64u32;
        let stride = width * 4;
        let size = (stride * height) as usize;

        // Create memfd, write known pixel data
        let name = std::ffi::CString::new("test-dmabuf").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        assert!(fd >= 0);
        unsafe { libc::ftruncate(fd, size as i64) };

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, size) };
        for pixel in slice.chunks_exact_mut(4) {
            pixel[0] = 0; // B
            pixel[1] = 0; // G
            pixel[2] = 255; // R
            pixel[3] = 255; // A
        }
        unsafe { libc::munmap(ptr, size) };

        let pixels = readback_dmabuf(fd, width, height, stride).unwrap();
        assert_eq!(pixels.len(), size);
        assert_eq!(pixels[0], 0); // B
        assert_eq!(pixels[2], 255); // R

        unsafe { libc::close(fd) };
    }

    // ---- shared helpers for the readback benchmark / tests ----------------

    /// A shareable buffer plus a writable "producer" mapping of it.
    struct TestBuf {
        fd: i32,
        map: *mut u8,
        size: usize,
    }

    impl TestBuf {
        fn fill(&self, f: impl Fn(usize) -> u8) {
            let s = unsafe { std::slice::from_raw_parts_mut(self.map, self.size) };
            for (i, b) in s.iter_mut().enumerate() {
                *b = f(i);
            }
        }
        fn bytes(&self) -> &[u8] {
            unsafe { std::slice::from_raw_parts(self.map, self.size) }
        }
        fn bytes_mut(&mut self) -> &mut [u8] {
            unsafe { std::slice::from_raw_parts_mut(self.map, self.size) }
        }
    }

    impl Drop for TestBuf {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.map as *mut libc::c_void, self.size);
                libc::close(self.fd);
            }
        }
    }

    fn map_rw(fd: i32, size: usize) -> *mut u8 {
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        assert_ne!(p, libc::MAP_FAILED, "producer mmap");
        p as *mut u8
    }

    fn page_align(n: usize) -> usize {
        (n + 4095) & !4095
    }

    fn memfd_sealed(size: usize, sealing: bool) -> i32 {
        let name = std::ffi::CString::new("gf-test").unwrap();
        let flags = if sealing { libc::MFD_ALLOW_SEALING } else { 0 };
        let fd = unsafe { libc::memfd_create(name.as_ptr(), flags) };
        assert!(fd >= 0);
        assert_eq!(unsafe { libc::ftruncate(fd, size as i64) }, 0);
        fd
    }

    fn new_memfd(size: usize) -> TestBuf {
        let fd = memfd_sealed(size, false);
        TestBuf {
            fd,
            map: map_rw(fd, size),
            size,
        }
    }

    /// A real dma-buf (system memory) via /dev/udmabuf; `None` if the host
    /// does not provide it or we may not open it.
    fn new_udmabuf(size: usize) -> Option<TestBuf> {
        #[repr(C)]
        struct UdmabufCreate {
            memfd: u32,
            flags: u32,
            offset: u64,
            size: u64,
        }
        // _IOW('u', 0x42, struct udmabuf_create)
        const UDMABUF_CREATE: libc::c_ulong = 0x40187542;
        let size = page_align(size);
        let dev = std::ffi::CString::new("/dev/udmabuf").unwrap();
        let udev = unsafe { libc::open(dev.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if udev < 0 {
            return None;
        }
        let mfd = memfd_sealed(size, true);
        unsafe {
            libc::fcntl(mfd, libc::F_ADD_SEALS, libc::F_SEAL_SHRINK);
        }
        let mut c = UdmabufCreate {
            memfd: mfd as u32,
            flags: 0,
            offset: 0,
            size: size as u64,
        };
        let fd = unsafe { libc::ioctl(udev, UDMABUF_CREATE, &mut c) };
        unsafe {
            libc::close(udev);
            libc::close(mfd);
        }
        if fd < 0 {
            return None;
        }
        Some(TestBuf {
            fd,
            map: map_rw(fd, size),
            size,
        })
    }

    fn report(label: &str, mut us: Vec<f64>) {
        us.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = us.len();
        let mean = us.iter().sum::<f64>() / n as f64;
        println!(
            "BENCH {label}: n={n} mean={mean:.0}us median={:.0}us p99={:.0}us",
            us[n / 2],
            us[(n * 99) / 100]
        );
    }

    /// Time `readback_dmabuf` for N frames of 1920x1080 (stride 7680).
    /// `dirty` mutates 3 tile rows of the source between frames so content
    /// differs. Run: `cargo test -p ghostframe-lib --release --lib
    /// bench_readback -- --ignored --nocapture`.
    fn bench_one(label: &str, buf: &mut TestBuf, dirty: bool, rows_mode: bool) {
        const N: usize = 300;
        let (w, h, stride) = (1920u32, 1080u32, 7680u32);
        buf.fill(|i| (i % 251) as u8);
        invalidate();
        let mut us = Vec::with_capacity(N);
        let mut sink = 0usize;
        for f in 0..N {
            let mut ranges: Vec<Range<u32>> = Vec::new();
            if dirty {
                // 3 tile rows (32 px each) change per frame.
                let b = buf.bytes_mut();
                for t in 0..3usize {
                    let row0 = ((f * 7 + t * 11) % 34) * 32;
                    let row1 = (row0 + 32).min(1080);
                    ranges.push(row0 as u32..row1 as u32);
                    for x in &mut b[row0 * 7680..row1 * 7680] {
                        *x = x.wrapping_add(1);
                    }
                }
            }
            let t0 = std::time::Instant::now();
            let p = if rows_mode {
                readback_dmabuf_rows(buf.fd, w, h, stride, &ranges).unwrap()
            } else {
                readback_dmabuf(buf.fd, w, h, stride).unwrap()
            };
            us.push(std::time::Instant::now().duration_since(t0).as_secs_f64() * 1e6);
            sink += p[(f * 4099) % p.len()] as usize;
            assert_eq!(p.len(), 7680 * 1080);
            // Outside the timed region: the fast path must still be exact.
            assert!(*p == buf.bytes()[..7680 * 1080], "{label}: frame {f}");
        }
        std::hint::black_box(sink);
        report(label, us);
    }

    #[test]
    #[ignore = "microbenchmark; run with --ignored --nocapture"]
    fn bench_readback() {
        let _g = serial();
        let size = 7680 * 1080;
        let mut m = new_memfd(size);
        bench_one("memfd static", &mut m, false, false);
        bench_one("memfd dirty-source", &mut m, true, false);
        bench_one("memfd rows static", &mut m, false, true);
        bench_one("memfd rows 3-of-34 dirty", &mut m, true, true);
        match new_udmabuf(size) {
            Some(mut u) => {
                bench_one("udmabuf static", &mut u, false, false);
                bench_one("udmabuf dirty-source", &mut u, true, false);
                bench_one("udmabuf rows static", &mut u, false, true);
                bench_one("udmabuf rows 3-of-34 dirty", &mut u, true, true);
            }
            None => println!("BENCH udmabuf: unavailable"),
        }
    }

    const W: u32 = 1920;
    const H: u32 = 1080;
    const STRIDE: u32 = 7680;
    const FRAME: usize = 7680 * 1080;

    /// Run `f` on a memfd and, when the host has /dev/udmabuf, a udmabuf.
    fn each_buf(size: usize, mut f: impl FnMut(&str, TestBuf)) {
        f("memfd", new_memfd(size));
        match new_udmabuf(size) {
            Some(u) => f("udmabuf", u),
            None => eprintln!("udmabuf unavailable: skipping that half"),
        }
    }

    fn pattern(seed: usize) -> impl Fn(usize) -> u8 {
        move |i| (i.wrapping_mul(31) ^ (i >> 7) ^ seed) as u8
    }

    #[test]
    fn round_trip_is_byte_exact() {
        let _g = serial();
        each_buf(FRAME, |name, buf| {
            buf.fill(pattern(1));
            let f = readback_dmabuf(buf.fd, W, H, STRIDE).unwrap();
            assert_eq!(f.len(), FRAME, "{name}");
            assert!(*f == buf.bytes()[..FRAME], "{name}: bytes differ");
        });
    }

    #[test]
    fn different_buffer_same_size_returns_new_pixels() {
        let _g = serial();
        let a = new_memfd(FRAME);
        let b = new_memfd(FRAME);
        a.fill(pattern(1));
        b.fill(pattern(2));
        assert!(*readback_dmabuf(a.fd, W, H, STRIDE).unwrap() == a.bytes()[..FRAME]);
        assert!(*readback_dmabuf(b.fd, W, H, STRIDE).unwrap() == b.bytes()[..FRAME]);
        // ...and back again.
        assert!(*readback_dmabuf(a.fd, W, H, STRIDE).unwrap() == a.bytes()[..FRAME]);
    }

    #[test]
    fn fd_number_reused_by_another_buffer() {
        let _g = serial();
        let a = new_memfd(FRAME);
        let b = new_memfd(FRAME);
        a.fill(pattern(1));
        b.fill(pattern(2));
        let fd = a.fd;
        assert!(*readback_dmabuf(fd, W, H, STRIDE).unwrap() == a.bytes()[..FRAME]);
        // dup2: the same fd number now names B's file.
        assert!(unsafe { libc::dup2(b.fd, fd) } == fd);
        assert!(
            *readback_dmabuf(fd, W, H, STRIDE).unwrap() == b.bytes()[..FRAME],
            "fd number reused for a different buffer must not hit the cache"
        );
        // Close-and-reopen flavour: free the number, let a new memfd take it.
        unsafe { libc::close(fd) };
        let c = new_memfd(FRAME);
        c.fill(pattern(3));
        // Whatever number c got, reading it yields c.
        assert!(*readback_dmabuf(c.fd, W, H, STRIDE).unwrap() == c.bytes()[..FRAME]);
        // `a` still owns fd number `fd` in its Drop; it is already closed or
        // reused, so leak the double close by forgetting the wrapper.
        unsafe { libc::munmap(a.map as *mut libc::c_void, a.size) };
        std::mem::forget(a);
    }

    #[test]
    fn geometry_change_on_same_fd_remaps() {
        let _g = serial();
        each_buf(FRAME, |name, buf| {
            buf.fill(pattern(5));
            let full = readback_dmabuf(buf.fd, W, H, STRIDE).unwrap();
            assert_eq!(full.len(), FRAME, "{name}");
            let half = readback_dmabuf(buf.fd, W / 2, H / 2, STRIDE / 2).unwrap();
            assert_eq!(half.len(), (STRIDE / 2 * (H / 2)) as usize, "{name}");
            assert!(*half == buf.bytes()[..half.len()], "{name}: half");
            // Same size, different shape: still a fresh key.
            let tall = readback_dmabuf(buf.fd, W / 2, H * 2, STRIDE / 2).unwrap();
            assert_eq!(tall.len(), FRAME, "{name}");
            assert!(*tall == buf.bytes()[..FRAME], "{name}: tall");
            drop((full, half, tall));
            let again = readback_dmabuf(buf.fd, W, H, STRIDE).unwrap();
            assert!(*again == buf.bytes()[..FRAME], "{name}: again");
        });
    }

    #[test]
    fn producer_writes_between_frames_are_seen() {
        let _g = serial();
        each_buf(FRAME, |name, mut buf| {
            buf.fill(pattern(7));
            let mut seed = 0x1234_5678u64;
            for n in 0..40 {
                // xorshift: scattered single-byte writes plus a block.
                for _ in 0..50 {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let i = (seed % FRAME as u64) as usize;
                    buf.bytes_mut()[i] = buf.bytes()[i].wrapping_add(1 + n as u8);
                }
                let o = (n * 200_000) % (FRAME - 4096);
                buf.bytes_mut()[o..o + 4096].fill(n as u8);
                let f = readback_dmabuf(buf.fd, W, H, STRIDE).unwrap();
                assert!(*f == buf.bytes()[..FRAME], "{name}: stale at frame {n}");
            }
        });
    }

    #[test]
    fn buffers_are_recycled() {
        let _g = serial();
        let buf = new_memfd(FRAME);
        buf.fill(pattern(9));
        let f1 = readback_dmabuf(buf.fd, W, H, STRIDE).unwrap();
        let p1 = f1.as_ptr();
        drop(f1);
        let f2 = readback_dmabuf(buf.fd, W, H, STRIDE).unwrap();
        assert_eq!(f2.as_ptr(), p1, "dropped frame's allocation is reused");
        // A frame still held is never handed out again.
        let f3 = readback_dmabuf(buf.fd, W, H, STRIDE).unwrap();
        assert_ne!(f3.as_ptr(), f2.as_ptr());
        assert!(*f2 == *f3);
    }

    #[test]
    fn degenerate_inputs() {
        let _g = serial();
        let buf = new_memfd(FRAME);
        buf.fill(pattern(1));
        assert!(readback_dmabuf(buf.fd, 0, 0, 0).unwrap().is_empty());
        assert!(readback_dmabuf(buf.fd, W, 0, STRIDE).unwrap().is_empty());
        // u32 product wraps to a small number; must be rejected, not read.
        let e = readback_dmabuf(buf.fd, 0, 65_537, 65_536);
        assert!(e.is_err(), "stride*height > file size must fail");
        assert!(readback_dmabuf(buf.fd, 0, u32::MAX, u32::MAX).is_err());
        // mmap failure (a pipe cannot be mapped) errs without disturbing
        // later reads.
        assert!(readback_dmabuf(buf.fd, W, H, STRIDE).is_ok());
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        assert!(readback_dmabuf(fds[0], 0, 16, 16).is_err());
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        assert!(*readback_dmabuf(buf.fd, W, H, STRIDE).unwrap() == buf.bytes()[..FRAME]);
    }

    /// Mutate `rows` of the producer and also the unlisted `canary` rows.
    fn poke(buf: &mut TestBuf, rows: &[u32], canary: &[u32], tag: u8) {
        for &r in rows.iter().chain(canary) {
            let (a, b) = (r as usize * 7680, (r as usize + 1) * 7680);
            for (i, x) in buf.bytes_mut()[a..b].iter_mut().enumerate() {
                *x = (i as u8).wrapping_mul(3) ^ tag ^ r as u8;
            }
        }
    }

    /// `got` equals `src` except on `stale` rows, which must equal `old`.
    fn assert_rows_eq(got: &[u8], src: &[u8], old: &[u8], stale: &[u32], what: &str) {
        for r in 0..1080usize {
            let (a, b) = (r * 7680, (r + 1) * 7680);
            let want = if stale.contains(&(r as u32)) {
                old
            } else {
                src
            };
            assert!(got[a..b] == want[a..b], "{what}: row {r} wrong");
        }
    }

    type Case = (&'static [u32], &'static [u32], Vec<Range<u32>>);

    #[test]
    #[allow(clippy::single_range_in_vec_init, clippy::reversed_empty_ranges)]
    fn dirty_rows_copy_exactly_the_listed_rows() {
        let _g = serial();
        each_buf(FRAME, |name, mut buf| {
            invalidate();
            buf.fill(pattern(11));
            let f0 = readback_dmabuf_rows(buf.fd, W, H, STRIDE, &[]).unwrap();
            assert!(*f0 == buf.bytes()[..FRAME], "{name}: first call is full");
            drop(f0);
            // (rows changed, unlisted neighbour row that must stay stale,
            //  ranges handed to the API)
            let cases: [Case; 6] = [
                (
                    &[0, 539, 1079],
                    &[1, 540, 1078],
                    vec![0..1, 539..540, 1079..1080],
                ),
                (&[31], &[32, 30], vec![31..32]),
                (&[32], &[31, 33], vec![32..33]),
                (&[1056, 1060, 1079], &[1055], vec![1056..1080]),
                // overlapping, unsorted, empty, reversed and over-long ranges
                (
                    &[10, 11, 12, 13, 1079],
                    &[14, 9],
                    vec![12..14, 10..12, 5..5, 9..3, 1079..9999],
                ),
                (&[0, 1079], &[2], vec![1079..1080, 0..1]),
            ];
            for (n, (rows, canary, ranges)) in cases.iter().enumerate() {
                let before = buf.bytes()[..FRAME].to_vec();
                poke(&mut buf, rows, canary, 0x40 + n as u8);
                let f = readback_dmabuf_rows(buf.fd, W, H, STRIDE, ranges).unwrap();
                assert_rows_eq(
                    &f,
                    &buf.bytes()[..FRAME],
                    &before,
                    canary,
                    &format!("{name} case {n}"),
                );
                drop(f);
                // The canary rows were not refreshed; list them now, as a
                // caller whose dirty set included them would.
                let ranges: Vec<Range<u32>> = canary.iter().map(|&r| r..r + 1).collect();
                let f = readback_dmabuf_rows(buf.fd, W, H, STRIDE, &ranges).unwrap();
                assert!(*f == buf.bytes()[..FRAME], "{name} case {n}: catch-up");
            }
        });
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn rows_mode_falls_back_to_full_copy_when_continuity_is_unproven() {
        let _g = serial();
        let mut a = new_memfd(FRAME);
        let b = new_memfd(FRAME);
        a.fill(pattern(1));
        b.fill(pattern(2));
        let canary = [100u32];

        // Prime with a full frame, then show the shortcut IS taken (row stale).
        invalidate();
        drop(readback_dmabuf_rows(a.fd, W, H, STRIDE, &[]).unwrap());
        poke(&mut a, &[], &canary, 1);
        let f = readback_dmabuf_rows(a.fd, W, H, STRIDE, &[]).unwrap();
        assert!(*f != a.bytes()[..FRAME], "precondition: shortcut taken");
        drop(f);

        // 1. invalidate(): the unlisted row now shows up.
        invalidate();
        let f = readback_dmabuf_rows(a.fd, W, H, STRIDE, &[]).unwrap();
        assert!(*f == a.bytes()[..FRAME], "after invalidate()");
        drop(f);

        // 2. different mapping key (other inode): full copy of B.
        poke(&mut a, &[], &canary, 2);
        let f = readback_dmabuf_rows(b.fd, W, H, STRIDE, &[]).unwrap();
        assert!(*f == b.bytes()[..FRAME], "other inode");
        drop(f);
        // ...and switching back is also unproven for A.
        let f = readback_dmabuf_rows(a.fd, W, H, STRIDE, &[]).unwrap();
        assert!(*f == a.bytes()[..FRAME], "back to A");
        drop(f);

        // 3. different geometry on the same fd.
        poke(&mut a, &[], &canary, 3);
        let f = readback_dmabuf_rows(a.fd, W / 2, H * 2, STRIDE / 2, &[]).unwrap();
        assert!(*f == a.bytes()[..FRAME], "other geometry");
        drop(f);

        // 4. the previous frame is still held: its buffer is not reusable.
        let held = readback_dmabuf_rows(a.fd, W / 2, H * 2, STRIDE / 2, &[]).unwrap();
        poke(&mut a, &[], &canary, 4);
        let f = readback_dmabuf_rows(a.fd, W / 2, H * 2, STRIDE / 2, &[]).unwrap();
        assert!(*f == a.bytes()[..FRAME], "held previous frame");
        assert!(*held != *f);
        drop((held, f));

        // 5. a failed readback in between (pipe: mmap fails).
        let f = readback_dmabuf_rows(a.fd, W, H, STRIDE, &[]).unwrap();
        drop(f);
        let mut p = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
        assert!(readback_dmabuf_rows(p[0], 0, 16, 16, &[]).is_err());
        unsafe {
            libc::close(p[0]);
            libc::close(p[1]);
        }
        poke(&mut a, &[], &canary, 5);
        let f = readback_dmabuf_rows(a.fd, W, H, STRIDE, &[]).unwrap();
        assert!(*f == a.bytes()[..FRAME], "after failed readback");
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)]
    fn merge_rows_clamps_sorts_and_merges() {
        assert_eq!(merge_rows(&[], 10), vec![]);
        assert_eq!(merge_rows(&[3..3, 9..2], 10), vec![]);
        assert_eq!(
            merge_rows(&[5..7, 0..2, 2..3, 6..9], 10),
            vec![(0, 3), (5, 9)]
        );
        assert_eq!(merge_rows(&[8..50, 20..30], 10), vec![(8, 10)]);
    }

    #[test]
    fn vulkan_device_creates_successfully() {
        match VulkanReadback::new() {
            Ok(vk) => {
                assert!(!vk.device_name().is_empty());
                println!("Vulkan device: {}", vk.device_name());
            }
            Err(e) => {
                eprintln!("Skipping Vulkan test (no GPU?): {e}");
            }
        }
    }

    #[test]
    fn pool_and_mapping_are_shared_across_threads() {
        let _g = serial();
        let buf = new_memfd(FRAME);
        buf.fill(pattern(21));
        let fd = buf.fd;
        // Read on one thread, drop on another: Send, and the allocation goes
        // back to the one process-wide pool.
        let f = std::thread::spawn(move || readback_dmabuf(fd, W, H, STRIDE).unwrap())
            .join()
            .unwrap();
        let p = f.as_ptr();
        std::thread::spawn(move || drop(f)).join().unwrap();
        let g = readback_dmabuf(fd, W, H, STRIDE).unwrap();
        assert_eq!(g.as_ptr(), p, "pool is not per thread");
        assert!(*g == buf.bytes()[..FRAME]);
        assert!(cached_key().is_some());
    }

    #[test]
    fn release_drops_mapping_and_pool_and_reads_still_work() {
        let _g = serial();
        let buf = new_memfd(FRAME);
        buf.fill(pattern(22));
        drop(readback_dmabuf(buf.fd, W, H, STRIDE).unwrap());
        assert!(cached_key().is_some());
        release_cached_mapping();
        assert!(cached_key().is_none());
        assert!(lock_state().pool.is_empty());
        assert!(*readback_dmabuf(buf.fd, W, H, STRIDE).unwrap() == buf.bytes()[..FRAME]);
        assert!(cached_key().is_some());
    }

    /// A file reporting st_size == 0 has no trustworthy identity: it must be
    /// mapped per call and never enter (or evict from) the cache.
    #[test]
    fn zero_st_size_is_not_cached() {
        let _g = serial();
        let buf = new_memfd(FRAME);
        buf.fill(pattern(23));
        drop(readback_dmabuf(buf.fd, W, H, STRIDE).unwrap());
        let before = cached_key();
        assert!(before.is_some());
        let path = std::ffi::CString::new("/dev/zero").unwrap();
        let z = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
        if z < 0 {
            eprintln!("/dev/zero unavailable: skipping");
            return;
        }
        let f = readback_dmabuf(z, 0, 16, 4096).unwrap();
        assert_eq!(f.len(), 16 * 4096);
        assert!(f.iter().all(|&b| b == 0));
        unsafe { libc::close(z) };
        assert_eq!(cached_key(), before, "uncacheable read touched the cache");
    }

    /// Continuity is process-wide: frame 1 on thread A, frames 2..5 on thread
    /// B, then a rows call on A (with an empty dirty set, as if nothing
    /// changed since *its* last call) must still be exact. With per-thread
    /// state, A's old buffer still matched A's own stamp and was reused four
    /// frames stale.
    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn rows_continuity_survives_thread_hopping() {
        use std::sync::mpsc::channel;
        let _g = serial();
        let mut a = new_memfd(FRAME);
        a.fill(pattern(31));
        let fd = a.fd;
        let (to_a, a_rx) = channel::<()>();
        let (a_done, from_a) = channel::<Option<Vec<u8>>>();
        std::thread::scope(|sc| {
            // Thread A: one long-lived thread, so thread-local state would
            // persist on it between its two calls.
            sc.spawn(move || {
                invalidate();
                drop(readback_dmabuf_rows(fd, W, H, STRIDE, &[]).unwrap());
                a_done.send(None).unwrap();
                a_rx.recv().unwrap();
                let f = readback_dmabuf_rows(fd, W, H, STRIDE, &[]).unwrap();
                a_done.send(Some(f.to_vec())).unwrap();
            });
            assert!(from_a.recv().unwrap().is_none());
            for n in 0..4u32 {
                let r = n * 100 + 3;
                poke(&mut a, &[r], &[], 0x50 + n as u8);
                // Thread B.
                sc.spawn(move || {
                    drop(readback_dmabuf_rows(fd, W, H, STRIDE, &[r..r + 1]).unwrap());
                })
                .join()
                .unwrap();
            }
            to_a.send(()).unwrap();
            let got = from_a.recv().unwrap().unwrap();
            assert!(
                got == a.bytes()[..FRAME],
                "stale buffer from another thread reused"
            );
        });
    }

    /// Any call, including one that fails before touching the mapping,
    /// breaks continuity.
    #[test]
    fn early_failure_breaks_row_continuity() {
        let _g = serial();
        let mut a = new_memfd(FRAME);
        a.fill(pattern(41));
        let canary = [200u32];
        invalidate();
        drop(readback_dmabuf_rows(a.fd, W, H, STRIDE, &[]).unwrap());
        poke(&mut a, &[], &canary, 1);
        // Precondition: the shortcut is taken (the unlisted row stays stale).
        let f = readback_dmabuf_rows(a.fd, W, H, STRIDE, &[]).unwrap();
        assert!(*f != a.bytes()[..FRAME], "precondition: shortcut taken");
        drop(f);

        // Bad fd: fstat fails with EBADF before any mapping work.
        poke(&mut a, &[], &canary, 2);
        assert!(readback_dmabuf_rows(-1, W, H, STRIDE, &[]).is_err());
        let f = readback_dmabuf_rows(a.fd, W, H, STRIDE, &[]).unwrap();
        assert!(*f == a.bytes()[..FRAME], "after fstat failure");
        drop(f);

        // Absurd geometry, rejected by the st_size check.
        poke(&mut a, &[], &canary, 3);
        assert!(readback_dmabuf_rows(a.fd, 0, u32::MAX, u32::MAX, &[]).is_err());
        let f = readback_dmabuf_rows(a.fd, W, H, STRIDE, &[]).unwrap();
        assert!(*f == a.bytes()[..FRAME], "after size rejection");
        drop(f);

        // st_size rejection (file smaller than stride * height).
        poke(&mut a, &[], &canary, 4);
        assert!(readback_dmabuf_rows(a.fd, 0, 65_537, 65_536, &[]).is_err());
        let f = readback_dmabuf_rows(a.fd, W, H, STRIDE, &[]).unwrap();
        assert!(*f == a.bytes()[..FRAME], "after st_size rejection");
    }
}
