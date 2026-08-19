// SPDX-License-Identifier: Apache-2.0
// Original offscreen Vulkan compositor for the M3a slice (ADR 0001).
//
// No window, no swapchain, no extensions: a Vulkan 1.2 instance, the first
// physical device with a graphics queue (--device filters by name substring,
// discrete GPUs preferred, llvmpipe works for the test lane), and a W x H
// COLOR_OPTIMAL image that is cleared every frame by a fullscreen triangle
// whose fragment color comes from a push constant. The result is copied
// image->buffer, mapped, converted to the frame protocol's premultiplied
// BGRA, and handed to the caller for SharedFrameWriter::publish.
//
// Format: B8G8R8A8_UNORM when the device supports COLOR_ATTACHMENT +
// TRANSFER_SRC in optimal tiling (the common case, and what the protocol
// wants); otherwise R8G8B8A8_UNORM and the conversion swaps the first and
// third channel (the swap and the premultiplication are one function, since
// the byte order already encodes the channel order).
//
// Synchronization: one fence, in-flight 1, 1 s wait bound. A fence timeout
// means the GPU is not making progress; the queue still holds the
// uncompleted submit (fence and command buffer remain pending), so retrying
// would reset a pending fence and re-record a pending command buffer —
// VUID violations on the exact error this used to claim to recover from.
// FenceTimeout is therefore immediately fatal: the caller escalates the
// first timeout to backend reject (exit 73). Other render failures are
// counted into a bounded streak before escalation.

use std::ffi::CStr;
use std::fmt;

use ash::vk;
use ash::{Device, Entry, Instance};

/// Fence wait bound per frame; a GPU stuck longer than this is treated as a
/// backend failure by the caller.
pub const FENCE_TIMEOUT_NS: u64 = 1_000_000_000;

#[derive(Debug)]
pub enum RenderError {
    Vulkan(String),
    FenceTimeout,
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vulkan(message) => write!(f, "vulkan error: {message}"),
            Self::FenceTimeout => write!(f, "fence wait timed out after 1 s"),
        }
    }
}

impl std::error::Error for RenderError {}

impl From<vk::Result> for RenderError {
    fn from(r: vk::Result) -> Self {
        Self::Vulkan(format!("{r}"))
    }
}

/// Convert a straight-alpha readback buffer to the protocol's premultiplied
/// BGRA8888 (byte order B, G, R, A). A B8G8R8A8_UNORM attachment already
/// stores B, G, R, A in memory order, so `bgr_source` (true) means identity
/// channel order; an R8G8B8A8_UNORM attachment stores R, G, B, A, so the
/// first and third channels are swapped. Both cases premultiply by alpha
/// with round-to-nearest: out[i] = (byte[i] * a + 127) / 255.
pub fn bgra_premultiplied(bytes: &[u8], bgr_source: bool) -> Vec<u8> {
    debug_assert_eq!(bytes.len() % 4, 0);
    let order: &[usize] = if bgr_source { &[0, 1, 2] } else { &[2, 1, 0] };
    let mut out = Vec::with_capacity(bytes.len());
    for pixel in bytes.chunks_exact(4) {
        let a = u16::from(pixel[3]);
        for &channel in order {
            out.push(((u16::from(pixel[channel]) * a + 127) / 255) as u8);
        }
        out.push(pixel[3]);
    }
    out
}

/// Device ranking for the pick: discrete first, then integrated, then
/// anything else; llvmpipe is the last resort (the test lane).
fn device_rank(device_type: vk::PhysicalDeviceType) -> u8 {
    match device_type {
        vk::PhysicalDeviceType::DISCRETE_GPU => 0,
        vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
        vk::PhysicalDeviceType::VIRTUAL_GPU => 2,
        vk::PhysicalDeviceType::CPU => 3,
        vk::PhysicalDeviceType::OTHER => 4,
        _ => 5,
    }
}

fn find_memory_type(
    instance: &Instance,
    physical: vk::PhysicalDevice,
    requirements: &vk::MemoryRequirements,
    flags: vk::MemoryPropertyFlags,
) -> Option<u32> {
    let properties = unsafe { instance.get_physical_device_memory_properties(physical) };
    properties
        .memory_types
        .iter()
        .enumerate()
        .find_map(|(index, memory_type)| {
            if requirements.memory_type_bits & (1 << index) != 0
                && memory_type.property_flags.contains(flags)
            {
                Some(index as u32)
            } else {
                None
            }
        })
}

/// What the device probe reports (printed by main.rs --probe).
#[derive(Debug, Clone)]
pub struct ProbeReport {
    pub device_name: String,
    pub device_kind: String,
    pub format: String,
}

pub struct ClearRenderer {
    instance: Instance,
    device: Device,
    queue: vk::Queue,
    format: vk::Format,
    image: vk::Image,
    image_memory: vk::DeviceMemory,
    image_view: vk::ImageView,
    buffer: vk::Buffer,
    buffer_memory: vk::DeviceMemory,
    mapped: *mut u8,
    buffer_size: usize,
    render_pass: vk::RenderPass,
    framebuffer: vk::Framebuffer,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    vertex_module: vk::ShaderModule,
    fragment_module: vk::ShaderModule,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    fence: vk::Fence,
    width: u32,
    height: u32,
    device_name: String,
    device_kind: String,
    // Kept alive for the whole renderer lifetime: ash 0.38's Entry owns the
    // dlopen guard on libvulkan.so.1, and the loader's own trampoline
    // function pointers (vkDestroyDevice among them) dangle once the entry
    // drops. Never read; the drop side effect is the point. Declared last so
    // it is destroyed after the explicit Drop body.
    _entry: Entry,
}

impl ClearRenderer {
    pub fn new(device_filter: Option<&str>, width: u32, height: u32) -> Result<Self, RenderError> {
        // SAFETY: loading the Vulkan loader functions is safe as long as the
        // returned entry is used only to call Vulkan functions, which is all
        // this crate does.
        let entry = unsafe { Entry::load() }
            .map_err(|e| RenderError::Vulkan(format!("load entry: {e}")))?;
        let app_name = c"kwe-scene-renderer";
        let app_info = vk::ApplicationInfo::default()
            .application_name(app_name)
            .application_version(vk::make_api_version(0, 0, 1, 0))
            .engine_name(app_name)
            .engine_version(vk::make_api_version(0, 0, 1, 0))
            .api_version(vk::API_VERSION_1_2);
        let create_info = vk::InstanceCreateInfo::default().application_info(&app_info);
        let instance = unsafe { entry.create_instance(&create_info, None) }?;

        let (physical, queue_family, device_name, device_kind) =
            pick_device(&instance, device_filter)?;

        let priorities = [1.0_f32];
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities);
        let device_info =
            vk::DeviceCreateInfo::default().queue_create_infos(std::slice::from_ref(&queue_info));
        let device = unsafe { instance.create_device(physical, &device_info, None) }?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };

        let format = pick_format(&instance, physical);
        let renderer = Self::new_with(
            entry,
            instance,
            device,
            physical,
            queue,
            queue_family,
            format,
            width,
            height,
            device_name,
            device_kind,
        )?;
        eprintln!(
            "event=renderer.scene.device name={} kind={} format={}",
            renderer.device_name,
            renderer.device_kind,
            format_name(renderer.format)
        );
        Ok(renderer)
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with(
        entry: Entry,
        instance: Instance,
        device: Device,
        physical: vk::PhysicalDevice,
        queue: vk::Queue,
        queue_family: u32,
        format: vk::Format,
        width: u32,
        height: u32,
        device_name: String,
        device_kind: String,
    ) -> Result<Self, RenderError> {
        // Offscreen color attachment, transfer source for the readback.
        let image_info = vk::ImageCreateInfo::default()
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
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&image_info, None) }?;
        let image_requirements = unsafe { device.get_image_memory_requirements(image) };
        let image_memory = allocate(
            &instance,
            &device,
            physical,
            &image_requirements,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )?;
        unsafe { device.bind_image_memory(image, image_memory, 0) }?;

        let image_view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(vk::ComponentMapping {
                r: vk::ComponentSwizzle::IDENTITY,
                g: vk::ComponentSwizzle::IDENTITY,
                b: vk::ComponentSwizzle::IDENTITY,
                a: vk::ComponentSwizzle::IDENTITY,
            })
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        let image_view = unsafe { device.create_image_view(&image_view_info, None) }?;

        // Host-visible staging buffer for the readback.
        let buffer_size = width as usize * height as usize * 4;
        let buffer_info = vk::BufferCreateInfo::default()
            .size(buffer_size as vk::DeviceSize)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { device.create_buffer(&buffer_info, None) }?;
        let buffer_requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
        let buffer_memory = allocate(
            &instance,
            &device,
            physical,
            &buffer_requirements,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        unsafe { device.bind_buffer_memory(buffer, buffer_memory, 0) }?;
        let mapped = unsafe {
            device.map_memory(
                buffer_memory,
                0,
                buffer_size as vk::DeviceSize,
                vk::MemoryMapFlags::empty(),
            )?
        }
        .cast::<u8>();

        // Render pass: UNDEFINED -> COLOR_ATTACHMENT_OPTIMAL, clear; the pass
        // ends in TRANSFER_SRC_OPTIMAL so the copy can read it. The two
        // external subpass dependencies cover both layout transitions.
        let attachment = vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
        let color_ref = vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
        let subpass = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(std::slice::from_ref(&color_ref));
        let dependencies = [
            vk::SubpassDependency::default()
                .src_subpass(vk::SUBPASS_EXTERNAL)
                .dst_subpass(0)
                .src_stage_mask(vk::PipelineStageFlags::TOP_OF_PIPE)
                .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE),
            vk::SubpassDependency::default()
                .src_subpass(0)
                .dst_subpass(vk::SUBPASS_EXTERNAL)
                .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_stage_mask(vk::PipelineStageFlags::TRANSFER)
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ),
        ];
        let render_pass_info = vk::RenderPassCreateInfo::default()
            .attachments(std::slice::from_ref(&attachment))
            .subpasses(std::slice::from_ref(&subpass))
            .dependencies(&dependencies);
        let render_pass = unsafe { device.create_render_pass(&render_pass_info, None) }?;

        let framebuffer_info = vk::FramebufferCreateInfo::default()
            .render_pass(render_pass)
            .attachments(std::slice::from_ref(&image_view))
            .width(width)
            .height(height)
            .layers(1);
        let framebuffer = unsafe { device.create_framebuffer(&framebuffer_info, None) }?;

        let vertex_module = shader_module(&device, FULLSCREEN_SPIRV)?;
        let fragment_module = shader_module(&device, SOLID_SPIRV)?;

        let push_constant = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(16);
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .push_constant_ranges(std::slice::from_ref(&push_constant));
        let pipeline_layout = unsafe { device.create_pipeline_layout(&layout_info, None) }?;

        let vertex_stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vertex_module)
            .name(c"main");
        let stages = [
            vertex_stage,
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fragment_module)
                .name(c"main"),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::Viewport::default()
            .x(0.0)
            .y(0.0)
            .width(width as f32)
            .height(height as f32)
            .min_depth(0.0)
            .max_depth(1.0);
        let scissor = vk::Rect2D::default()
            .offset(vk::Offset2D { x: 0, y: 0 })
            .extent(vk::Extent2D { width, height });
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewports(std::slice::from_ref(&viewport))
            .scissors(std::slice::from_ref(&scissor));
        let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .depth_clamp_enable(false)
            .rasterizer_discard_enable(false)
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(false)
            .color_write_mask(
                vk::ColorComponentFlags::R
                    | vk::ColorComponentFlags::G
                    | vk::ColorComponentFlags::B
                    | vk::ColorComponentFlags::A,
            );
        let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(std::slice::from_ref(&blend_attachment));
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default();
        let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterization)
            .multisample_state(&multisample)
            .color_blend_state(&color_blend)
            .layout(pipeline_layout)
            .render_pass(render_pass)
            .subpass(0)
            .depth_stencil_state(&depth_stencil);
        let pipeline = match unsafe {
            device.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
        } {
            Ok(pipelines) => pipelines[0],
            Err((_, result)) => return Err(result.into()),
        };

        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = unsafe { device.create_command_pool(&pool_info, None) }?;
        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let command_buffer = unsafe { device.allocate_command_buffers(&alloc_info) }?[0];

        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }?;

        Ok(Self {
            instance,
            device,
            queue,
            format,
            image,
            image_memory,
            image_view,
            buffer,
            buffer_memory,
            mapped,
            buffer_size,
            render_pass,
            framebuffer,
            pipeline_layout,
            pipeline,
            vertex_module,
            fragment_module,
            command_pool,
            command_buffer,
            fence,
            width,
            height,
            device_name,
            device_kind,
            _entry: entry,
        })
    }

    /// Clear the attachment with `color` (straight RGBA), read the pixels
    /// back, and return them premultiplied BGRA. In-flight 1: a single fence
    /// is waited on before the next submit.
    pub fn render(&mut self, color: [f32; 4]) -> Result<Vec<u8>, RenderError> {
        unsafe { self.device.reset_fences(&[self.fence]) }?;

        let begin_info = vk::CommandBufferBeginInfo::default();
        unsafe {
            self.device
                .begin_command_buffer(self.command_buffer, &begin_info)
        }?;

        // The draw consults the bound pipeline; without this every frame
        // faults the device (missing-pipeline-bind VUID).
        unsafe {
            self.device.cmd_bind_pipeline(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline,
            );
        }
        let color_bytes = unsafe { std::slice::from_raw_parts(color.as_ptr().cast::<u8>(), 16) };
        unsafe {
            self.device.cmd_push_constants(
                self.command_buffer,
                self.pipeline_layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                color_bytes,
            );
        }

        // ClearValue is a union in ash 0.38; only the color member is set.
        let clear = vk::ClearValue {
            color: vk::ClearColorValue { float32: color },
        };
        let render_pass_info = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(self.framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: self.width,
                    height: self.height,
                },
            })
            .clear_values(std::slice::from_ref(&clear));
        unsafe {
            self.device.cmd_begin_render_pass(
                self.command_buffer,
                &render_pass_info,
                vk::SubpassContents::INLINE,
            );
            self.device.cmd_draw(self.command_buffer, 3, 1, 0, 0);
            self.device.cmd_end_render_pass(self.command_buffer);
        }
        let region = vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .image_extent(vk::Extent3D {
                width: self.width,
                height: self.height,
                depth: 1,
            });
        unsafe {
            self.device.cmd_copy_image_to_buffer(
                self.command_buffer,
                self.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.buffer,
                std::slice::from_ref(&region),
            );
        }
        unsafe { self.device.end_command_buffer(self.command_buffer) }?;

        let submit =
            vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&self.command_buffer));
        unsafe { self.device.queue_submit(self.queue, &[submit], self.fence) }?;
        match unsafe {
            self.device
                .wait_for_fences(&[self.fence], true, FENCE_TIMEOUT_NS)
        } {
            Ok(()) => {}
            // The submit is still pending: this fence and this command
            // buffer must not be reused (a retry would reset a pending
            // fence and re-record a pending command buffer — VUID
            // violations). The caller treats FenceTimeout as fatal.
            Err(_) => return Err(RenderError::FenceTimeout),
        }

        // The fence guarantees the copy finished; the mapping is host-coherent
        // so no flush is needed.
        let bytes = unsafe { std::slice::from_raw_parts(self.mapped, self.buffer_size) };
        Ok(bgra_premultiplied(
            bytes,
            self.format == vk::Format::B8G8R8A8_UNORM,
        ))
    }

    /// A cheap health probe: create a minimal device and report the pick.
    pub fn probe(device_filter: Option<&str>) -> Result<ProbeReport, RenderError> {
        // SAFETY: see `new` — the entry is used only for Vulkan calls.
        let entry = unsafe { Entry::load() }
            .map_err(|e| RenderError::Vulkan(format!("load entry: {e}")))?;
        let app_name = c"kwe-scene-renderer";
        let app_info = vk::ApplicationInfo::default()
            .application_name(app_name)
            .application_version(vk::make_api_version(0, 0, 1, 0))
            .engine_name(app_name)
            .engine_version(vk::make_api_version(0, 0, 1, 0))
            .api_version(vk::API_VERSION_1_2);
        let create_info = vk::InstanceCreateInfo::default().application_info(&app_info);
        let instance = unsafe { entry.create_instance(&create_info, None) }?;
        let (physical, _, device_name, device_kind) = pick_device(&instance, device_filter)?;
        let format = pick_format(&instance, physical);
        let report = ProbeReport {
            device_name,
            device_kind,
            format: format_name(format),
        };
        unsafe { instance.destroy_instance(None) };
        Ok(report)
    }
}

impl Drop for ClearRenderer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.device
                .destroy_shader_module(self.fragment_module, None);
            self.device.destroy_shader_module(self.vertex_module, None);
            self.device.destroy_framebuffer(self.framebuffer, None);
            self.device.destroy_render_pass(self.render_pass, None);
            self.device.unmap_memory(self.buffer_memory);
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.buffer_memory, None);
            self.device.destroy_image_view(self.image_view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.image_memory, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// Pick the device: filter by name substring if given, then prefer discrete
/// GPUs, then any device with a graphics queue family.
fn pick_device(
    instance: &Instance,
    device_filter: Option<&str>,
) -> Result<(vk::PhysicalDevice, u32, String, String), RenderError> {
    let devices = unsafe { instance.enumerate_physical_devices() }?;
    if devices.is_empty() {
        return Err(RenderError::Vulkan("no Vulkan physical devices".into()));
    }
    let mut candidates: Vec<(vk::PhysicalDevice, u8, String, String)> = devices
        .into_iter()
        .filter_map(|physical| {
            let properties = unsafe { instance.get_physical_device_properties(physical) };
            let name = unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let families =
                unsafe { instance.get_physical_device_queue_family_properties(physical) };
            let has_graphics = families
                .iter()
                .any(|family| family.queue_flags.contains(vk::QueueFlags::GRAPHICS));
            if !has_graphics {
                return None;
            }
            let kind = device_kind_name(instance, physical);
            Some((physical, device_rank(properties.device_type), name, kind))
        })
        .collect();
    if candidates.is_empty() {
        return Err(RenderError::Vulkan(
            "no Vulkan device with a graphics queue".into(),
        ));
    }
    if let Some(filter) = device_filter {
        let needle = filter.to_ascii_lowercase();
        candidates.retain(|(_, _, name, _)| name.to_ascii_lowercase().contains(&needle));
        if candidates.is_empty() {
            return Err(RenderError::Vulkan(format!(
                "no Vulkan device matches --device {filter}"
            )));
        }
    }
    candidates.sort_by_key(|(_, rank, _, _)| *rank);
    let (physical, _, name, kind) = candidates.remove(0);
    let family = unsafe { instance.get_physical_device_queue_family_properties(physical) }
        .iter()
        .position(|family| family.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .ok_or_else(|| RenderError::Vulkan("no graphics queue family".into()))?
        as u32;
    Ok((physical, family, name, kind))
}

/// B8G8R8A8 when supported as a color attachment with transfer source reads;
/// R8G8B8A8 otherwise (the readback conversion handles the swap).
fn pick_format(instance: &Instance, physical: vk::PhysicalDevice) -> vk::Format {
    let needed = vk::FormatFeatureFlags::COLOR_ATTACHMENT | vk::FormatFeatureFlags::TRANSFER_SRC;
    let properties = unsafe {
        instance.get_physical_device_format_properties(physical, vk::Format::B8G8R8A8_UNORM)
    };
    if properties.optimal_tiling_features.contains(needed) {
        vk::Format::B8G8R8A8_UNORM
    } else {
        vk::Format::R8G8B8A8_UNORM
    }
}

fn device_kind_name(instance: &Instance, physical: vk::PhysicalDevice) -> String {
    let properties = unsafe { instance.get_physical_device_properties(physical) };
    match properties.device_type {
        vk::PhysicalDeviceType::DISCRETE_GPU => "discrete_gpu".into(),
        vk::PhysicalDeviceType::INTEGRATED_GPU => "integrated_gpu".into(),
        vk::PhysicalDeviceType::VIRTUAL_GPU => "virtual_gpu".into(),
        vk::PhysicalDeviceType::CPU => "cpu".into(),
        _ => "other".into(),
    }
}

fn format_name(format: vk::Format) -> String {
    match format {
        vk::Format::B8G8R8A8_UNORM => "B8G8R8A8_UNORM".into(),
        vk::Format::R8G8B8A8_UNORM => "R8G8B8A8_UNORM".into(),
        _ => format!("{format:?}"),
    }
}

fn allocate(
    instance: &Instance,
    device: &Device,
    physical: vk::PhysicalDevice,
    requirements: &vk::MemoryRequirements,
    flags: vk::MemoryPropertyFlags,
) -> Result<vk::DeviceMemory, RenderError> {
    let index = find_memory_type(instance, physical, requirements, flags)
        .or_else(|| {
            find_memory_type(
                instance,
                physical,
                requirements,
                vk::MemoryPropertyFlags::empty(),
            )
        })
        .ok_or_else(|| RenderError::Vulkan("no compatible memory type".into()))?;
    let info = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(index);
    unsafe { device.allocate_memory(&info, None) }.map_err(Into::into)
}

fn shader_module(device: &Device, code: &[u32]) -> Result<vk::ShaderModule, RenderError> {
    let info = vk::ShaderModuleCreateInfo::default().code(code);
    unsafe { device.create_shader_module(&info, None) }.map_err(Into::into)
}

/// The fullscreen triangle: gl_VertexIndex 0,1,2 -> (-1,-1), (3,-1), (-1,3)
/// with matching UVs; compiled with glslangValidator -V --target-env vulkan1.2
/// from shaders/fullscreen.vert.
#[rustfmt::skip]
const FULLSCREEN_SPIRV: &[u32] = &[
    0x07230203, 0x00010500, 0x0008000B, 0x00000031, 0x00000000, 0x00020011, 0x00000001, 0x0006000B,
    0x00000001, 0x4C534C47, 0x6474732E, 0x3035342E, 0x00000000, 0x0003000E, 0x00000000, 0x00000001,
    0x0008000F, 0x00000000, 0x00000004, 0x6E69616D, 0x00000000, 0x00000018, 0x0000001C, 0x00000029,
    0x00030003, 0x00000002, 0x000001C2, 0x00040005, 0x00000004, 0x6E69616D, 0x00000000, 0x00030005,
    0x0000000C, 0x00736F70, 0x00060005, 0x00000016, 0x505F6C67, 0x65567265, 0x78657472, 0x00000000,
    0x00060006, 0x00000016, 0x00000000, 0x505F6C67, 0x7469736F, 0x006E6F69, 0x00070006, 0x00000016,
    0x00000001, 0x505F6C67, 0x746E696F, 0x657A6953, 0x00000000, 0x00070006, 0x00000016, 0x00000002,
    0x435F6C67, 0x4470696C, 0x61747369, 0x0065636E, 0x00070006, 0x00000016, 0x00000003, 0x435F6C67,
    0x446C6C75, 0x61747369, 0x0065636E, 0x00030005, 0x00000018, 0x00000000, 0x00060005, 0x0000001C,
    0x565F6C67, 0x65747265, 0x646E4978, 0x00007865, 0x00040005, 0x00000029, 0x76755F76, 0x00000000,
    0x00030047, 0x00000016, 0x00000002, 0x00050048, 0x00000016, 0x00000000, 0x0000000B, 0x00000000,
    0x00050048, 0x00000016, 0x00000001, 0x0000000B, 0x00000001, 0x00050048, 0x00000016, 0x00000002,
    0x0000000B, 0x00000003, 0x00050048, 0x00000016, 0x00000003, 0x0000000B, 0x00000004, 0x00040047,
    0x0000001C, 0x0000000B, 0x0000002A, 0x00040047, 0x00000029, 0x0000001E, 0x00000000, 0x00020013,
    0x00000002, 0x00030021, 0x00000003, 0x00000002, 0x00030016, 0x00000006, 0x00000020, 0x00040017,
    0x00000007, 0x00000006, 0x00000002, 0x00040015, 0x00000008, 0x00000020, 0x00000000, 0x0004002B,
    0x00000008, 0x00000009, 0x00000003, 0x0004001C, 0x0000000A, 0x00000007, 0x00000009, 0x00040020,
    0x0000000B, 0x00000007, 0x0000000A, 0x0004002B, 0x00000006, 0x0000000D, 0xBF800000, 0x0005002C,
    0x00000007, 0x0000000E, 0x0000000D, 0x0000000D, 0x0004002B, 0x00000006, 0x0000000F, 0x40400000,
    0x0005002C, 0x00000007, 0x00000010, 0x0000000F, 0x0000000D, 0x0005002C, 0x00000007, 0x00000011,
    0x0000000D, 0x0000000F, 0x0006002C, 0x0000000A, 0x00000012, 0x0000000E, 0x00000010, 0x00000011,
    0x00040017, 0x00000013, 0x00000006, 0x00000004, 0x0004002B, 0x00000008, 0x00000014, 0x00000001,
    0x0004001C, 0x00000015, 0x00000006, 0x00000014, 0x0006001E, 0x00000016, 0x00000013, 0x00000006,
    0x00000015, 0x00000015, 0x00040020, 0x00000017, 0x00000003, 0x00000016, 0x0004003B, 0x00000017,
    0x00000018, 0x00000003, 0x00040015, 0x00000019, 0x00000020, 0x00000001, 0x0004002B, 0x00000019,
    0x0000001A, 0x00000000, 0x00040020, 0x0000001B, 0x00000001, 0x00000019, 0x0004003B, 0x0000001B,
    0x0000001C, 0x00000001, 0x00040020, 0x0000001E, 0x00000007, 0x00000007, 0x0004002B, 0x00000006,
    0x00000021, 0x00000000, 0x0004002B, 0x00000006, 0x00000022, 0x3F800000, 0x00040020, 0x00000026,
    0x00000003, 0x00000013, 0x00040020, 0x00000028, 0x00000003, 0x00000007, 0x0004003B, 0x00000028,
    0x00000029, 0x00000003, 0x0004002B, 0x00000006, 0x0000002D, 0x3F000000, 0x00050036, 0x00000002,
    0x00000004, 0x00000000, 0x00000003, 0x000200F8, 0x00000005, 0x0004003B, 0x0000000B, 0x0000000C,
    0x00000007, 0x0003003E, 0x0000000C, 0x00000012, 0x0004003D, 0x00000019, 0x0000001D, 0x0000001C,
    0x00050041, 0x0000001E, 0x0000001F, 0x0000000C, 0x0000001D, 0x0004003D, 0x00000007, 0x00000020,
    0x0000001F, 0x00050051, 0x00000006, 0x00000023, 0x00000020, 0x00000000, 0x00050051, 0x00000006,
    0x00000024, 0x00000020, 0x00000001, 0x00070050, 0x00000013, 0x00000025, 0x00000023, 0x00000024,
    0x00000021, 0x00000022, 0x00050041, 0x00000026, 0x00000027, 0x00000018, 0x0000001A, 0x0003003E,
    0x00000027, 0x00000025, 0x0004003D, 0x00000019, 0x0000002A, 0x0000001C, 0x00050041, 0x0000001E,
    0x0000002B, 0x0000000C, 0x0000002A, 0x0004003D, 0x00000007, 0x0000002C, 0x0000002B, 0x0005008E,
    0x00000007, 0x0000002E, 0x0000002C, 0x0000002D, 0x00050050, 0x00000007, 0x0000002F, 0x0000002D,
    0x0000002D, 0x00050081, 0x00000007, 0x00000030, 0x0000002E, 0x0000002F, 0x0003003E, 0x00000029,
    0x00000030, 0x000100FD, 0x00010038,
];

/// The solid-color fragment shader: out_color = push constant color.
#[rustfmt::skip]
const SOLID_SPIRV: &[u32] = &[
    0x07230203, 0x00010500, 0x0008000B, 0x00000012, 0x00000000, 0x00020011, 0x00000001, 0x0006000B,
    0x00000001, 0x4C534C47, 0x6474732E, 0x3035342E, 0x00000000, 0x0003000E, 0x00000000, 0x00000001,
    0x0007000F, 0x00000004, 0x00000004, 0x6E69616D, 0x00000000, 0x00000009, 0x0000000C, 0x00030010,
    0x00000004, 0x00000007, 0x00030003, 0x00000002, 0x000001C2, 0x00040005, 0x00000004, 0x6E69616D,
    0x00000000, 0x00050005, 0x00000009, 0x5F74756F, 0x6F6C6F63, 0x00000072, 0x00050005, 0x0000000A,
    0x68737550, 0x6F6C6F43, 0x00000072, 0x00050006, 0x0000000A, 0x00000000, 0x6F6C6F63, 0x00000072,
    0x00030005, 0x0000000C, 0x00006370, 0x00040047, 0x00000009, 0x0000001E, 0x00000000, 0x00030047,
    0x0000000A, 0x00000002, 0x00050048, 0x0000000A, 0x00000000, 0x00000023, 0x00000000, 0x00020013,
    0x00000002, 0x00030021, 0x00000003, 0x00000002, 0x00030016, 0x00000006, 0x00000020, 0x00040017,
    0x00000007, 0x00000006, 0x00000004, 0x00040020, 0x00000008, 0x00000003, 0x00000007, 0x0004003B,
    0x00000008, 0x00000009, 0x00000003, 0x0003001E, 0x0000000A, 0x00000007, 0x00040020, 0x0000000B,
    0x00000009, 0x0000000A, 0x0004003B, 0x0000000B, 0x0000000C, 0x00000009, 0x00040015, 0x0000000D,
    0x00000020, 0x00000001, 0x0004002B, 0x0000000D, 0x0000000E, 0x00000000, 0x00040020, 0x0000000F,
    0x00000009, 0x00000007, 0x00050036, 0x00000002, 0x00000004, 0x00000000, 0x00000003, 0x000200F8,
    0x00000005, 0x00050041, 0x0000000F, 0x00000010, 0x0000000C, 0x0000000E, 0x0004003D, 0x00000007,
    0x00000011, 0x00000010, 0x0003003E, 0x00000009, 0x00000011, 0x000100FD, 0x00010038,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_premultiplied_exact_bytes() {
        // B8G8R8A8 readback is already in protocol order: identity with
        // premultiplication (opaque alpha keeps the bytes).
        assert_eq!(
            bgra_premultiplied(&[0, 128, 255, 255], true),
            vec![0, 128, 255, 255]
        );
        // R8G8B8A8 readback stores R,G,B,A: the first and third channels
        // swap to reach B,G,R,A.
        assert_eq!(
            bgra_premultiplied(&[0, 128, 255, 255], false),
            vec![255, 128, 0, 255]
        );
        // a=0: everything premultiplied to zero.
        assert_eq!(bgra_premultiplied(&[99, 88, 77, 0], true), vec![0, 0, 0, 0]);
        assert_eq!(
            bgra_premultiplied(&[99, 88, 77, 0], false),
            vec![0, 0, 0, 0]
        );
        // a=85 (=1/3): B=3*85/255=1, G=6*85/255=2, R=9*85/255=3 exactly.
        assert_eq!(bgra_premultiplied(&[3, 6, 9, 85], true), vec![1, 2, 3, 85]);
        // Same bytes as R8G8B8A8: B=9*85/255=3, G=2, R=1.
        assert_eq!(bgra_premultiplied(&[3, 6, 9, 85], false), vec![3, 2, 1, 85]);
        // Rounding to nearest: (255*128 + 127) / 255 = 128. B8G8R8A8 keeps
        // the first channel's value at 128; R8G8B8A8 moves it to the last.
        assert_eq!(
            bgra_premultiplied(&[255, 0, 0, 128], true),
            vec![128, 0, 0, 128]
        );
        assert_eq!(
            bgra_premultiplied(&[255, 0, 0, 128], false),
            vec![0, 0, 128, 128]
        );
    }

    #[test]
    fn bgra_premultiplied_length_preserved() {
        let input = vec![10_u8, 20, 30, 255, 40, 50, 60, 128];
        assert_eq!(bgra_premultiplied(&input, true).len(), input.len());
        assert_eq!(bgra_premultiplied(&input, false).len(), input.len());
    }

    /// One full offscreen render through the real pipeline and readback: the
    /// worker machinery (QuickJS, mmap) is not involved, so this catches
    /// pipeline/command-regression faults (a device-lost draw and a teardown
    /// SIGSEGV were found here). Skip-by-default: needs a Vulkan device —
    /// run with KWE_TEST_DEVICE set (e.g. `KWE_TEST_DEVICE=llvmpipe`) and
    /// VK_ICD_FILENAMES pointing at an ICD JSON for the software lane.
    #[test]
    fn isolated_draw() {
        let Ok(binding) = std::env::var("KWE_TEST_DEVICE") else {
            eprintln!("isolated_draw: skipped (set KWE_TEST_DEVICE to run)");
            return;
        };
        let mut renderer = ClearRenderer::new(Some(&binding), 64, 48).expect("create renderer");
        let pixels = renderer.render([0.1, 0.2, 0.3, 1.0]).expect("render once");
        assert_eq!(pixels.len(), 64 * 48 * 4);
        // The fullscreen triangle covers the whole attachment with the push
        // constant color (r=0.1, g=0.2, b=0.3, a=1.0). The B8G8R8A8 readback
        // is already B,G,R,A in memory order, so the frame bytes are
        // B=0.3*255, G=0.2*255, R=0.1*255, A=255 (rounding varies by driver).
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(pixel[3], 255);
            assert!((76..=77).contains(&pixel[0]), "B={}", pixel[0]);
            assert_eq!(pixel[1], 51);
            assert!((25..=26).contains(&pixel[2]), "R={}", pixel[2]);
        }
        // The drop must not fault: the renderer's entry keeps the loader
        // mapped through the destroy calls.
    }
}
