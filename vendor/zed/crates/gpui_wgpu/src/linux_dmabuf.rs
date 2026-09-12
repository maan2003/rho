use anyhow::{Context as _, Result, bail};
use ash::vk;
use gpui::{LinuxDmaBufDevice, LinuxDmaBufFormat};
use std::path::PathBuf;
use std::sync::Arc;
use wgpu_hal::vulkan;

pub(crate) fn supported(adapter: &vulkan::Adapter) -> bool {
    let caps = adapter.physical_device_capabilities();
    caps.supports_extension(ash::khr::external_semaphore_fd::NAME)
        && caps.supports_extension(ash::ext::queue_family_foreign::NAME)
        && external_sync_fd_importable(adapter)
}

pub(crate) unsafe fn open(
    adapter: &wgpu::Adapter,
    features: wgpu::Features,
    limits: &wgpu::Limits,
    memory_hints: &wgpu::MemoryHints,
    descriptor: &wgpu::DeviceDescriptor<'_>,
) -> Result<(wgpu::Device, wgpu::Queue)> {
    let hal = unsafe { adapter.as_hal::<vulkan::Api>() }.context("not a Vulkan adapter")?;
    if !supported(&hal) {
        bail!("explicit DMA-BUF synchronization is unavailable");
    }
    let add_external_semaphore = hal.shared_instance().instance_api_version() < vk::API_VERSION_1_1
        && hal
            .physical_device_capabilities()
            .supports_extension(ash::khr::external_semaphore::NAME);
    let opened = unsafe {
        hal.open_with_callback(
            features,
            limits,
            memory_hints,
            Some(Box::new(move |args| {
                if add_external_semaphore {
                    args.extensions.push(ash::khr::external_semaphore::NAME);
                }
                args.extensions.push(ash::khr::external_semaphore_fd::NAME);
                args.extensions.push(ash::ext::queue_family_foreign::NAME);
            })),
        )
    }
    .context("open Vulkan device with external synchronization")?;
    unsafe { adapter.create_device_from_hal(opened, descriptor) }
        .context("wrap external-sync Vulkan device")
}

pub(crate) fn install_capabilities(adapter: &wgpu::Adapter, device: &wgpu::Device) {
    gpui::clear_linux_dmabuf_device();
    if !device
        .features()
        .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
    {
        return;
    }
    let Some(hal) = (unsafe { adapter.as_hal::<vulkan::Api>() }) else {
        return;
    };
    if !supported(&hal) {
        return;
    }
    match capabilities(&hal) {
        Ok(capabilities) => gpui::set_linux_dmabuf_device(capabilities),
        Err(error) => log::warn!("DMA-BUF embedding disabled: {error:#}"),
    }
}

fn external_sync_fd_importable(adapter: &vulkan::Adapter) -> bool {
    let info = vk::PhysicalDeviceExternalSemaphoreInfo::default()
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let mut properties = vk::ExternalSemaphoreProperties::default();
    unsafe {
        adapter
            .shared_instance()
            .raw_instance()
            .get_physical_device_external_semaphore_properties(
                adapter.raw_physical_device(),
                &info,
                &mut properties,
            )
    };
    properties
        .external_semaphore_features
        .contains(vk::ExternalSemaphoreFeatureFlags::IMPORTABLE)
}

fn capabilities(adapter: &vulkan::Adapter) -> Result<LinuxDmaBufDevice> {
    let instance = adapter.shared_instance().raw_instance();
    let physical = adapter.raw_physical_device();
    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
    unsafe { instance.get_physical_device_properties2(physical, &mut properties) };
    if drm.has_render == 0 {
        bail!("Vulkan device has no DRM render node");
    }
    let device_id = libc::makedev(drm.render_major as _, drm.render_minor as _);
    let render_node = render_node(device_id)?;
    let modifiers = importable_modifiers(adapter)?;
    if modifiers.is_empty() {
        bail!("Vulkan device has no importable single-plane BGRA DMA-BUF modifiers");
    }
    let mut formats = Vec::with_capacity(modifiers.len() * 2);
    for modifier in modifiers {
        // DRM_FORMAT_ARGB8888 and DRM_FORMAT_XRGB8888, little-endian BGRA bytes.
        formats.push(LinuxDmaBufFormat {
            fourcc: u32::from_le_bytes(*b"AR24"),
            modifier,
        });
        formats.push(LinuxDmaBufFormat {
            fourcc: u32::from_le_bytes(*b"XR24"),
            modifier,
        });
    }
    Ok(LinuxDmaBufDevice {
        render_node,
        device_id,
        formats: Arc::from(formats),
    })
}

fn render_node(device_id: libc::dev_t) -> Result<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;
    for entry in std::fs::read_dir("/dev/dri").context("read /dev/dri")? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("renderD")
            && entry.metadata()?.rdev() == device_id
        {
            return Ok(entry.path());
        }
    }
    bail!("DRM render node {device_id} is not present under /dev/dri")
}

fn importable_modifiers(adapter: &vulkan::Adapter) -> Result<Vec<u64>> {
    let instance = adapter.shared_instance().raw_instance();
    let physical = adapter.raw_physical_device();
    let format = vk::Format::B8G8R8A8_UNORM;
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    {
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe { instance.get_physical_device_format_properties2(physical, format, &mut props) };
    }
    let mut entries = vec![
        vk::DrmFormatModifierPropertiesEXT::default();
        list.drm_format_modifier_count as usize
    ];
    list.p_drm_format_modifier_properties = entries.as_mut_ptr();
    list.drm_format_modifier_count = entries.len() as u32;
    {
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe { instance.get_physical_device_format_properties2(physical, format, &mut props) };
    }
    entries.truncate(list.drm_format_modifier_count as usize);
    Ok(entries
        .into_iter()
        .filter(|entry| {
            entry.drm_format_modifier_plane_count == 1
                && entry.drm_format_modifier_tiling_features.contains(
                    vk::FormatFeatureFlags::SAMPLED_IMAGE
                        | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
                        | vk::FormatFeatureFlags::TRANSFER_SRC,
                )
                && modifier_importable(adapter, format, entry.drm_format_modifier)
        })
        .map(|entry| entry.drm_format_modifier)
        .collect())
}

fn modifier_importable(adapter: &vulkan::Adapter, format: vk::Format, modifier: u64) -> bool {
    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
        .push_next(&mut modifier_info)
        .push_next(&mut external_info);
    let mut external = vk::ExternalImageFormatProperties::default();
    let mut properties = vk::ImageFormatProperties2::default().push_next(&mut external);
    unsafe {
        adapter
            .shared_instance()
            .raw_instance()
            .get_physical_device_image_format_properties2(
                adapter.raw_physical_device(),
                &info,
                &mut properties,
            )
    }
    .is_ok()
        && external
            .external_memory_properties
            .external_memory_features
            .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
}
