// SPDX-License-Identifier: MIT OR Apache-2.0
//! The fence a publish hands its consumers, on the Vulkan backend: an
//! exportable binary semaphore, signalled by the frame's submit and exported
//! as a sync_file (`VK_KHR_external_semaphore_fd`, `SYNC_FD`).
//!
//! Vulkan only. GLES needs no fence at all (see `ring.rs`): Mesa puts every BO a
//! batch touches in the submit's handle list, so the kernel attaches the job's
//! fence to the shared dmabuf by itself. A Vulkan driver does not do that for
//! memory it did not allocate for a swapchain, which is why this exists.
//!
//! The order matters: [`ExportFence::arm`] queues the semaphore on wgpu-hal's
//! queue *before* the submit, where it joins that submit's signal list, and
//! [`ExportFence::take`] exports it *after*. Exporting a semaphore nothing is
//! going to signal violates VUID-VkSemaphoreGetFdInfoKHR-handleType-03254 and
//! can hang, so `take` first removes the semaphore from the queue's pending
//! list: if it was still there the submit never consumed it (an error, device
//! loss, an empty flush) and nothing is exported.
//!
//! One semaphore serves every publish: exporting a SYNC_FD moves the payload
//! into the descriptor and leaves the semaphore unsignalled and reusable.
//!
//! **The device needs `VK_KHR_external_semaphore_fd` enabled**, which wgpu-hal
//! does not do on its own; `WgpuContext::new` adds it when the driver has it,
//! and [`ExportFence::new`] reports why not when it was left out.

use crate::wgpu_ctx::WgpuContext;
use crate::GpuError;
use ash::vk;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct ExportFence {
    semaphore: vk::Semaphore,
    fns: ash::khr::external_semaphore_fd::Device,
    device: ash::Device,
    /// Whether the semaphore is on the queue's signal list awaiting a submit.
    /// Arming twice would queue it twice, which Vulkan forbids.
    armed: AtomicBool,
    queue: wgpu::Queue,
    /// Keeps the `VkDevice` alive for the `Drop` below, the way `VkOwner` in
    /// `upload_slab.rs` does.
    keepalive: wgpu::Device,
}

impl ExportFence {
    /// Create the semaphore, or say why this device cannot make fences.
    pub(crate) fn new(ctx: &WgpuContext) -> Result<Self, String> {
        // SAFETY: the borrowed hal device is used only to read the list of
        // enabled extensions, and does not outlive this expression.
        let enabled = unsafe { ctx.device.as_hal::<wgpu_hal::api::Vulkan>() }
            .map(|d| {
                d.enabled_device_extensions()
                    .contains(&ash::khr::external_semaphore_fd::NAME)
            })
            .ok_or("not the Vulkan backend")?;
        if !enabled {
            return Err("VK_KHR_external_semaphore_fd is not enabled on the device".to_string());
        }
        ctx.with_raw(|instance, device, phys| {
            // `vkGetPhysicalDeviceExternalSemaphoreProperties` is core 1.1; the
            // extension is enabled, so 1.1+ is what the instance runs unless
            // the driver is older, which we do not try to support here.
            // SAFETY: a read-only query on the adapter's own physical device.
            let props = unsafe {
                let mut out = vk::ExternalSemaphoreProperties::default();
                instance.get_physical_device_external_semaphore_properties(
                    phys,
                    &vk::PhysicalDeviceExternalSemaphoreInfo::default()
                        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
                    &mut out,
                );
                out
            };
            if !props
                .external_semaphore_features
                .contains(vk::ExternalSemaphoreFeatureFlags::EXPORTABLE)
            {
                return Err("SYNC_FD semaphores are not exportable here".to_string());
            }
            let mut export = vk::ExportSemaphoreCreateInfo::default()
                .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
            let info = vk::SemaphoreCreateInfo::default().push_next(&mut export);
            // SAFETY: `device` is the live device borrowed from `with_raw`; the
            // semaphore is destroyed in `Drop` and nowhere else.
            let semaphore = unsafe { device.create_semaphore(&info, None) }
                .map_err(|e| format!("create_semaphore (exportable): {e}"))?;
            Ok(Self {
                semaphore,
                fns: ash::khr::external_semaphore_fd::Device::new(instance, device),
                device: device.clone(),
                armed: AtomicBool::new(false),
                queue: ctx.queue.clone(),
                keepalive: ctx.device.clone(),
            })
        })
        .unwrap_or_else(|| Err("not the Vulkan backend".to_string()))
    }

    /// Before the submit: add the semaphore to the signal list of the next one.
    pub(crate) fn arm(&self, ctx: &WgpuContext) -> Result<(), GpuError> {
        if self.armed.swap(true, Ordering::Relaxed) {
            return Err(GpuError::Vulkan("export fence armed twice".into()));
        }
        // SAFETY: the borrowed hal queue does not outlive this call.
        let queue = unsafe { ctx.queue.as_hal::<wgpu_hal::api::Vulkan>() }
            .ok_or_else(|| GpuError::Vulkan("queue is not the Vulkan backend".into()))?;
        queue.add_signal_semaphore(self.semaphore, None);
        Ok(())
    }

    /// After the submit: export the semaphore's pending signal as a sync_file.
    /// `None` when the driver reports it already signalled (`-1`), which needs
    /// no waiting. An error when the submit did not consume the semaphore, in
    /// which case nothing was exported.
    pub(crate) fn take(&self, ctx: &WgpuContext) -> Result<Option<OwnedFd>, GpuError> {
        if !self.armed.swap(false, Ordering::Relaxed) {
            return Err(GpuError::Vulkan("export fence was not armed".into()));
        }
        // SAFETY: the borrowed hal queue does not outlive this call.
        let queue = unsafe { ctx.queue.as_hal::<wgpu_hal::api::Vulkan>() }
            .ok_or_else(|| GpuError::Vulkan("queue is not the Vulkan backend".into()))?;
        // A submit moves the pending signals into its own list, so finding the
        // semaphore still queued means no submit took it.
        if queue.remove_signal_semaphore(self.semaphore) {
            return Err(GpuError::Vulkan(
                "the submit did not consume the export semaphore".into(),
            ));
        }
        let info = vk::SemaphoreGetFdInfoKHR::default()
            .semaphore(self.semaphore)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        // SAFETY: the semaphore was created exportable as SYNC_FD and a submit
        // consumed the signal `arm` queued, which is the precondition for
        // exporting it.
        let raw = unsafe { self.fns.get_semaphore_fd(&info) }
            .map_err(|e| GpuError::Vulkan(format!("vkGetSemaphoreFdKHR: {e}")))?;
        if raw < 0 {
            return Ok(None);
        }
        // SAFETY: `raw` is a fresh descriptor this call owns exclusively.
        Ok(Some(unsafe { OwnedFd::from_raw_fd(raw) }))
    }
}

impl Drop for ExportFence {
    fn drop(&mut self) {
        // A semaphore with a signal still pending may not be destroyed. Take it
        // back off the queue if no submit did, then let any that did finish.
        // SAFETY: the borrowed hal queue does not outlive this statement.
        if let Some(queue) = unsafe { self.queue.as_hal::<wgpu_hal::api::Vulkan>() } {
            queue.remove_signal_semaphore(self.semaphore);
        }
        let _ = self.keepalive.poll(wgpu::PollType::wait_indefinitely());
        // SAFETY: created in `new`, destroyed once, and `keepalive` holds the
        // device open.
        unsafe { self.device.destroy_semaphore(self.semaphore, None) };
    }
}
