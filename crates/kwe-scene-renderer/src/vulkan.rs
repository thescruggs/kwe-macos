// SPDX-License-Identifier: Apache-2.0
// Original offscreen Vulkan compositor for the M3a..M3c slices (ADR 0001).
//
// No window, no swapchain, no extensions: a Vulkan 1.2 instance, the first
// physical device with a graphics queue (--device filters by name substring,
// discrete GPUs preferred, llvmpipe works for the test lane), and a W x H
// COLOR_OPTIMAL image that is cleared every frame. The M3c slice replaced
// the M3a fullscreen-triangle clear pass with a textured-quad pipeline: one
// unit-quad vertex buffer (6 verts of pos+uv, two fan-ordered triangles),
// a per-layer combined image sampler from a bounded per-layer descriptor
// set pool (at most MAX_LAYERS layers, each its own set), and 48 bytes of
// push constants per draw — m0 = (a, c, tx, 0), m1 = (b, d, ty, alpha),
// viewport = (w, h, 0, 0) — so world = mat2(m0.xy, m1.xy)·pos + (m0.z, m1.z)
// with alpha carried to the fragment shader (see layers.rs for the model
// math and docs/SCENE_FORMAT_V1.md for the format). Layers draw in
// scene.json order with src-over blending (the pipeline's blend state; M3d
// brings blend modes). The clear is a CLEAR_VALUE, not a draw.
//
// Frame orientation is an empirical contract: the delivered frames are
// upright on both tested drivers (NVIDIA RTX 3070 and llvmpipe), pinned by
// the quad_orientation device test and the smoke suite's layer oracles on
// both lanes. How an OPTIMAL-tiling color attachment comes back in the
// readback is driver-dependent (not specified by Vulkan), so a new driver
// MUST be re-verified against those oracles before it is declared
// supported — the vertex shader applies no y-flip (quad.vert documents
// this).
//
// Textures are R8G8B8A8_UNORM (RGBA8, identity channel order — the M3a
// readback lesson), sampled with a linear clamp-to-edge sampler. Uploads
// go through a per-upload host-visible staging buffer + one-shot transfer
// command buffer, waited to completion before the texture is used; each
// upload is bounded (textures.rs caps) and a failed upload skips the layer
// without touching the renderer's health.
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

use crate::layers::{BlendMode, LayerDraw, MAX_LAYERS};

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

/// One uploaded layer texture: the sampled image plus its view. The
/// descriptor set referencing it lives in `descriptor_sets`.
struct LayerTexture {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

pub struct LayerRenderer {
    instance: Instance,
    device: Device,
    /// The picked physical device; kept for memory allocation decisions
    /// (uploads allocate per-layer, so the probe results must outlive
    /// `new_with`).
    physical: vk::PhysicalDevice,
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
    /// One pipeline per implemented blend mode (M3d), indexed by
    /// BlendMode::variant_index — all sharing this layout, the render pass,
    /// and the descriptor sets; only the blend attachment state differs.
    pipelines: Vec<vk::Pipeline>,
    vertex_module: vk::ShaderModule,
    fragment_module: vk::ShaderModule,
    /// Unit quad: 6 × (pos vec2, uv vec2) — see UNIT_QUAD.
    vertex_buffer: vk::Buffer,
    vertex_buffer_memory: vk::DeviceMemory,
    /// Linear clamp-to-edge sampler shared by every layer texture.
    sampler: vk::Sampler,
    descriptor_set_layout: vk::DescriptorSetLayout,
    /// Bounded pool: at most MAX_LAYERS sets of one combined image sampler.
    descriptor_pool: vk::DescriptorPool,
    /// One descriptor set per layer index; None until the layer uploaded.
    descriptor_sets: Vec<Option<vk::DescriptorSet>>,
    /// Uploaded textures per layer index; None = skipped at load or failed
    /// upload.
    textures: Vec<Option<LayerTexture>>,
    command_pool: vk::CommandPool,
    /// Per-frame command buffer.
    command_buffer: vk::CommandBuffer,
    /// One-shot upload command buffer (uploads complete before any render
    /// submits, so the single fence serializes them).
    upload_buffer: vk::CommandBuffer,
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

impl LayerRenderer {
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

        // Host-visible staging buffer for the readback. Requiring
        // HOST_VISIBLE here (no empty-flags fallback) is the point: a
        // non-host-visible type would fail at map time, mid-frame.
        let buffer_size = width as usize * height as usize * 4;
        let buffer_info = vk::BufferCreateInfo::default()
            .size(buffer_size as vk::DeviceSize)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { device.create_buffer(&buffer_info, None) }?;
        let buffer_requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
        let buffer_memory =
            allocate_host_visible(&instance, &device, physical, &buffer_requirements)?;
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

        let vertex_module = shader_module(&device, QUAD_SPIRV)?;
        let fragment_module = shader_module(&device, TEXTURE_SPIRV)?;

        // The unit quad (pos + uv). The uv origin is the image's top-left
        // corner (row 0 = the top of the picture), which in scene space
        // (+y down) is the smaller y — so v=0 sits at pos.y = -0.5. The
        // DELIVERED frame's orientation is an empirical contract, not a
        // chain of inference: frames are upright on both tested drivers
        // (NVIDIA RTX 3070 and llvmpipe), pinned by quad_orientation and
        // the smoke suite's layer oracles on both lanes; OPTIMAL-tiling
        // readback orientation is driver-dependent and must be re-verified
        // per driver. The buffer size and map range are the BYTE count
        // (64): UNIT_QUAD.len() is the f32 element count (16), and an
        // element/byte mix-up made the buffer 16 bytes — the GPU's read of
        // vertices 2..5 ran out of bounds and the second triangle
        // rasterized garbage (found via the isolated_draw probe).
        let vertex_bytes = (UNIT_QUAD.len() * std::mem::size_of::<f32>()) as vk::DeviceSize;
        let vertex_info = vk::BufferCreateInfo::default()
            .size(vertex_bytes)
            .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let vertex_buffer = unsafe { device.create_buffer(&vertex_info, None) }?;
        let vertex_requirements = unsafe { device.get_buffer_memory_requirements(vertex_buffer) };
        let vertex_buffer_memory =
            allocate_host_visible(&instance, &device, physical, &vertex_requirements)?;
        unsafe { device.bind_buffer_memory(vertex_buffer, vertex_buffer_memory, 0) }?;
        unsafe {
            std::ptr::copy_nonoverlapping(
                UNIT_QUAD.as_ptr(),
                device
                    .map_memory(
                        vertex_buffer_memory,
                        0,
                        vertex_bytes,
                        vk::MemoryMapFlags::empty(),
                    )?
                    .cast::<f32>(),
                UNIT_QUAD.len(),
            );
            device.unmap_memory(vertex_buffer_memory);
        }

        // One linear clamp-to-edge sampler for every layer texture.
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .max_lod(0.25); // single mip level: the LOD clamps to 0
        let sampler = unsafe { device.create_sampler(&sampler_info, None) }?;

        // Per-layer descriptor sets: at most MAX_LAYERS, one combined image
        // sampler each (the bounded table the draw list indexes into).
        let set_layout_binding = vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT);
        let set_layout_info = vk::DescriptorSetLayoutCreateInfo::default()
            .bindings(std::slice::from_ref(&set_layout_binding));
        let descriptor_set_layout =
            unsafe { device.create_descriptor_set_layout(&set_layout_info, None) }?;
        let pool_size = vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(MAX_LAYERS as u32);
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(MAX_LAYERS as u32)
            .pool_sizes(std::slice::from_ref(&pool_size));
        let descriptor_pool = unsafe { device.create_descriptor_pool(&pool_info, None) }?;

        // 64 bytes of per-draw push constants shared by both stages. The
        // layout grew from 48 to 64 bytes for M3d; the first 48 bytes are
        // unchanged (the vertex shader reads only m0/m1/viewport, so
        // quad.vert is untouched) and a new `effects` vec4 lands at offset
        // 48, read by the fragment shader:
        //   m0 = (a, c, tx, 0)              column 0 + translate x
        //   m1 = (b, d, ty, alpha·tint.a)   column 1 + translate y + the
        //                                   layer alpha folded with the
        //                                   tint alpha (M3d)
        //   viewport = (w, h, 0, 0)         scene size in pixels
        //   effects = (brightness, tint.r, tint.g, tint.b)  (M3d)
        let push_constant = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(64);
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(std::slice::from_ref(&descriptor_set_layout))
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
        let binding = vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(16)
            .input_rate(vk::VertexInputRate::VERTEX);
        let attributes = [
            vk::VertexInputAttributeDescription::default()
                .binding(0)
                .location(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(0),
            vk::VertexInputAttributeDescription::default()
                .binding(0)
                .location(1)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(8),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(std::slice::from_ref(&binding))
            .vertex_attribute_descriptions(&attributes);
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
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default();

        // M3d: one pipeline per implemented blend mode, sharing the layout,
        // the render pass, and every other state — only the blend
        // attachment differs (see blend_attachment_for). The draw loop
        // binds the variant of the layer's (clamped) blend_mode.
        let mut pipelines = Vec::with_capacity(BlendMode::ALL.len());
        for mode in BlendMode::ALL {
            let blend_attachment = blend_attachment_for(mode);
            let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
                .attachments(std::slice::from_ref(&blend_attachment));
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
            pipelines.push(pipeline);
        }

        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = unsafe { device.create_command_pool(&pool_info, None) }?;
        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(2);
        let buffers = unsafe { device.allocate_command_buffers(&alloc_info) }?;
        let command_buffer = buffers[0];
        let upload_buffer = buffers[1];

        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }?;

        Ok(Self {
            instance,
            device,
            physical,
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
            pipelines,
            vertex_module,
            fragment_module,
            vertex_buffer,
            vertex_buffer_memory,
            sampler,
            descriptor_set_layout,
            descriptor_pool,
            descriptor_sets: Vec::new(),
            textures: Vec::new(),
            command_pool,
            command_buffer,
            upload_buffer,
            fence,
            width,
            height,
            device_name,
            device_kind,
            _entry: entry,
        })
    }

    /// Upload one layer's RGBA8 texture (R8G8B8A8_UNORM — identity channel
    /// order) into a device-local sampled image and bind its descriptor set.
    /// Bounded by the caller (textures.rs caps: ≤ 64 MiB, ≤ 8192², ≤ 16.7M
    /// pixels); a failed upload returns an error and the caller skips the
    /// layer — the renderer stays healthy. The staging buffer is
    /// host-visible and per-upload; the single fence is waited to
    /// completion before the texture is used, and no render submits are in
    /// flight during startup uploads, so sharing it is safe.
    pub fn upload_layer(
        &mut self,
        index: usize,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<(), RenderError> {
        if index >= MAX_LAYERS {
            return Err(RenderError::Vulkan(format!(
                "layer index {index} beyond the {MAX_LAYERS} layer cap"
            )));
        }
        let expected = width as usize * height as usize * 4;
        if rgba.len() != expected {
            return Err(RenderError::Vulkan(format!(
                "texture byte count {} does not match {width}x{height} RGBA8",
                rgba.len()
            )));
        }

        // Build + upload; every handle created here is tracked so a failure
        // cleans up and the caller skips the layer — including the staging
        // buffer/memory, whose per-upload size can reach the 64 MiB image
        // cap (a leak across the 256 layers would hold 16 GiB).
        let mut image: Option<vk::Image> = None;
        let mut image_memory: Option<vk::DeviceMemory> = None;
        let mut view: Option<vk::ImageView> = None;
        let mut set: Option<vk::DescriptorSet> = None;
        let mut staging: Option<vk::Buffer> = None;
        let mut staging_memory: Option<vk::DeviceMemory> = None;
        let mut staging_mapped = false;
        let outcome = (|| -> Result<(), RenderError> {
            let image_info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk::Format::R8G8B8A8_UNORM)
                .extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED);
            let created = unsafe { self.device.create_image(&image_info, None) }?;
            image = Some(created);
            let requirements = unsafe { self.device.get_image_memory_requirements(created) };
            let memory = allocate(
                &self.instance,
                &self.device,
                self.physical,
                &requirements,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )?;
            image_memory = Some(memory);
            unsafe { self.device.bind_image_memory(created, memory, 0) }?;

            // Staging: host-visible, copied, then freed after the fence.
            let staging_info = vk::BufferCreateInfo::default()
                .size(rgba.len() as vk::DeviceSize)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let created_staging = unsafe { self.device.create_buffer(&staging_info, None) }?;
            staging = Some(created_staging);
            let staging_requirements =
                unsafe { self.device.get_buffer_memory_requirements(created_staging) };
            let created_staging_memory = allocate_host_visible(
                &self.instance,
                &self.device,
                self.physical,
                &staging_requirements,
            )?;
            staging_memory = Some(created_staging_memory);
            unsafe {
                self.device
                    .bind_buffer_memory(created_staging, created_staging_memory, 0)
            }?;
            let staging_map = unsafe {
                self.device.map_memory(
                    created_staging_memory,
                    0,
                    rgba.len() as vk::DeviceSize,
                    vk::MemoryMapFlags::empty(),
                )?
            }
            .cast::<u8>();
            staging_mapped = true;
            unsafe { std::ptr::copy_nonoverlapping(rgba.as_ptr(), staging_map, rgba.len()) };

            unsafe { self.device.reset_fences(&[self.fence]) }?;
            let begin_info = vk::CommandBufferBeginInfo::default();
            unsafe {
                self.device
                    .begin_command_buffer(self.upload_buffer, &begin_info)
            }?;
            // UNDEFINED -> TRANSFER_DST_OPTIMAL. The access masks make the
            // transition a real memory dependency (zero masks would be an
            // execution-only barrier): srcAccess TRANSFER_WRITE orders any
            // prior transfer writes before the layout change, and
            // dstAccess TRANSFER_WRITE orders the layout change before the
            // copy below.
            let to_transfer = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(created)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });
            unsafe {
                self.device.cmd_pipeline_barrier(
                    self.upload_buffer,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    std::slice::from_ref(&to_transfer),
                );
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
                    width,
                    height,
                    depth: 1,
                });
            unsafe {
                self.device.cmd_copy_buffer_to_image(
                    self.upload_buffer,
                    created_staging,
                    created,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    std::slice::from_ref(&region),
                );
            }
            // TRANSFER_DST_OPTIMAL -> SHADER_READ_ONLY_OPTIMAL. The access
            // masks give the draw's sampled-image reads the spec's
            // visibility guarantee: TRANSFER_WRITE (the copy) is ordered
            // before SHADER_READ (the fragment-sampler accesses in the
            // render pass).
            let to_shader = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(created)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });
            unsafe {
                self.device.cmd_pipeline_barrier(
                    self.upload_buffer,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    std::slice::from_ref(&to_shader),
                );
            }
            unsafe { self.device.end_command_buffer(self.upload_buffer) }?;
            let submit = vk::SubmitInfo::default()
                .command_buffers(std::slice::from_ref(&self.upload_buffer));
            unsafe { self.device.queue_submit(self.queue, &[submit], self.fence) }?;
            match unsafe {
                self.device
                    .wait_for_fences(&[self.fence], true, FENCE_TIMEOUT_NS)
            } {
                Ok(()) => {}
                Err(_) => return Err(RenderError::FenceTimeout),
            }

            unsafe {
                self.device.unmap_memory(created_staging_memory);
                self.device.destroy_buffer(created_staging, None);
                self.device.free_memory(created_staging_memory, None);
            }

            let view_info = vk::ImageViewCreateInfo::default()
                .image(created)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(vk::Format::R8G8B8A8_UNORM)
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
            let created_view = unsafe { self.device.create_image_view(&view_info, None) }?;
            view = Some(created_view);

            let alloc_info = vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(self.descriptor_pool)
                .set_layouts(std::slice::from_ref(&self.descriptor_set_layout));
            let created_set = unsafe { self.device.allocate_descriptor_sets(&alloc_info) }?[0];
            let image_info = vk::DescriptorImageInfo::default()
                .sampler(self.sampler)
                .image_view(created_view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
            let write = vk::WriteDescriptorSet::default()
                .dst_set(created_set)
                .dst_binding(0)
                .dst_array_element(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&image_info));
            unsafe { self.device.update_descriptor_sets(&[write], &[]) };
            set = Some(created_set);
            Ok(())
        })();
        if let Err(error) = outcome {
            // Every error here happens before or after the submit's fence
            // completed (the submit either succeeded and the fence was
            // waited, or it never ran); FenceTimeout — the one error with a
            // still-pending submit — is the process-fatal path, so freeing
            // while the GPU might still read the staging is bounded to the
            // exit window. The staging is destroyed on ALL other paths so a
            // skipped layer never holds up to 64 MiB of host memory.
            if let Some(memory) = staging_memory.filter(|_| staging_mapped) {
                unsafe { self.device.unmap_memory(memory) };
            }

            if let Some(buffer) = staging {
                unsafe { self.device.destroy_buffer(buffer, None) };
            }
            if let Some(memory) = staging_memory {
                unsafe { self.device.free_memory(memory, None) };
            }
            if let Some(created_view) = view {
                unsafe { self.device.destroy_image_view(created_view, None) };
            }
            if let Some(created) = image {
                unsafe { self.device.destroy_image(created, None) };
            }
            if let Some(memory) = image_memory {
                unsafe { self.device.free_memory(memory, None) };
            }
            return Err(error);
        }
        while self.textures.len() <= index {
            self.textures.push(None);
            self.descriptor_sets.push(None);
        }
        self.textures[index] = Some(LayerTexture {
            image: image.expect("upload succeeded"),
            memory: image_memory.expect("upload succeeded"),
            view: view.expect("upload succeeded"),
        });
        self.descriptor_sets[index] = set;
        Ok(())
    }

    /// Clear the attachment with `color` (straight RGBA), draw the given
    /// layers in order (scene.json order, src-over blending), read the
    /// pixels back, and return them premultiplied BGRA. In-flight 1: a
    /// single fence is waited on before the next submit.
    pub fn render(&mut self, clear: [f32; 4], draws: &[LayerDraw]) -> Result<Vec<u8>, RenderError> {
        unsafe { self.device.reset_fences(&[self.fence]) }?;

        let begin_info = vk::CommandBufferBeginInfo::default();
        unsafe {
            self.device
                .begin_command_buffer(self.command_buffer, &begin_info)
        }?;

        unsafe {
            self.device.cmd_bind_vertex_buffers(
                self.command_buffer,
                0,
                std::slice::from_ref(&self.vertex_buffer),
                &[0],
            );
        }

        // ClearValue is a union in ash 0.38; only the color member is set.
        let clear_value = vk::ClearValue {
            color: vk::ClearColorValue { float32: clear },
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
            .clear_values(std::slice::from_ref(&clear_value));
        unsafe {
            self.device.cmd_begin_render_pass(
                self.command_buffer,
                &render_pass_info,
                vk::SubpassContents::INLINE,
            );
        }
        for draw in draws {
            // A draw whose texture never uploaded (skipped at load) is
            // silently dropped here — the draw list builder already skips
            // it, so this is only a defense.
            let Some(set) = self
                .descriptor_sets
                .get(draw.layer_index)
                .copied()
                .flatten()
            else {
                continue;
            };
            // M3d: the draw's pipeline variant — the layer's blend mode
            // (clamped to the implemented set at every boundary, so the
            // variant index is always in range). Binding inside the pass
            // per draw is what makes per-layer blend modes work.
            let variant = draw.blend_mode.variant_index();
            unsafe {
                self.device.cmd_bind_pipeline(
                    self.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipelines[variant],
                );
            }
            // The push constant layout (shared by both stages, 64 bytes):
            // m0 = (a, c, tx, 0), m1 = (b, d, ty, alpha·tint.a),
            // viewport = (w, h, 0, 0), effects = (brightness, tint.rgb) —
            // see the vertex shader's PC block and the layout comment in
            // `new`. The tint alpha is folded into m1.w so the fragment
            // shader's single alpha scale covers the layer alpha and the
            // tint alpha together (multiplication is commutative, so the
            // math is exact).
            let push: [f32; 16] = [
                draw.m[0][0],
                draw.m[1][0],
                draw.t[0],
                0.0,
                draw.m[0][1],
                draw.m[1][1],
                draw.t[1],
                draw.alpha * draw.tint[3],
                self.width as f32,
                self.height as f32,
                0.0,
                0.0,
                draw.brightness,
                draw.tint[0],
                draw.tint[1],
                draw.tint[2],
            ];
            let push_bytes = unsafe { std::slice::from_raw_parts(push.as_ptr().cast::<u8>(), 64) };
            unsafe {
                self.device.cmd_push_constants(
                    self.command_buffer,
                    self.pipeline_layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    push_bytes,
                );
                self.device.cmd_bind_descriptor_sets(
                    self.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_layout,
                    0,
                    std::slice::from_ref(&set),
                    &[],
                );
                // The quad is two fan-ordered triangles in the vertex
                // buffer ([v0,v1,v2, v0,v2,v3]); one 6-vertex TRIANGLE_LIST
                // draw emits both. The original half-quad bug was NOT the
                // draw shape — it was the vertex-buffer size: an
                // element/byte mix-up sized the buffer at 16 bytes (one
                // vertex), so the GPU's reads of vertices 2..5 ran out of
                // bounds and the second triangle rasterized garbage
                // (found via the isolated_draw probe; see `new`).
                self.device.cmd_draw(self.command_buffer, 6, 1, 0, 0);
            }
        }
        unsafe {
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
        let out = bgra_premultiplied(bytes, self.format == vk::Format::B8G8R8A8_UNORM);
        Ok(out)
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

/// The blend attachment state for one implemented blend mode (M3d). The
/// per-mode factors/ops implement the researched WE semantics
/// (docs/SCENE_FORMAT_V1.md, M3d section): WE applies its modes as
/// Photoshop-style COLOR operations (ApplyBlending), and our alpha policy
/// is deliberate — **the mode acts on the color; the alpha channel always
/// composites src-over, except Add which is additive on both channels** —
/// so the layer's own opacity still matters under every mode. The fragment
/// shader outputs STRAIGHT color, so the attachment stores the straight
/// composite and bgra_premultiplied applies the ONE premultiplication at
/// the readback boundary. For Normal, using SRC_ALPHA as the color factor
/// would store an already-premultiplied composite and the readback would
/// premultiply AGAIN, darkening translucent pixels by alpha/255 (the
/// (79,58,36,191) double-premultiplied oracle vs the correct
/// (106,77,48,191)); scaling the alpha channel by itself (SRC_ALPHA as
/// its src factor) would double-scale it (a 191/255 layer over an opaque
/// dst would land at 143/255 instead of staying opaque).
///
/// | Mode | Color (src, dst, op) | Alpha (src, dst, op) |
/// |---|---|---|
/// | Normal (src-over) | ONE, ONE_MINUS_SRC_ALPHA, ADD | ONE, ONE_MINUS_SRC_ALPHA, ADD |
/// | Multiply (texel·background/255) | DST_COLOR, ZERO, ADD | ONE, ONE_MINUS_SRC_ALPHA, ADD |
/// | Add (texel+background, saturating) | ONE, ONE, ADD | ONE, ONE, ADD |
/// | Screen (texel·(1−background)+background) | ONE_MINUS_DST_COLOR, ONE, ADD | ONE, ONE_MINUS_SRC_ALPHA, ADD |
/// | Subtract (max(0, background−texel)) | ONE, ONE, REVERSE_SUBTRACT | ONE, ONE_MINUS_SRC_ALPHA, ADD |
///
/// Multiply's color factors are the spec's pinned formula
/// texel·background/255 — a "hard" multiply that ignores the source alpha
/// (a translucent multiply over a transparent backdrop is black × its
/// alpha, and over an opaque one the color is fully hard-multiplied); the
/// src-over ALPHA keeps the layer's own opacity in the delivered alpha
/// instead of discarding it — the review-fixed (ZERO, ONE) dropped the
/// layer's alpha entirely, so a translucent multiply over a transparent
/// backdrop vanished and over an opaque one delivered a fully opaque
/// composite regardless of the layer's opacity. Screen's color factors
/// are NOT symmetric — (ONE, ONE_MINUS_DST_COLOR) would compute texel +
/// background·(1−background), which a device oracle caught. Subtract is
/// REVERSE_SUBTRACT (dst − src) clamped to 0 by the operation, Photoshop's
/// base − blend, the spec's max(0, c2−c1) with c1 = texel, c2 = background.
fn blend_attachment_for(mode: BlendMode) -> vk::PipelineColorBlendAttachmentState {
    let write_mask = vk::ColorComponentFlags::R
        | vk::ColorComponentFlags::G
        | vk::ColorComponentFlags::B
        | vk::ColorComponentFlags::A;
    let (src_color, dst_color, color_op, src_alpha, dst_alpha, alpha_op) = match mode {
        BlendMode::Normal => (
            vk::BlendFactor::ONE,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
            vk::BlendOp::ADD,
            vk::BlendFactor::ONE,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
            vk::BlendOp::ADD,
        ),
        BlendMode::Multiply => (
            vk::BlendFactor::DST_COLOR,
            vk::BlendFactor::ZERO,
            vk::BlendOp::ADD,
            vk::BlendFactor::ONE,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
            vk::BlendOp::ADD,
        ),
        BlendMode::Add => (
            vk::BlendFactor::ONE,
            vk::BlendFactor::ONE,
            vk::BlendOp::ADD,
            vk::BlendFactor::ONE,
            vk::BlendFactor::ONE,
            vk::BlendOp::ADD,
        ),
        BlendMode::Screen => (
            vk::BlendFactor::ONE_MINUS_DST_COLOR,
            vk::BlendFactor::ONE,
            vk::BlendOp::ADD,
            vk::BlendFactor::ONE,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
            vk::BlendOp::ADD,
        ),
        BlendMode::Subtract => (
            vk::BlendFactor::ONE,
            vk::BlendFactor::ONE,
            vk::BlendOp::REVERSE_SUBTRACT,
            vk::BlendFactor::ONE,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
            vk::BlendOp::ADD,
        ),
    };
    vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(src_color)
        .dst_color_blend_factor(dst_color)
        .color_blend_op(color_op)
        .src_alpha_blend_factor(src_alpha)
        .dst_alpha_blend_factor(dst_alpha)
        .alpha_blend_op(alpha_op)
        .color_write_mask(write_mask)
}

impl Drop for LayerRenderer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.command_pool, None);
            for pipeline in &self.pipelines {
                self.device.destroy_pipeline(*pipeline, None);
            }
            // The descriptor pool frees the sets; the layout outlives the
            // pool so the sets never reference a destroyed layout.
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.device
                .destroy_descriptor_pool(self.descriptor_pool, None);
            self.device
                .destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            self.device.destroy_sampler(self.sampler, None);
            self.device
                .destroy_shader_module(self.fragment_module, None);
            self.device.destroy_shader_module(self.vertex_module, None);
            self.device.destroy_framebuffer(self.framebuffer, None);
            self.device.destroy_render_pass(self.render_pass, None);
            self.device.destroy_buffer(self.vertex_buffer, None);
            self.device.free_memory(self.vertex_buffer_memory, None);
            self.device.unmap_memory(self.buffer_memory);
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.buffer_memory, None);
            self.device.destroy_image_view(self.image_view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.image_memory, None);
            for texture in self.textures.iter().flatten() {
                self.device.destroy_image_view(texture.view, None);
                self.device.destroy_image(texture.image, None);
                self.device.free_memory(texture.memory, None);
            }
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

/// Host-visible memory for buffers that are mapped (the readback staging and
/// the per-upload staging). Requiring HOST_VISIBLE here — no empty-flags
/// fallback — is the point: a non-host-visible type would only fail at map
/// time, mid-frame. HOST_COHERENT is preferred so no flush is needed.
fn allocate_host_visible(
    instance: &Instance,
    device: &Device,
    physical: vk::PhysicalDevice,
    requirements: &vk::MemoryRequirements,
) -> Result<vk::DeviceMemory, RenderError> {
    let visible = vk::MemoryPropertyFlags::HOST_VISIBLE;
    let index = find_memory_type(
        instance,
        physical,
        requirements,
        visible | vk::MemoryPropertyFlags::HOST_COHERENT,
    )
    .or_else(|| find_memory_type(instance, physical, requirements, visible))
    .ok_or_else(|| RenderError::Vulkan("no host-visible memory type".into()))?;
    let info = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(index);
    unsafe { device.allocate_memory(&info, None) }.map_err(Into::into)
}

fn shader_module(device: &Device, code: &[u32]) -> Result<vk::ShaderModule, RenderError> {
    let info = vk::ShaderModuleCreateInfo::default().code(code);
    unsafe { device.create_shader_module(&info, None) }.map_err(Into::into)
}

/// The M3c layer-quad vertex shader: the unit quad (pos, uv) transformed
/// by the per-layer push-constant model matrix; compiled with
/// glslangValidator -V --target-env vulkan1.2 from shaders/quad.vert.
#[rustfmt::skip]
const QUAD_SPIRV: &[u32] = &[
    0x07230203, 0x00010500, 0x0008000b, 0x00000047, 0x00000000, 0x00020011, 0x00000001, 0x0006000b,
    0x00000001, 0x4c534c47, 0x6474732e, 0x3035342e, 0x00000000, 0x0003000e, 0x00000000, 0x00000001,
    0x000a000f, 0x00000000, 0x00000004, 0x6e69616d, 0x00000000, 0x0000000d, 0x00000023, 0x0000003c,
    0x00000044, 0x00000045, 0x00030003, 0x00000002, 0x000001c2, 0x00040005, 0x00000004, 0x6e69616d,
    0x00000000, 0x00040005, 0x00000009, 0x6c726f77, 0x00000064, 0x00030005, 0x0000000b, 0x00004350,
    0x00040006, 0x0000000b, 0x00000000, 0x0000306d, 0x00040006, 0x0000000b, 0x00000001, 0x0000316d,
    0x00060006, 0x0000000b, 0x00000002, 0x77656976, 0x74726f70, 0x00000000, 0x00030005, 0x0000000d,
    0x00006370, 0x00040005, 0x00000023, 0x736f5061, 0x00000000, 0x00030005, 0x0000002f, 0x0063646e,
    0x00060005, 0x0000003a, 0x505f6c67, 0x65567265, 0x78657472, 0x00000000, 0x00060006, 0x0000003a,
    0x00000000, 0x505f6c67, 0x7469736f, 0x006e6f69, 0x00070006, 0x0000003a, 0x00000001, 0x505f6c67,
    0x746e696f, 0x657a6953, 0x00000000, 0x00070006, 0x0000003a, 0x00000002, 0x435f6c67, 0x4470696c,
    0x61747369, 0x0065636e, 0x00070006, 0x0000003a, 0x00000003, 0x435f6c67, 0x446c6c75, 0x61747369,
    0x0065636e, 0x00030005, 0x0000003c, 0x00000000, 0x00030005, 0x00000044, 0x00565576, 0x00030005,
    0x00000045, 0x00565561, 0x00030047, 0x0000000b, 0x00000002, 0x00050048, 0x0000000b, 0x00000000,
    0x00000023, 0x00000000, 0x00050048, 0x0000000b, 0x00000001, 0x00000023, 0x00000010, 0x00050048,
    0x0000000b, 0x00000002, 0x00000023, 0x00000020, 0x00040047, 0x00000023, 0x0000001e, 0x00000000,
    0x00030047, 0x0000003a, 0x00000002, 0x00050048, 0x0000003a, 0x00000000, 0x0000000b, 0x00000000,
    0x00050048, 0x0000003a, 0x00000001, 0x0000000b, 0x00000001, 0x00050048, 0x0000003a, 0x00000002,
    0x0000000b, 0x00000003, 0x00050048, 0x0000003a, 0x00000003, 0x0000000b, 0x00000004, 0x00040047,
    0x00000044, 0x0000001e, 0x00000000, 0x00040047, 0x00000045, 0x0000001e, 0x00000001, 0x00020013,
    0x00000002, 0x00030021, 0x00000003, 0x00000002, 0x00030016, 0x00000006, 0x00000020, 0x00040017,
    0x00000007, 0x00000006, 0x00000002, 0x00040020, 0x00000008, 0x00000007, 0x00000007, 0x00040017,
    0x0000000a, 0x00000006, 0x00000004, 0x0005001e, 0x0000000b, 0x0000000a, 0x0000000a, 0x0000000a,
    0x00040020, 0x0000000c, 0x00000009, 0x0000000b, 0x0004003b, 0x0000000c, 0x0000000d, 0x00000009,
    0x00040015, 0x0000000e, 0x00000020, 0x00000001, 0x0004002b, 0x0000000e, 0x0000000f, 0x00000000,
    0x00040020, 0x00000010, 0x00000009, 0x0000000a, 0x0004002b, 0x0000000e, 0x00000014, 0x00000001,
    0x00040018, 0x00000018, 0x00000007, 0x00000002, 0x0004002b, 0x00000006, 0x00000019, 0x3f800000,
    0x0004002b, 0x00000006, 0x0000001a, 0x00000000, 0x00040020, 0x00000022, 0x00000001, 0x00000007,
    0x0004003b, 0x00000022, 0x00000023, 0x00000001, 0x00040015, 0x00000026, 0x00000020, 0x00000000,
    0x0004002b, 0x00000026, 0x00000027, 0x00000002, 0x00040020, 0x00000028, 0x00000009, 0x00000006,
    0x0004002b, 0x00000006, 0x00000031, 0x40000000, 0x0004002b, 0x0000000e, 0x00000033, 0x00000002,
    0x0004002b, 0x00000026, 0x00000038, 0x00000001, 0x0004001c, 0x00000039, 0x00000006, 0x00000038,
    0x0006001e, 0x0000003a, 0x0000000a, 0x00000006, 0x00000039, 0x00000039, 0x00040020, 0x0000003b,
    0x00000003, 0x0000003a, 0x0004003b, 0x0000003b, 0x0000003c, 0x00000003, 0x00040020, 0x00000041,
    0x00000003, 0x0000000a, 0x00040020, 0x00000043, 0x00000003, 0x00000007, 0x0004003b, 0x00000043,
    0x00000044, 0x00000003, 0x0004003b, 0x00000022, 0x00000045, 0x00000001, 0x00050036, 0x00000002,
    0x00000004, 0x00000000, 0x00000003, 0x000200f8, 0x00000005, 0x0004003b, 0x00000008, 0x00000009,
    0x00000007, 0x0004003b, 0x00000008, 0x0000002f, 0x00000007, 0x00050041, 0x00000010, 0x00000011,
    0x0000000d, 0x0000000f, 0x0004003d, 0x0000000a, 0x00000012, 0x00000011, 0x0007004f, 0x00000007,
    0x00000013, 0x00000012, 0x00000012, 0x00000000, 0x00000001, 0x00050041, 0x00000010, 0x00000015,
    0x0000000d, 0x00000014, 0x0004003d, 0x0000000a, 0x00000016, 0x00000015, 0x0007004f, 0x00000007,
    0x00000017, 0x00000016, 0x00000016, 0x00000000, 0x00000001, 0x00050051, 0x00000006, 0x0000001b,
    0x00000013, 0x00000000, 0x00050051, 0x00000006, 0x0000001c, 0x00000013, 0x00000001, 0x00050051,
    0x00000006, 0x0000001d, 0x00000017, 0x00000000, 0x00050051, 0x00000006, 0x0000001e, 0x00000017,
    0x00000001, 0x00050050, 0x00000007, 0x0000001f, 0x0000001b, 0x0000001c, 0x00050050, 0x00000007,
    0x00000020, 0x0000001d, 0x0000001e, 0x00050050, 0x00000018, 0x00000021, 0x0000001f, 0x00000020,
    0x0004003d, 0x00000007, 0x00000024, 0x00000023, 0x00050091, 0x00000007, 0x00000025, 0x00000021,
    0x00000024, 0x00060041, 0x00000028, 0x00000029, 0x0000000d, 0x0000000f, 0x00000027, 0x0004003d,
    0x00000006, 0x0000002a, 0x00000029, 0x00060041, 0x00000028, 0x0000002b, 0x0000000d, 0x00000014,
    0x00000027, 0x0004003d, 0x00000006, 0x0000002c, 0x0000002b, 0x00050050, 0x00000007, 0x0000002d,
    0x0000002a, 0x0000002c, 0x00050081, 0x00000007, 0x0000002e, 0x00000025, 0x0000002d, 0x0003003e,
    0x00000009, 0x0000002e, 0x0004003d, 0x00000007, 0x00000030, 0x00000009, 0x0005008e, 0x00000007,
    0x00000032, 0x00000030, 0x00000031, 0x00050041, 0x00000010, 0x00000034, 0x0000000d, 0x00000033,
    0x0004003d, 0x0000000a, 0x00000035, 0x00000034, 0x0007004f, 0x00000007, 0x00000036, 0x00000035,
    0x00000035, 0x00000000, 0x00000001, 0x00050088, 0x00000007, 0x00000037, 0x00000032, 0x00000036,
    0x0003003e, 0x0000002f, 0x00000037, 0x0004003d, 0x00000007, 0x0000003d, 0x0000002f, 0x00050051,
    0x00000006, 0x0000003e, 0x0000003d, 0x00000000, 0x00050051, 0x00000006, 0x0000003f, 0x0000003d,
    0x00000001, 0x00070050, 0x0000000a, 0x00000040, 0x0000003e, 0x0000003f, 0x0000001a, 0x00000019,
    0x00050041, 0x00000041, 0x00000042, 0x0000003c, 0x0000000f, 0x0003003e, 0x00000042, 0x00000040,
    0x0004003d, 0x00000007, 0x00000046, 0x00000045, 0x0003003e, 0x00000044, 0x00000046, 0x000100fd,
    0x00010038,
];

/// The M3c+M3d layer-texture fragment shader: sample the combined image
/// sampler, apply the M3d color effects (brightness × tint rgb on the
/// sampled RGB, alpha scaled by the effective layer alpha from m1.w), and
/// output straight color; compiled with
/// glslangValidator -V --target-env vulkan1.2 from shaders/texture.frag.
#[rustfmt::skip]
const TEXTURE_SPIRV: &[u32] = &[
    0x07230203, 0x00010500, 0x0008000b, 0x00000047, 0x00000000, 0x00020011, 0x00000001, 0x0006000b,
    0x00000001, 0x4c534c47, 0x6474732e, 0x3035342e, 0x00000000, 0x0003000e, 0x00000000, 0x00000001,
    0x0009000f, 0x00000004, 0x00000004, 0x6e69616d, 0x00000000, 0x0000000d, 0x00000011, 0x00000016,
    0x00000039, 0x00030010, 0x00000004, 0x00000007, 0x00030003, 0x00000002, 0x000001c2, 0x00040005,
    0x00000004, 0x6e69616d, 0x00000000, 0x00030005, 0x00000009, 0x00000063, 0x00030005, 0x0000000d,
    0x00786574, 0x00030005, 0x00000011, 0x00565576, 0x00030005, 0x00000014, 0x00004350, 0x00040006,
    0x00000014, 0x00000000, 0x0000306d, 0x00040006, 0x00000014, 0x00000001, 0x0000316d, 0x00060006,
    0x00000014, 0x00000002, 0x77656976, 0x74726f70, 0x00000000, 0x00050006, 0x00000014, 0x00000003,
    0x65666665, 0x00737463, 0x00030005, 0x00000016, 0x00006370, 0x00050005, 0x00000039, 0x4374756f,
    0x726f6c6f, 0x00000000, 0x00040047, 0x0000000d, 0x00000021, 0x00000000, 0x00040047, 0x0000000d,
    0x00000022, 0x00000000, 0x00040047, 0x00000011, 0x0000001e, 0x00000000, 0x00030047, 0x00000014,
    0x00000002, 0x00050048, 0x00000014, 0x00000000, 0x00000023, 0x00000000, 0x00050048, 0x00000014,
    0x00000001, 0x00000023, 0x00000010, 0x00050048, 0x00000014, 0x00000002, 0x00000023, 0x00000020,
    0x00050048, 0x00000014, 0x00000003, 0x00000023, 0x00000030, 0x00040047, 0x00000039, 0x0000001e,
    0x00000000, 0x00020013, 0x00000002, 0x00030021, 0x00000003, 0x00000002, 0x00030016, 0x00000006,
    0x00000020, 0x00040017, 0x00000007, 0x00000006, 0x00000004, 0x00040020, 0x00000008, 0x00000007,
    0x00000007, 0x00090019, 0x0000000a, 0x00000006, 0x00000001, 0x00000000, 0x00000000, 0x00000000,
    0x00000001, 0x00000000, 0x0003001b, 0x0000000b, 0x0000000a, 0x00040020, 0x0000000c, 0x00000000,
    0x0000000b, 0x0004003b, 0x0000000c, 0x0000000d, 0x00000000, 0x00040017, 0x0000000f, 0x00000006,
    0x00000002, 0x00040020, 0x00000010, 0x00000001, 0x0000000f, 0x0004003b, 0x00000010, 0x00000011,
    0x00000001, 0x0006001e, 0x00000014, 0x00000007, 0x00000007, 0x00000007, 0x00000007, 0x00040020,
    0x00000015, 0x00000009, 0x00000014, 0x0004003b, 0x00000015, 0x00000016, 0x00000009, 0x00040015,
    0x00000017, 0x00000020, 0x00000001, 0x0004002b, 0x00000017, 0x00000018, 0x00000003, 0x00040015,
    0x00000019, 0x00000020, 0x00000000, 0x0004002b, 0x00000019, 0x0000001a, 0x00000000, 0x00040020,
    0x0000001b, 0x00000009, 0x00000006, 0x00040017, 0x0000001e, 0x00000006, 0x00000003, 0x00040020,
    0x00000022, 0x00000007, 0x00000006, 0x0004002b, 0x00000019, 0x00000025, 0x00000001, 0x0004002b,
    0x00000019, 0x00000028, 0x00000002, 0x00040020, 0x0000002b, 0x00000009, 0x00000007, 0x00040020,
    0x00000038, 0x00000003, 0x00000007, 0x0004003b, 0x00000038, 0x00000039, 0x00000003, 0x0004002b,
    0x00000019, 0x0000003c, 0x00000003, 0x0004002b, 0x00000017, 0x0000003f, 0x00000001, 0x00050036,
    0x00000002, 0x00000004, 0x00000000, 0x00000003, 0x000200f8, 0x00000005, 0x0004003b, 0x00000008,
    0x00000009, 0x00000007, 0x0004003d, 0x0000000b, 0x0000000e, 0x0000000d, 0x0004003d, 0x0000000f,
    0x00000012, 0x00000011, 0x00050057, 0x00000007, 0x00000013, 0x0000000e, 0x00000012, 0x0003003e,
    0x00000009, 0x00000013, 0x00060041, 0x0000001b, 0x0000001c, 0x00000016, 0x00000018, 0x0000001a,
    0x0004003d, 0x00000006, 0x0000001d, 0x0000001c, 0x0004003d, 0x00000007, 0x0000001f, 0x00000009,
    0x0008004f, 0x0000001e, 0x00000020, 0x0000001f, 0x0000001f, 0x00000000, 0x00000001, 0x00000002,
    0x0005008e, 0x0000001e, 0x00000021, 0x00000020, 0x0000001d, 0x00050041, 0x00000022, 0x00000023,
    0x00000009, 0x0000001a, 0x00050051, 0x00000006, 0x00000024, 0x00000021, 0x00000000, 0x0003003e,
    0x00000023, 0x00000024, 0x00050041, 0x00000022, 0x00000026, 0x00000009, 0x00000025, 0x00050051,
    0x00000006, 0x00000027, 0x00000021, 0x00000001, 0x0003003e, 0x00000026, 0x00000027, 0x00050041,
    0x00000022, 0x00000029, 0x00000009, 0x00000028, 0x00050051, 0x00000006, 0x0000002a, 0x00000021,
    0x00000002, 0x0003003e, 0x00000029, 0x0000002a, 0x00050041, 0x0000002b, 0x0000002c, 0x00000016,
    0x00000018, 0x0004003d, 0x00000007, 0x0000002d, 0x0000002c, 0x0008004f, 0x0000001e, 0x0000002e,
    0x0000002d, 0x0000002d, 0x00000001, 0x00000002, 0x00000003, 0x0004003d, 0x00000007, 0x0000002f,
    0x00000009, 0x0008004f, 0x0000001e, 0x00000030, 0x0000002f, 0x0000002f, 0x00000000, 0x00000001,
    0x00000002, 0x00050085, 0x0000001e, 0x00000031, 0x00000030, 0x0000002e, 0x00050041, 0x00000022,
    0x00000032, 0x00000009, 0x0000001a, 0x00050051, 0x00000006, 0x00000033, 0x00000031, 0x00000000,
    0x0003003e, 0x00000032, 0x00000033, 0x00050041, 0x00000022, 0x00000034, 0x00000009, 0x00000025,
    0x00050051, 0x00000006, 0x00000035, 0x00000031, 0x00000001, 0x0003003e, 0x00000034, 0x00000035,
    0x00050041, 0x00000022, 0x00000036, 0x00000009, 0x00000028, 0x00050051, 0x00000006, 0x00000037,
    0x00000031, 0x00000002, 0x0003003e, 0x00000036, 0x00000037, 0x0004003d, 0x00000007, 0x0000003a,
    0x00000009, 0x0008004f, 0x0000001e, 0x0000003b, 0x0000003a, 0x0000003a, 0x00000000, 0x00000001,
    0x00000002, 0x00050041, 0x00000022, 0x0000003d, 0x00000009, 0x0000003c, 0x0004003d, 0x00000006,
    0x0000003e, 0x0000003d, 0x00060041, 0x0000001b, 0x00000040, 0x00000016, 0x0000003f, 0x0000003c,
    0x0004003d, 0x00000006, 0x00000041, 0x00000040, 0x00050085, 0x00000006, 0x00000042, 0x0000003e,
    0x00000041, 0x00050051, 0x00000006, 0x00000043, 0x0000003b, 0x00000000, 0x00050051, 0x00000006,
    0x00000044, 0x0000003b, 0x00000001, 0x00050051, 0x00000006, 0x00000045, 0x0000003b, 0x00000002,
    0x00070050, 0x00000007, 0x00000046, 0x00000043, 0x00000044, 0x00000045, 0x00000042, 0x0003003e,
    0x00000039, 0x00000046, 0x000100fd, 0x00010038,
];

/// The unit quad as two fan-ordered triangles: 6 vertices of
/// (pos: vec2 in [-0.5, 0.5]², uv: vec2), matching the pipeline's vertex
/// input (binding 0, stride 16). The order is [v0,v1,v2, v0,v2,v3], i.e.
/// (pos -0.5,-0.5)(0.5,-0.5)(0.5,0.5) then (0.5,0.5)(-0.5,0.5)(-0.5,-0.5):
/// with TRIANGLE_LIST the two primitives are (v0,v1,v2) and (v0,v2,v3),
/// which together tile the quad's full area (the 4-vertex order
/// (0,1,2)+(1,2,3) covers only the right half). The uv origin is the
/// texture's top-left corner (row 0 = the top of the picture), which in
/// scene space (+y down) is the smaller y — so v=0 sits at pos.y = -0.5
/// and the image renders upright, not mirrored.
const UNIT_QUAD: [f32; 24] = [
    -0.5, -0.5, 0.0, 0.0, //
    0.5, -0.5, 1.0, 0.0, //
    0.5, 0.5, 1.0, 1.0, //
    0.5, 0.5, 1.0, 1.0, //
    -0.5, 0.5, 0.0, 1.0, //
    -0.5, -0.5, 0.0, 0.0,
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
        let mut renderer = LayerRenderer::new(Some(&binding), 64, 48).expect("create renderer");
        // A 1x1 opaque red texture drawn fullscreen (scale 64x48, no offset):
        // src-over over the clear gives the texture color exactly, since an
        // opaque source fully replaces the destination.
        renderer
            .upload_layer(0, &[255, 0, 0, 255], 1, 1)
            .expect("upload layer");
        let draws = [LayerDraw {
            layer_index: 0,
            m: [[64.0, 0.0], [0.0, 48.0]],
            t: [0.0, 0.0],
            alpha: 1.0,
            blend_mode: BlendMode::Normal,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
        }];
        let pixels = renderer
            .render([0.1, 0.2, 0.3, 1.0], &draws)
            .expect("render once");
        assert_eq!(pixels.len(), 64 * 48 * 4);
        // B8G8R8A8 readback is already B,G,R,A in memory order; premultiplied
        // by the opaque alpha, so every pixel is exactly the red texture.
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(pixel, &[0, 0, 255, 255]);
        }
        // A clear-only pass (no draws) shows the clear color: B=0.3*255,
        // G=0.2*255, R=0.1*255, A=255 (rounding varies by driver).
        let cleared = renderer
            .render([0.1, 0.2, 0.3, 1.0], &[])
            .expect("render clear");
        for pixel in cleared.chunks_exact(4) {
            assert_eq!(pixel[3], 255);
            assert!((76..=77).contains(&pixel[0]), "B={}", pixel[0]);
            assert_eq!(pixel[1], 51);
            assert!((25..=26).contains(&pixel[2]), "R={}", pixel[2]);
        }
        // The drop must not fault: the renderer's entry keeps the loader
        // mapped through the destroy calls, and the uploaded texture's
        // image/view/memory and descriptor pool are destroyed here.
    }

    /// Texture orientation through the real pipeline: a 2x2 texture with a
    /// distinct color per corner, stretched over the full frame. The texture
    /// top row (red, green) must land at the frame's top row (scene +y down)
    /// and not come back mirrored — the vertex shader renders the scene
    /// bottom-first on the attachment because OPTIMAL-tiling color images are
    /// stored bottom-first in the readback, and this test pins the net result
    /// (it is the M3c orientation contract with the protocol).
    #[test]
    fn quad_orientation() {
        let Ok(binding) = std::env::var("KWE_TEST_DEVICE") else {
            eprintln!("quad_orientation: skipped (set KWE_TEST_DEVICE to run)");
            return;
        };
        let mut renderer = LayerRenderer::new(Some(&binding), 64, 48).expect("create renderer");
        // R8G8B8A8, rows top-to-bottom: red, green / blue, white.
        let texture = [
            255, 0, 0, 255, 0, 255, 0, 255, //
            0, 0, 255, 255, 255, 255, 255, 255,
        ];
        renderer
            .upload_layer(0, &texture, 2, 2)
            .expect("upload layer");
        let draws = [LayerDraw {
            layer_index: 0,
            m: [[64.0, 0.0], [0.0, 48.0]],
            t: [0.0, 0.0],
            alpha: 1.0,
            blend_mode: BlendMode::Normal,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
        }];
        let pixels = renderer
            .render([0.0, 0.0, 0.0, 1.0], &draws)
            .expect("render once");
        let at = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * 64 + x) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };
        assert_eq!(at(8, 8), [0, 0, 255, 255], "top-left = texture top-left");
        assert_eq!(at(56, 8), [0, 255, 0, 255], "top-right = texture top-right");
        assert_eq!(
            at(8, 40),
            [255, 0, 0, 255],
            "bottom-left = texture bottom-left"
        );
        assert_eq!(
            at(56, 40),
            [255, 255, 255, 255],
            "bottom-right = texture bottom-right"
        );
    }

    /// The src-over blend math end to end, byte-exact: an opaque
    /// (64,103,142,255) texel drawn at layer alpha 191/255 over a fully
    /// transparent black clear. The blend scales the straight source by the
    /// fragment alpha — stored (48,77,106,191) — and the readback then
    /// premultiplies by the stored alpha: R = 48*191/255 = 36,
    /// G = 77*191/255 = 58, B = 106*191/255 = 79. The alpha channel blend
    /// factor must be ONE (not SRC_ALPHA): scaling the source alpha by
    /// itself would store 143 here and 143/255 opacity over an opaque
    /// destination.
    #[test]
    fn blend_partial_alpha() {
        let Ok(binding) = std::env::var("KWE_TEST_DEVICE") else {
            eprintln!("blend_partial_alpha: skipped (set KWE_TEST_DEVICE to run)");
            return;
        };
        let mut renderer = LayerRenderer::new(Some(&binding), 64, 48).expect("create renderer");
        renderer
            .upload_layer(0, &[64, 103, 142, 255], 1, 1)
            .expect("upload layer");
        let draws = [LayerDraw {
            layer_index: 0,
            m: [[64.0, 0.0], [0.0, 48.0]],
            t: [0.0, 0.0],
            alpha: 191.0 / 255.0,
            blend_mode: BlendMode::Normal,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
        }];
        let pixels = renderer
            .render([0.0, 0.0, 0.0, 0.0], &draws)
            .expect("render once");
        // The shader outputs straight (64,103,142) with alpha 191/255; the
        // blend stores it straight (color factor ONE) and the readback
        // premultiplies exactly once: R=48, G=77, B=106 — BGRA memory order
        // (106,77,48,191). The old oracle (79,58,36,191) was the
        // double-premultiplied value (blend factor SRC_ALPHA + readback).
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(pixel, &[106, 77, 48, 191], "premultiplied BGRA");
        }
    }

    /// The M3d blend-state table, pure: each implemented mode maps to the
    /// fixed-function factors/ops the researched WE semantics require
    /// (docs/SCENE_FORMAT_V1.md, M3d section).
    #[test]
    fn blend_attachment_table_matches_the_researched_we_semantics() {
        let normal = blend_attachment_for(BlendMode::Normal);
        assert_eq!(normal.src_color_blend_factor, vk::BlendFactor::ONE);
        assert_eq!(
            normal.dst_color_blend_factor,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA
        );
        assert_eq!(normal.color_blend_op, vk::BlendOp::ADD);
        assert_eq!(
            normal.dst_alpha_blend_factor,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA
        );

        // Multiply: texel × background (color); the alpha composites
        // src-over — the layer's own opacity must survive (the review-fixed
        // (ZERO, ONE) discarded it: a translucent multiply over a
        // transparent backdrop vanished, over an opaque one the composite
        // ignored the layer's opacity).
        let multiply = blend_attachment_for(BlendMode::Multiply);
        assert_eq!(multiply.src_color_blend_factor, vk::BlendFactor::DST_COLOR);
        assert_eq!(multiply.dst_color_blend_factor, vk::BlendFactor::ZERO);
        assert_eq!(multiply.color_blend_op, vk::BlendOp::ADD);
        assert_eq!(multiply.src_alpha_blend_factor, vk::BlendFactor::ONE);
        assert_eq!(
            multiply.dst_alpha_blend_factor,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA
        );
        assert_eq!(multiply.alpha_blend_op, vk::BlendOp::ADD);

        // Add: texel + background, additive on both channels.
        let add = blend_attachment_for(BlendMode::Add);
        assert_eq!(add.src_color_blend_factor, vk::BlendFactor::ONE);
        assert_eq!(add.dst_color_blend_factor, vk::BlendFactor::ONE);
        assert_eq!(add.color_blend_op, vk::BlendOp::ADD);
        assert_eq!(add.src_alpha_blend_factor, vk::BlendFactor::ONE);
        assert_eq!(add.dst_alpha_blend_factor, vk::BlendFactor::ONE);
        assert_eq!(add.alpha_blend_op, vk::BlendOp::ADD);

        // Screen: 255-(255-texel)(255-background)/255 =
        // texel·(1−background) + background — the src factor is
        // ONE_MINUS_DST_COLOR, the dst factor ONE (the device oracle
        // pinned the direction); the alpha composites src-over.
        let screen = blend_attachment_for(BlendMode::Screen);
        assert_eq!(
            screen.src_color_blend_factor,
            vk::BlendFactor::ONE_MINUS_DST_COLOR
        );
        assert_eq!(screen.dst_color_blend_factor, vk::BlendFactor::ONE);
        assert_eq!(screen.color_blend_op, vk::BlendOp::ADD);
        assert_eq!(screen.src_alpha_blend_factor, vk::BlendFactor::ONE);
        assert_eq!(
            screen.dst_alpha_blend_factor,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA
        );

        // Subtract: max(0, background − texel) — REVERSE_SUBTRACT computes
        // dst − src with the texel as src; the alpha composites src-over.
        let subtract = blend_attachment_for(BlendMode::Subtract);
        assert_eq!(subtract.src_color_blend_factor, vk::BlendFactor::ONE);
        assert_eq!(subtract.dst_color_blend_factor, vk::BlendFactor::ONE);
        assert_eq!(subtract.color_blend_op, vk::BlendOp::REVERSE_SUBTRACT);
        assert_eq!(subtract.src_alpha_blend_factor, vk::BlendFactor::ONE);
        assert_eq!(
            subtract.dst_alpha_blend_factor,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA
        );
        assert_eq!(subtract.alpha_blend_op, vk::BlendOp::ADD);

        for mode in BlendMode::ALL {
            let state = blend_attachment_for(mode);
            assert_eq!(state.blend_enable, vk::TRUE);
            assert_eq!(
                state.color_write_mask,
                vk::ColorComponentFlags::R
                    | vk::ColorComponentFlags::G
                    | vk::ColorComponentFlags::B
                    | vk::ColorComponentFlags::A
            );
        }
    }

    /// The M3d fragment-shader math in pure Rust — the oracle the
    /// byte-exact smoke cases and the unit tests compute against. The
    /// effects (brightness × tint) apply to the sampled texel BEFORE
    /// blending: RGB scale by brightness and the tint rgb, alpha by the
    /// pushed effective alpha (layer alpha × tint alpha, folded
    /// host-side).
    fn apply_color_effects(
        texel: [f32; 4],
        brightness: f32,
        tint: [f32; 4],
        layer_alpha: f32,
    ) -> [f32; 4] {
        [
            texel[0] * brightness * tint[0],
            texel[1] * brightness * tint[1],
            texel[2] * brightness * tint[2],
            texel[3] * layer_alpha * tint[3],
        ]
    }

    #[test]
    fn color_effects_math_matches_the_shader() {
        // The identity effects leave the texel and the layer alpha alone.
        let texel = [64.0 / 255.0, 103.0 / 255.0, 142.0 / 255.0, 1.0];
        assert_eq!(
            apply_color_effects(texel, 1.0, [1.0, 1.0, 1.0, 1.0], 1.0),
            texel
        );
        // Brightness scales RGB only; the alpha stays the sampled alpha.
        let bright = apply_color_effects(texel, 2.0, [1.0, 1.0, 1.0, 1.0], 1.0);
        assert_eq!(bright[0], texel[0] * 2.0);
        assert_eq!(bright[3], texel[3]);
        // Tint scales RGB and alpha; the tint alpha folds with the layer
        // alpha (multiplication is commutative — the host-side fold is
        // exact).
        let tinted = apply_color_effects(texel, 1.0, [0.5, 1.0, 0.25, 0.5], 0.5);
        assert_eq!(tinted, [texel[0] * 0.5, texel[1], texel[2] * 0.25, 0.25]);
        // Everything at once, matching the push-constant construction.
        let all = apply_color_effects(texel, 2.0, [0.5, 0.75, 1.0, 0.5], 0.5);
        assert_eq!(
            all,
            [texel[0], texel[1] * 1.5, texel[2] * 2.0, texel[3] * 0.25]
        );
    }

    /// Every implemented blend mode through the real pipeline, byte-exact
    /// over an opaque clear: an opaque (64,103,142) texel fullscreen over
    /// an opaque (102,64,26) background. The hand-computed per-mode
    /// composites (premultiplied readback is the identity at alpha 255):
    ///   normal:   the texel            -> (64, 103, 142)
    ///   multiply: texel·bg/255         -> (26, 26, 14)
    ///   add:      min(255, texel+bg)   -> (166, 167, 168)
    ///   screen:   255-(255-t)(255-b)/255 -> (140, 141, 154)
    ///   subtract: max(0, bg−texel)     -> (38, 0, 0)
    /// (R, G, B) — BGRA readback reverses each quadruple.
    #[test]
    fn blend_modes_composite_byte_exact() {
        let Ok(binding) = std::env::var("KWE_TEST_DEVICE") else {
            eprintln!("blend_modes_composite_byte_exact: skipped (set KWE_TEST_DEVICE to run)");
            return;
        };
        let mut renderer = LayerRenderer::new(Some(&binding), 64, 48).expect("create renderer");
        renderer
            .upload_layer(0, &[64, 103, 142, 255], 1, 1)
            .expect("upload layer");
        let cases: [(BlendMode, [u8; 4]); 5] = [
            (BlendMode::Normal, [142, 103, 64, 255]),
            (BlendMode::Multiply, [14, 26, 26, 255]),
            (BlendMode::Add, [168, 167, 166, 255]),
            (BlendMode::Screen, [154, 141, 140, 255]),
            (BlendMode::Subtract, [0, 0, 38, 255]),
        ];
        for (mode, expected) in cases {
            let draws = [LayerDraw {
                layer_index: 0,
                m: [[64.0, 0.0], [0.0, 48.0]],
                t: [0.0, 0.0],
                alpha: 1.0,
                blend_mode: mode,
                brightness: 1.0,
                tint: [1.0, 1.0, 1.0, 1.0],
            }];
            let pixels = renderer
                .render([0.4, 0.25, 0.1, 1.0], &draws)
                .expect("render once");
            for pixel in pixels.chunks_exact(4) {
                assert_eq!(pixel, &expected, "mode {mode:?}: BGRA");
            }
        }
    }

    /// The brightness and tint effects through the real pipeline, byte
    /// exact: the effects apply to the sampled texel before blending, so
    /// over an opaque background with an opaque texel the composite is the
    /// effect-scaled texel. brightness 2.0 and tint (1, 0.25, 0.5) on a
    /// (64,103,142) texel: R = 128, G = 103·0.5 = 51.5 → 52 by the UNORM
    /// round-to-nearest (the driver's tie behavior is pinned as 51 or 52),
    /// B = 142 (exact).
    #[test]
    fn color_effects_composite_byte_exact() {
        let Ok(binding) = std::env::var("KWE_TEST_DEVICE") else {
            eprintln!("color_effects_composite_byte_exact: skipped (set KWE_TEST_DEVICE to run)");
            return;
        };
        let mut renderer = LayerRenderer::new(Some(&binding), 64, 48).expect("create renderer");
        renderer
            .upload_layer(0, &[64, 103, 142, 255], 1, 1)
            .expect("upload layer");
        let draws = [LayerDraw {
            layer_index: 0,
            m: [[64.0, 0.0], [0.0, 48.0]],
            t: [0.0, 0.0],
            alpha: 1.0,
            blend_mode: BlendMode::Normal,
            brightness: 2.0,
            tint: [1.0, 0.25, 0.5, 1.0],
        }];
        let pixels = renderer
            .render([0.4, 0.25, 0.1, 1.0], &draws)
            .expect("render once");
        let g = pixels[1];
        assert!(g == 51 || g == 52, "G={g} (51.5 → round-to-nearest)");
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(pixel[0], 142, "B = 142·2·0.5");
            assert_eq!(pixel[1], g, "G = 103·2·0.25 = 51.5");
            assert_eq!(pixel[2], 128, "R = 64·2·1");
            assert_eq!(pixel[3], 255, "A stays opaque");
        }
    }

    /// The translucent-multiply alpha policy, byte exact: multiply at layer
    /// alpha 0.5 over a 0.5-alpha clear. The mode acts on the color, so the
    /// attachment stores the hard multiply — B = 142·26/255 → 14, G =
    /// 103·64/255 → 26, R = 64·102/255 → 26 — and the ALPHA channel
    /// composites src-over: 0.5 + (128/255)·0.5 = 0.75098 → 191.5 → 192 by
    /// round-to-nearest-even (the dst alpha is the QUANTIZED 128, which is
    /// what pushes the tie past 191). The readback premultiplies exactly
    /// once: floor((v·192+127)/255) → (11, 20, 20, 192). This pins that the
    /// layer's own opacity survives — the review-fixed (ZERO, ONE) alpha
    /// factors would have delivered the backdrop's (7,13,13,128) instead,
    /// discarding the layer's 0.5 entirely.
    #[test]
    fn translucent_multiply_alpha_composites_src_over_byte_exact() {
        let Ok(binding) = std::env::var("KWE_TEST_DEVICE") else {
            eprintln!(
                "translucent_multiply_alpha_composites_src_over_byte_exact: skipped (set KWE_TEST_DEVICE to run)"
            );
            return;
        };
        let mut renderer = LayerRenderer::new(Some(&binding), 64, 48).expect("create renderer");
        renderer
            .upload_layer(0, &[64, 103, 142, 255], 1, 1)
            .expect("upload layer");
        let draws = [LayerDraw {
            layer_index: 0,
            m: [[64.0, 0.0], [0.0, 48.0]],
            t: [0.0, 0.0],
            alpha: 0.5,
            blend_mode: BlendMode::Multiply,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
        }];
        let pixels = renderer
            .render([0.4, 0.25, 0.1, 0.5], &draws)
            .expect("render once");
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(
                pixel,
                &[11, 20, 20, 192],
                "multiply α=0.5 over α=0.5 clear: BGRA"
            );
        }
    }
}
