// SPDX-License-Identifier: GPL-3.0-or-later
//! Original Vulkan preflight worker. It creates an instance and a logical
//! graphics device but does not attach to Plasma or create a display surface.

use std::ffi::{CStr, CString};

use anyhow::{Context, Result, bail};
use ash::{Entry, vk};
use clap::Parser;
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(version, about = "Probe Vulkan devices for the isolated KWE renderer")]
struct Arguments {
    #[arg(long)]
    json: bool,
    /// Also create and immediately destroy a logical graphics device.
    #[arg(long, default_value_t = true)]
    create_device: bool,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    loader_api_version: String,
    capabilities: CapabilityManifest,
    devices: Vec<DeviceReport>,
}

#[derive(Debug, Serialize)]
struct CapabilityManifest {
    backend: &'static str,
    version: &'static str,
    scene2d: bool,
    scene3d: bool,
    shaders: bool,
    particles: bool,
    pointer_input: bool,
    audio_response: bool,
}

#[derive(Debug, Serialize)]
struct DeviceReport {
    name: String,
    kind: String,
    api_version: String,
    driver_version_raw: u32,
    graphics_queue_family: Option<u32>,
    logical_device_created: bool,
    required_external_memory_extensions: ExtensionSupport,
}

#[derive(Debug, Serialize)]
struct ExtensionSupport {
    external_memory_fd: bool,
    external_memory_dma_buf: bool,
    external_semaphore_fd: bool,
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let report = unsafe { probe(arguments.create_device) }?;
    if arguments.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Vulkan loader {}", report.loader_api_version);
        for device in report.devices {
            println!(
                "{} [{}] Vulkan {}, graphics queue {:?}, logical device {}, DMA-BUF {}/{}/{}",
                device.name,
                device.kind,
                device.api_version,
                device.graphics_queue_family,
                device.logical_device_created,
                device
                    .required_external_memory_extensions
                    .external_memory_fd,
                device
                    .required_external_memory_extensions
                    .external_memory_dma_buf,
                device
                    .required_external_memory_extensions
                    .external_semaphore_fd,
            );
        }
    }
    Ok(())
}

unsafe fn probe(create_device: bool) -> Result<Report> {
    // SAFETY: ash loads the system Vulkan loader; every Vulkan object is
    // destroyed before its parent and all returned arrays are owned by Rust.
    let entry = unsafe { Entry::load() }.context("load libvulkan")?;
    let loader_version =
        unsafe { entry.try_enumerate_instance_version() }?.unwrap_or(vk::API_VERSION_1_0);
    let app_name = CString::new("kwe-vulkan-alpha")?;
    let app_info = vk::ApplicationInfo::default()
        .application_name(&app_name)
        .application_version(vk::make_api_version(0, 0, 1, 0))
        .engine_name(&app_name)
        .engine_version(vk::make_api_version(0, 0, 1, 0))
        .api_version(loader_version.min(vk::API_VERSION_1_3));
    let create_info = vk::InstanceCreateInfo::default().application_info(&app_info);
    let instance =
        unsafe { entry.create_instance(&create_info, None) }.context("create Vulkan instance")?;
    let result = (|| -> Result<Vec<DeviceReport>> {
        let physical_devices = unsafe { instance.enumerate_physical_devices() }
            .context("enumerate Vulkan physical devices")?;
        if physical_devices.is_empty() {
            bail!("Vulkan loader reported no physical devices");
        }
        physical_devices
            .into_iter()
            .map(|physical| {
                let properties = unsafe { instance.get_physical_device_properties(physical) };
                let name = unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
                let queue_families =
                    unsafe { instance.get_physical_device_queue_family_properties(physical) };
                let graphics_queue_family = queue_families
                    .iter()
                    .position(|family| family.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                    .map(|index| index as u32);
                let extensions =
                    unsafe { instance.enumerate_device_extension_properties(physical) }?;
                let has_extension = |needle: &CStr| {
                    extensions.iter().any(|property| {
                        (unsafe { CStr::from_ptr(property.extension_name.as_ptr()) }) == needle
                    })
                };
                let support = ExtensionSupport {
                    external_memory_fd: has_extension(ash::khr::external_memory_fd::NAME),
                    external_memory_dma_buf: has_extension(ash::ext::external_memory_dma_buf::NAME),
                    external_semaphore_fd: has_extension(ash::khr::external_semaphore_fd::NAME),
                };
                let mut logical_device_created = false;
                if create_device && let Some(family) = graphics_queue_family {
                    let priorities = [1.0_f32];
                    let queue = vk::DeviceQueueCreateInfo::default()
                        .queue_family_index(family)
                        .queue_priorities(&priorities);
                    let device_info = vk::DeviceCreateInfo::default()
                        .queue_create_infos(std::slice::from_ref(&queue));
                    let device = unsafe { instance.create_device(physical, &device_info, None) }
                        .with_context(|| format!("create logical device for {name}"))?;
                    logical_device_created = true;
                    unsafe {
                        device.device_wait_idle()?;
                        device.destroy_device(None);
                    }
                }
                Ok(DeviceReport {
                    name,
                    kind: format!("{:?}", properties.device_type).to_ascii_lowercase(),
                    api_version: version(properties.api_version),
                    driver_version_raw: properties.driver_version,
                    graphics_queue_family,
                    logical_device_created,
                    required_external_memory_extensions: support,
                })
            })
            .collect()
    })();
    unsafe {
        instance.destroy_instance(None);
    }
    Ok(Report {
        schema_version: 1,
        loader_api_version: version(loader_version),
        capabilities: CapabilityManifest {
            backend: "vulkan-preflight",
            version: env!("CARGO_PKG_VERSION"),
            // This worker only probes the device today. Keep unsupported
            // renderer features explicit until their execution paths land.
            scene2d: false,
            scene3d: false,
            shaders: false,
            particles: false,
            pointer_input: false,
            audio_response: false,
        },
        devices: result?,
    })
}

fn version(value: u32) -> String {
    format!(
        "{}.{}.{}",
        vk::api_version_major(value),
        vk::api_version_minor(value),
        vk::api_version_patch(value)
    )
}
