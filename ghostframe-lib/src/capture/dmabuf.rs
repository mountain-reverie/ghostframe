//! DMA-BUF pixel readback via mmap and Vulkan DMA-BUF import.
//!
//! M1: simple mmap of the DMA-BUF fd to read raw pixels.
//! VulkanReadback: imports DMA-BUF fd via Vulkan extensions for GPU-accelerated readback.

use std::io;

/// Read pixels from a DMA-BUF file descriptor via mmap.
///
/// Returns a `Vec<u8>` of BGRA pixel data, `stride * height` bytes.
/// The caller is responsible for the fd's lifetime (this function does not close it).
pub fn readback_dmabuf(
    fd: std::os::unix::io::RawFd,
    _width: u32,
    height: u32,
    stride: u32,
) -> io::Result<Vec<u8>> {
    let size = (stride * height) as usize;
    if size == 0 {
        return Ok(Vec::new());
    }
    unsafe {
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let mut pixels = vec![0u8; size];
        std::ptr::copy_nonoverlapping(ptr as *const u8, pixels.as_mut_ptr(), size);
        libc::munmap(ptr, size);
        Ok(pixels)
    }
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

    #[test]
    fn readback_from_memfd() {
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
    fn bench_one(label: &str, buf: &mut TestBuf, dirty: bool) {
        const N: usize = 300;
        let (w, h, stride) = (1920u32, 1080u32, 7680u32);
        buf.fill(|i| (i % 251) as u8);
        let mut us = Vec::with_capacity(N);
        let mut sink = 0usize;
        for f in 0..N {
            if dirty {
                // 3 tile rows (32 px each) change per frame.
                let b = buf.bytes_mut();
                for t in 0..3usize {
                    let row0 = ((f * 7 + t * 11) % 34) * 32;
                    let (a, e) = (row0 * 7680, ((row0 + 32).min(1080)) * 7680);
                    for x in &mut b[a..e] {
                        *x = x.wrapping_add(1);
                    }
                }
            }
            let t0 = std::time::Instant::now();
            let p = readback_dmabuf(buf.fd, w, h, stride).unwrap();
            us.push(std::time::Instant::now().duration_since(t0).as_secs_f64() * 1e6);
            sink += p[(f * 4099) % p.len()] as usize;
            assert_eq!(p.len(), 7680 * 1080);
        }
        std::hint::black_box(sink);
        report(label, us);
    }

    #[test]
    #[ignore = "microbenchmark; run with --ignored --nocapture"]
    fn bench_readback() {
        let size = 7680 * 1080;
        let mut m = new_memfd(size);
        bench_one("memfd static", &mut m, false);
        bench_one("memfd dirty-source", &mut m, true);
        match new_udmabuf(size) {
            Some(mut u) => {
                bench_one("udmabuf static", &mut u, false);
                bench_one("udmabuf dirty-source", &mut u, true);
            }
            None => println!("BENCH udmabuf: unavailable"),
        }
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
}
