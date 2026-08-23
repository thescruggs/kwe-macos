// SPDX-License-Identifier: GPL-3.0-or-later
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
// M3f: particle systems are CPU-simulated (particles.rs) and composited as
// one batched draw per system — a second pipeline family (particle.vert /
// particle.frag, stride 40: pos, uv, color, size) with one host-visible
// vertex buffer per system, rebuilt per frame from the simulation. Each
// system owns a descriptor slot at MAX_LAYERS + system_index (the pool and
// the upload bound are TEXTURE_SLOT_COUNT = MAX_LAYERS + MAX_PARTICLE_SYSTEMS
// sets), so a system's texture can never collide with a layer's. Particle
// draws reuse the same push constants and per-mode blend variants; the
// render dispatch picks the pipeline family and vertex source by
// DrawKind::Particles (layer_index - MAX_LAYERS into the particle buffers).
// The particle vertex count is bounded by the simulation cap (particles.rs:
// at most MAX_PARTICLES × 6 verts per system, 4096 × 6 × 40 B = 983,040 B).
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

use crate::layers::{BlendMode, DrawKind, LayerDraw, MAX_LAYERS};
use crate::materialshader::{
    MATERIAL_UNIFORMS_SIZE, MaterialKey, MaterialUniforms, build_orthographic_mvp,
};
use crate::particles::MAX_PARTICLE_SYSTEMS;
use crate::shaderpre::MAX_MATERIAL_TEXTURES;
use kwe_core::FULL_FRAME_BUFFER;
use std::collections::HashMap;

/// Fence wait bound per frame; a GPU stuck longer than this is treated as a
/// backend failure by the caller.
pub const FENCE_TIMEOUT_NS: u64 = 1_000_000_000;

/// Total descriptor-slot budget: one set per layer plus one texture set per
/// particle system (M3f), so systems and layers never share descriptor
/// sets. Bounded by both caps; the pool and the upload bound use this, and
/// particle system i lives at slot MAX_LAYERS + i (see particles.rs).
const TEXTURE_SLOT_COUNT: usize = MAX_LAYERS + MAX_PARTICLE_SYSTEMS;

/// S3: cap on live effect render targets in one scene (task bound: "≤ 64
/// targets/scene with a memory cap") — `_rt_FullFrameBuffer` plus every
/// distinct `fbos[]` entry across every resolved effect. A request past
/// this cap is simply not created; a name that never gets an entry
/// degrades every slot referencing it to the shared `dummy_texture` (this
/// module's universal "unresolved effect reference" fallback), never a
/// failure.
const MAX_EFFECT_TARGETS_PER_SCENE: usize = 64;

/// S3: cumulative byte budget across every live effect render target
/// (RGBA8, 4 bytes/pixel) — independent of the per-target dimension clamp
/// applied when a target is sized, so a scene with many large targets
/// cannot exhaust GPU memory even if each one individually looks
/// reasonable.
const MAX_EFFECT_TARGET_BYTES: u64 = 256 * 1024 * 1024;

/// S3: cap on one FBO target's side length in pixels — generous over any
/// real scene resolution, bounds a hostile `scale` value (e.g. a tiny
/// fraction) from producing a target larger than the canvas itself.
const MAX_EFFECT_TARGET_DIMENSION: u32 = 4096;

/// S3: cap on distinct compiled+bound effect passes (targeted material
/// passes) in one scene — generous over the real corpus's largest chain
/// (godrays: 5 passes) while bounding a hostile scene's pipeline/
/// descriptor-set allocation the same way `MAX_PIPELINES_PER_SCENE`
/// bounds base material pipelines.
const MAX_EFFECT_PASS_BINDINGS: usize = 256;

#[derive(Debug)]
pub enum RenderError {
    Vulkan(String),
    FenceTimeout,
}

/// A fence timeout means the last queue submit may still own the command
/// buffer and all resources referenced by it. Callers must terminate the
/// worker immediately; resetting the fence or freeing those resources would
/// race the device.
#[must_use]
pub fn is_fence_timeout(error: &RenderError) -> bool {
    matches!(error, RenderError::FenceTimeout)
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
    /// The image's extent. M3g's `refresh_layer` reuses the image, view,
    /// and descriptor set when a new frame has these exact dimensions —
    /// without them the video path could not tell an in-place update from
    /// a reallocation.
    width: u32,
    height: u32,
}

/// One `g_Texture<N>` slot's binding source, as `bind_material_layer` /
/// `compile_effect_pass` see it — `main.rs` builds this from
/// `scene::MaterialTextureSource` (raw bytes decode to `Bytes`, S3's
/// `RenderTarget` name passes straight through).
pub enum MaterialTextureBind {
    /// Already-decoded RGBA8 bytes, uploaded fresh by this call (S1/S2's
    /// original contract, unchanged).
    Bytes(Vec<u8>, u32, u32),
    /// A `_rt_`/`_alias_` name: bound to `effect_targets[name]`'s current
    /// view if that target exists, else the shared `dummy_texture` — see
    /// the module doc comment on `effect_targets`.
    RenderTarget(String),
}

/// S3: one requested effect render target, built by `main.rs` from a
/// resolved `FboSpec` (name + the owning object's own pixel size /
/// `scale`, per upstream `CImage.cpp:652-654` — see
/// `LayerRenderer::effect_targets`'s doc comment) and handed to
/// `prepare_effect_targets`.
pub struct EffectTargetRequest {
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// S2: one layer's bound material — everything `render`'s draw loop needs
/// to issue the draw, plus the resources `bind_material_layer`/`Drop` must
/// tear down. `textures` holds ONLY the slots this layer uploaded its own
/// image for (index = `g_Texture<N>`'s N); any other index sampled the
/// shared `dummy_texture` and owns nothing here.
struct MaterialBinding {
    pipeline: vk::Pipeline,
    descriptor_set: vk::DescriptorSet,
    textures: Vec<(u32, LayerTexture)>,
    ubo_buffer: vk::Buffer,
    ubo_memory: vk::DeviceMemory,
    ubo_mapped: *mut u8,
    /// The CPU-side mirror of what is currently in `ubo_mapped` — `render`
    /// mutates `mvp`/`time_alpha_brightness` here each draw and re-copies
    /// the whole struct, rather than computing byte offsets into the
    /// mapped buffer by hand (fragile if `MaterialUniforms`'s field order
    /// ever changes).
    uniforms: MaterialUniforms,
}

/// A persistent host-visible staging buffer (M3g). `upload_layer` creates
/// and destroys its staging per call, which is right for a handful of
/// startup uploads and wrong for a video layer re-uploading at 30 fps:
/// that would create, map, unmap, and destroy a multi-megabyte buffer
/// every frame. `refresh_layer` keeps one buffer per renderer, grown in
/// place to the largest frame seen, mapped once for its lifetime.
struct StagingBuffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *mut u8,
    bytes: usize,
}

/// S3: one live effect render target — a color-attachment-and-sampled
/// image sized in pixels, cleared to transparent black every time a
/// material pass renders into it (`CFBO.cpp`'s documented fix for
/// effects otherwise drawing solid rectangles — see the module doc
/// comment on `render_effect_chains`). Every entry uses the SAME format
/// as the compositor's own `self.image` (`LayerRenderer::format`, chosen
/// once at device probe time to be whatever the driver actually supports
/// for `COLOR_ATTACHMENT | TRANSFER_SRC`) rather than a fixed
/// `R8G8B8A8_UNORM`: `_rt_FullFrameBuffer` is refreshed every frame by a
/// raw `vkCmdCopyImage` from `self.image` (`snapshot_full_frame_buffer`),
/// and `vkCmdCopyImage` between images of DIFFERENT channel-order formats
/// (e.g. `B8G8R8A8_UNORM` source into an `R8G8B8A8_UNORM` destination) is
/// permitted by the Vulkan spec (same texel size = compatible) but
/// reinterprets the copied bytes with the destination format's channel
/// order — a silent red/blue channel swap. Matching `self.format`
/// everywhere sidesteps that entirely.
struct EffectFbo {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    width: u32,
    height: u32,
}

/// S3: one compiled, bound effect pass that renders into a named
/// [`EffectFbo`] every frame (`render_effect_chains`). Structurally a
/// smaller `MaterialBinding` — its own pipeline (not shared with
/// `material_pipelines`: a targeted pass's viewport is the TARGET's own
/// pixel size, which a canvas-sized pipeline cannot correctly draw into,
/// since this renderer's pipelines bake a static, non-dynamic viewport)
/// and its own descriptor set/UBO, torn down together with it. Unlike
/// `MaterialBinding`, the UBO is written exactly once, at compile time
/// (`compile_effect_pass`) — an effect pass has no per-frame dynamic
/// transform (`render()` never touches `mvp`/`time_alpha_brightness` for
/// these; `render_effect_chains` only re-runs the DRAW, sampling whatever
/// its bound texture slots currently contain), so no mapped pointer needs
/// to survive past that one write.
struct EffectPassBinding {
    pipeline: vk::Pipeline,
    descriptor_set: vk::DescriptorSet,
    textures: Vec<(u32, LayerTexture)>,
    ubo_buffer: vk::Buffer,
    ubo_memory: vk::DeviceMemory,
    /// The FBO this pass renders into — always present in
    /// `effect_targets` (`compile_effect_pass` only records a binding
    /// once its target FBO already exists).
    target: String,
}

/// S3: one step of the per-frame effect replay `render_effect_chains`
/// executes in order.
enum EffectFrameAction {
    /// Render one [`EffectPassBinding`]'s fresh content into its target
    /// FBO (index into `effect_pass_bindings`).
    Render(usize),
    /// `command: copy` (and, per this renderer's own documented
    /// completion of upstream's unexecuted `swap` — see
    /// `kwe_core::sceneeffect::EffectCommand::Swap`'s doc comment — also
    /// `command: swap`): copy `source`'s CURRENT content into `target`.
    /// A true pointer swap would require re-resolving every LATER pass's
    /// already-baked descriptor-set view bindings each frame, which this
    /// renderer's load-time-only binding design does not support; a copy
    /// produces the same visible pixels for the frame it runs in, which
    /// is what upstream's own unexecuted `swap` would have done too on
    /// the one frame it mattered.
    Copy { source: String, target: String },
}

/// One text layer's quad vertex buffer (M3e): host-visible, grown in
/// place (create-or-grow on resize), index-aligned with the layer table.
/// The byte capacity is tracked so a shorter text reuses the buffer.
struct LayerVertexBuffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    bytes: usize,
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
    /// M3f: the particle pipeline family — one per blend mode, same
    /// variant ordering and shared state as `pipelines`, but built from
    /// particle.vert/frag (stride 40: pos, uv, color, size). Selected when
    /// the draw kind is Particles.
    particle_pipelines: Vec<vk::Pipeline>,
    particle_vertex_module: vk::ShaderModule,
    particle_fragment_module: vk::ShaderModule,
    /// M3f: one host-visible vertex buffer per particle system, rebuilt per
    /// frame from the CPU simulation (bounded: ≤ MAX_PARTICLES × 6 × 40 B
    /// per system). Index-aligned with the system table: system i lives at
    /// particle_vertex_buffers[i], reached from a draw's layer_index minus
    /// MAX_LAYERS.
    particle_vertex_buffers: Vec<Option<LayerVertexBuffer>>,
    /// Unit quad: 6 × (pos vec2, uv vec2) — see UNIT_QUAD.
    vertex_buffer: vk::Buffer,
    vertex_buffer_memory: vk::DeviceMemory,
    /// Linear clamp-to-edge sampler shared by every layer texture.
    sampler: vk::Sampler,
    descriptor_set_layout: vk::DescriptorSetLayout,
    /// Bounded pool: at most MAX_LAYERS sets of one combined image sampler.
    descriptor_pool: vk::DescriptorPool,
    /// One descriptor set per layer index; None until the layer uploaded.
    ///
    /// M3e: one host-visible vertex buffer per text layer (the quad
    /// geometry rebuilt on text change), index-aligned with the layer
    /// table; None until the layer's text synced.
    text_vertex_buffers: Vec<Option<LayerVertexBuffer>>,
    descriptor_sets: Vec<Option<vk::DescriptorSet>>,
    /// Uploaded textures per layer index; None = skipped at load or failed
    /// upload.
    textures: Vec<Option<LayerTexture>>,
    /// M3g: the shared staging buffer for in-place texture refreshes (the
    /// video path). Created on the first `refresh_layer` and grown to the
    /// largest frame seen; None until then, so a scene without video never
    /// allocates it.
    video_staging: Option<StagingBuffer>,
    /// Number of currently live layer textures (image + atlas). Re-uploads
    /// replace in place, so this only grows with distinct layer indices —
    /// the drop-accounting assertion backing the device test for the M3e
    /// atlas-rebuild leak.
    live_uploads: usize,
    command_pool: vk::CommandPool,
    /// Per-frame command buffer.
    command_buffer: vk::CommandBuffer,
    /// One-shot upload command buffer (uploads complete before any render
    /// submits, so the single fence serializes them).
    upload_buffer: vk::CommandBuffer,
    fence: vk::Fence,
    width: u32,
    height: u32,
    /// F1: the NDC divisor for layer/particle quads — the visible world
    /// rectangle in scene units (defaults to the canvas size).
    world_width: f32,
    world_height: f32,
    device_name: String,
    device_kind: String,
    /// S2: shared across every material pipeline — the descriptor set
    /// layout (8 combined-image-samplers at bindings 0..7, one uniform
    /// buffer at binding 8) and the pipeline layout built from it (no
    /// push constants: the whole per-instance transform lives in the
    /// per-layer `MaterialUniforms` UBO instead — see
    /// `materialshader::build_orthographic_mvp`).
    material_descriptor_set_layout: vk::DescriptorSetLayout,
    material_pipeline_layout: vk::PipelineLayout,
    /// Bounded pool: `MAX_LAYERS` sets, each `MAX_MATERIAL_TEXTURES`
    /// combined-image-samplers + 1 uniform buffer (materials apply to
    /// layers only, not particle systems, in S2 scope).
    material_descriptor_pool: vk::DescriptorPool,
    /// One compiled pipeline per distinct `MaterialKey` (shader + resolved
    /// combos + blend variant) — `register_material_pipeline` compiles
    /// once and every layer sharing that key reuses it. Bounded by the
    /// caller (`materialshader::MAX_PIPELINES_PER_SCENE`).
    material_pipelines: HashMap<u64, vk::Pipeline>,
    /// The one vertex format the material pipeline draws — a unit quad
    /// with `a_Position` (vec3, z=0) + `a_TexCoord` (vec2), matching every
    /// `genericimage*`-family vertex shader's attribute list with default
    /// combos (S2 scope: mesh/puppet geometry stays a quad). Mirrors
    /// `UNIT_QUAD`'s uv convention exactly.
    material_vertex_buffer: vk::Buffer,
    material_vertex_buffer_memory: vk::DeviceMemory,
    /// 1x1 transparent-black image bound to every material texture slot a
    /// layer's material does not fill — Vulkan requires every descriptor
    /// a pipeline's shader statically references to be valid, and a
    /// material's descriptor set always declares all 8 sampler bindings
    /// regardless of which ones that particular shader actually samples.
    dummy_texture: LayerTexture,
    /// Per layer, `Some` once `bind_material_layer` succeeds for it —
    /// None means the layer either is not a model layer, or its material
    /// fell back to the S1 quad path.
    material_bindings: Vec<Option<MaterialBinding>>,
    /// S2's `g_Time`: not real elapsed time (`render` takes no time
    /// parameter — this slice keeps existing `render`/test call sites
    /// unchanged) but a monotonic frame counter divided by an assumed 60
    /// fps. This is exact only when the renderer's actual frame rate IS
    /// 60 — it drifts systematically and permanently under any other
    /// steady-state target/achieved rate (a user- or scene-configured fps
    /// cap, F2's fps limiter, a slower device), not just transiently
    /// "under sustained frame-time pressure". Good enough for a uniform
    /// almost no default-combo `genericimage*` shader reads; documented
    /// as a known simplification (see `AI-Skills/BETA_PLAN.md`).
    material_frame_counter: u64,
    /// S3: a second offscreen render pass, identical to `render_pass`
    /// except its attachment ends in `SHADER_READ_ONLY_OPTIMAL` (a
    /// render target meant to be SAMPLED by a later pass, not read back
    /// to a staging buffer) and its usage adds `SAMPLED`/`TRANSFER_SRC`/
    /// `TRANSFER_DST` (a target can be a `command: copy` source or
    /// destination too). Created lazily (`vk::RenderPass::null()` until
    /// the first scene with a resolved effect chain) since most scenes
    /// have none.
    effect_render_pass: vk::RenderPass,
    /// Every live effect render target this scene needs, keyed by its
    /// declared `_rt_`/`_alias_` name — `_rt_FullFrameBuffer` (scene-wide,
    /// present whenever any layer has a resolved effect chain) plus every
    /// `fbos[]` entry from every resolved `ObjectEffect`. Global
    /// namespace (S3 documented scope limit: two different objects
    /// declaring the SAME fbo name share one instance rather than getting
    /// independent ones — not observed in the local corpus). Bounded by
    /// `MAX_EFFECT_TARGETS_PER_SCENE` and a cumulative byte budget;
    /// looked up by name at material-texture-slot bind time (a name with
    /// no entry here degrades to the shared `dummy_texture`, never a
    /// failure).
    effect_targets: HashMap<String, EffectFbo>,
    /// One entry per resolved effect pass that renders INTO a named FBO
    /// (a material pass with `target: Some(..)`). A pass with no target
    /// is never recorded here — it becomes its OWNING LAYER's own
    /// material binding instead (`compile_material_layers`; visually,
    /// upstream's "no target = draws directly onto the compositor" case
    /// is exactly what the layer's own `LayerDraw` already does).
    effect_pass_bindings: Vec<EffectPassBinding>,
    /// The scene-wide, ordered list of per-frame effect work
    /// `render_effect_chains` replays every frame, built once at load
    /// (`compile_material_layers`) in each layer's own pass order: render
    /// a targeted pass's fresh content, or execute a `command: copy`
    /// (`swap` also executes as a copy — see `EffectFrameAction::Copy`'s
    /// doc comment for why). Bounded by the same per-effect/per-object
    /// caps `kwe_core::sceneeffect` already enforces at parse time.
    effect_frame_actions: Vec<EffectFrameAction>,
    // Kept alive for the whole renderer lifetime: ash 0.38's Entry owns the
    // dlopen guard on libvulkan.so.1, and the loader's own trampoline
    // function pointers (vkDestroyDevice among them) dangle once the entry
    // drops. Never read; the drop side effect is the point. Declared last so
    // it is destroyed after the explicit Drop body.
    _entry: Entry,
}

impl LayerRenderer {
    /// F1: set the visible world rectangle (scene units) the canvas shows;
    /// quads are positioned against it instead of the canvas size.
    pub fn set_world_extent(&mut self, width: f32, height: f32) {
        self.world_width = width.max(1.0);
        self.world_height = height.max(1.0);
    }

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
        // M3f: the particle pipeline family shares the descriptor layout,
        // the render pass, and the 64-byte push constant block; its vertex
        // input is the 40-byte per-particle vertex (pos, uv, color, size)
        // written by the CPU simulation (see PARTICLE_VERTEX_BYTES and the
        // binding in `new`).
        let particle_vertex_module = shader_module(&device, PARTICLE_VERT_SPIRV)?;
        let particle_fragment_module = shader_module(&device, PARTICLE_FRAG_SPIRV)?;

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

        // Per-layer descriptor sets: one combined image sampler each, at
        // most TEXTURE_SLOT_COUNT — MAX_LAYERS layers plus one set per
        // particle system (M3f), the bounded table the draw list indexes
        // into (a particle system's texture lives at MAX_LAYERS + i).
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
            .descriptor_count(TEXTURE_SLOT_COUNT as u32);
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            // FREE_DESCRIPTOR_SET: M3e re-uploads a text layer's atlas in
            // place on rebuild, freeing the previous set — without the
            // flag, vkFreeDescriptorSets is a no-op and the bounded pool
            // (TEXTURE_SLOT_COUNT sets) would exhaust after that many
            // rebuilds.
            .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
            .max_sets(TEXTURE_SLOT_COUNT as u32)
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

        // M3f: the particle pipeline family — same variant ordering and
        // per-mode blend state, but with the particle vertex input (one
        // 40-byte vertex: pos.xy @ 0, uv.xy @ 8, color.rgba @ 16, size @
        // 32 — the layout particles.rs::build_vertex_bytes writes). The
        // draw loop selects this family for DrawKind::Particles.
        let particle_stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(particle_vertex_module)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(particle_fragment_module)
                .name(c"main"),
        ];
        let particle_binding = vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(40)
            .input_rate(vk::VertexInputRate::VERTEX);
        let particle_attributes = [
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
            vk::VertexInputAttributeDescription::default()
                .binding(0)
                .location(2)
                .format(vk::Format::R32G32B32A32_SFLOAT)
                .offset(16),
            vk::VertexInputAttributeDescription::default()
                .binding(0)
                .location(3)
                .format(vk::Format::R32_SFLOAT)
                .offset(32),
        ];
        let particle_vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(std::slice::from_ref(&particle_binding))
            .vertex_attribute_descriptions(&particle_attributes);
        let mut particle_pipelines = Vec::with_capacity(BlendMode::ALL.len());
        for mode in BlendMode::ALL {
            let blend_attachment = blend_attachment_for(mode);
            let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
                .attachments(std::slice::from_ref(&blend_attachment));
            let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
                .stages(&particle_stages)
                .vertex_input_state(&particle_vertex_input)
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
            particle_pipelines.push(pipeline);
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

        // S2: material pipeline shared state — descriptor set layout (8
        // combined-image-samplers + 1 UBO), its pipeline layout, a bounded
        // descriptor pool (MAX_LAYERS sets: materials apply to layers
        // only), the one vertex format the material path draws
        // (`MATERIAL_UNIT_QUAD`), and a 1x1 transparent-black dummy
        // texture for the sampler slots a given material does not fill
        // (Vulkan requires every statically-referenced descriptor to be
        // valid, and the descriptor set always declares all 8 bindings).
        let mut material_bindings_layout =
            [vk::DescriptorSetLayoutBinding::default(); MAX_MATERIAL_TEXTURES + 1];
        for (index, binding) in material_bindings_layout
            .iter_mut()
            .take(MAX_MATERIAL_TEXTURES)
            .enumerate()
        {
            *binding = vk::DescriptorSetLayoutBinding::default()
                .binding(index as u32)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT);
        }
        material_bindings_layout[MAX_MATERIAL_TEXTURES] = vk::DescriptorSetLayoutBinding::default()
            .binding(MAX_MATERIAL_TEXTURES as u32)
            .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT);
        let material_set_layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(&material_bindings_layout);
        let material_descriptor_set_layout =
            unsafe { device.create_descriptor_set_layout(&material_set_layout_info, None) }?;
        let material_layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(std::slice::from_ref(&material_descriptor_set_layout));
        let material_pipeline_layout =
            unsafe { device.create_pipeline_layout(&material_layout_info, None) }?;
        let material_pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count((MAX_LAYERS * MAX_MATERIAL_TEXTURES) as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(MAX_LAYERS as u32),
        ];
        let material_pool_info = vk::DescriptorPoolCreateInfo::default()
            .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
            .max_sets(MAX_LAYERS as u32)
            .pool_sizes(&material_pool_sizes);
        let material_descriptor_pool =
            unsafe { device.create_descriptor_pool(&material_pool_info, None) }?;

        let material_vertex_bytes =
            (MATERIAL_UNIT_QUAD.len() * std::mem::size_of::<f32>()) as vk::DeviceSize;
        let material_vertex_info = vk::BufferCreateInfo::default()
            .size(material_vertex_bytes)
            .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let material_vertex_buffer = unsafe { device.create_buffer(&material_vertex_info, None) }?;
        let material_vertex_requirements =
            unsafe { device.get_buffer_memory_requirements(material_vertex_buffer) };
        let material_vertex_buffer_memory =
            allocate_host_visible(&instance, &device, physical, &material_vertex_requirements)?;
        unsafe {
            device.bind_buffer_memory(material_vertex_buffer, material_vertex_buffer_memory, 0)
        }?;
        unsafe {
            std::ptr::copy_nonoverlapping(
                MATERIAL_UNIT_QUAD.as_ptr(),
                device
                    .map_memory(
                        material_vertex_buffer_memory,
                        0,
                        material_vertex_bytes,
                        vk::MemoryMapFlags::empty(),
                    )?
                    .cast::<f32>(),
                MATERIAL_UNIT_QUAD.len(),
            );
            device.unmap_memory(material_vertex_buffer_memory);
        }

        let dummy_texture = upload_image_now(
            &instance,
            &device,
            physical,
            queue,
            upload_buffer,
            fence,
            &[0, 0, 0, 0],
            1,
            1,
        )?;

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
            particle_pipelines,
            particle_vertex_module,
            particle_fragment_module,
            particle_vertex_buffers: Vec::new(),
            vertex_buffer,
            vertex_buffer_memory,
            sampler,
            descriptor_set_layout,
            descriptor_pool,
            descriptor_sets: Vec::new(),
            textures: Vec::new(),
            video_staging: None,
            live_uploads: 0,
            text_vertex_buffers: Vec::new(),
            command_pool,
            command_buffer,
            upload_buffer,
            fence,
            width,
            height,
            world_width: width as f32,
            world_height: height as f32,
            device_name,
            device_kind,
            material_descriptor_set_layout,
            material_pipeline_layout,
            material_descriptor_pool,
            material_pipelines: HashMap::new(),
            material_vertex_buffer,
            material_vertex_buffer_memory,
            dummy_texture,
            material_bindings: Vec::new(),
            material_frame_counter: 0,
            effect_render_pass: vk::RenderPass::null(),
            effect_targets: HashMap::new(),
            effect_pass_bindings: Vec::new(),
            effect_frame_actions: Vec::new(),
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
        if index >= TEXTURE_SLOT_COUNT {
            return Err(RenderError::Vulkan(format!(
                "layer index {index} beyond the {TEXTURE_SLOT_COUNT} texture-slot cap \
                 ({MAX_LAYERS} layers + {MAX_PARTICLE_SYSTEMS} particle systems)"
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
            if is_fence_timeout(&error) {
                // The queue submit may still be reading the staging buffer
                // and destination image. Do not unmap, destroy, or free any
                // of these raw handles. The caller must exit immediately
                // through reject_render; leaking the handles to process
                // teardown is safe, while freeing them here races Vulkan.
                // Vulkan handles are Copy scalars, so returning here leaves
                // the raw allocations owned by the process until exit.
                return Err(error);
            }
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
        // Replace-in-place: M3e re-uploads a text layer's atlas on every
        // rebuild, so the previous image/view/memory and its descriptor
        // set are destroyed here — a leaked set would exhaust the bounded
        // pool (MAX_LAYERS sets) after MAX_LAYERS rebuilds and silently
        // kill every later texture upload. A failed upload earlier in this
        // function returns before reaching this point, so the old texture
        // stays valid on failure (the layer keeps rendering the old
        // content).
        if let Some(old) = self.textures[index].take() {
            unsafe {
                self.device.destroy_image_view(old.view, None);
                self.device.destroy_image(old.image, None);
                self.device.free_memory(old.memory, None);
            }
            self.live_uploads -= 1;
        }
        if let Some(old_set) = self.descriptor_sets[index].take() {
            unsafe {
                self.device
                    .free_descriptor_sets(self.descriptor_pool, std::slice::from_ref(&old_set))
                    .expect("free_descriptor_sets: the pool has FREE_DESCRIPTOR_SET");
            }
        }
        self.textures[index] = Some(LayerTexture {
            image: image.expect("upload succeeded"),
            memory: image_memory.expect("upload succeeded"),
            view: view.expect("upload succeeded"),
            width,
            height,
        });
        self.descriptor_sets[index] = set;
        self.live_uploads += 1;
        Ok(())
    }

    /// S2: compile (if not already cached under `key`) one material
    /// pipeline — vertex + fragment SPIR-V, the fixed material vertex
    /// format (`MATERIAL_UNIT_QUAD`), and the given blend variant, sharing
    /// `material_pipeline_layout`/`material_descriptor_set_layout` and the
    /// render pass every other pipeline in this renderer uses. A no-op
    /// (`Ok`) if `key` is already registered — two layers with the same
    /// material and resolved combos share one pipeline.
    pub fn register_material_pipeline(
        &mut self,
        key: MaterialKey,
        vertex_spirv: &[u32],
        fragment_spirv: &[u32],
        blend_mode: BlendMode,
    ) -> Result<(), RenderError> {
        if self.material_pipelines.contains_key(&key.0) {
            return Ok(());
        }
        let vertex_module = shader_module(&self.device, vertex_spirv)?;
        let fragment_module = match shader_module(&self.device, fragment_spirv) {
            Ok(module) => module,
            Err(error) => {
                unsafe { self.device.destroy_shader_module(vertex_module, None) };
                return Err(error);
            }
        };
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vertex_module)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fragment_module)
                .name(c"main"),
        ];
        let binding = vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(20)
            .input_rate(vk::VertexInputRate::VERTEX);
        let attributes = [
            vk::VertexInputAttributeDescription::default()
                .binding(0)
                .location(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(0),
            vk::VertexInputAttributeDescription::default()
                .binding(0)
                .location(1)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(12),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(std::slice::from_ref(&binding))
            .vertex_attribute_descriptions(&attributes);
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::Viewport::default()
            .x(0.0)
            .y(0.0)
            .width(self.width as f32)
            .height(self.height as f32)
            .min_depth(0.0)
            .max_depth(1.0);
        let scissor = vk::Rect2D::default()
            .offset(vk::Offset2D { x: 0, y: 0 })
            .extent(vk::Extent2D {
                width: self.width,
                height: self.height,
            });
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
        let blend_attachment = blend_attachment_for(blend_mode);
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
            .layout(self.material_pipeline_layout)
            .render_pass(self.render_pass)
            .subpass(0)
            .depth_stencil_state(&depth_stencil);
        let result = unsafe {
            self.device
                .create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
        };
        // A shader module is only referenced during pipeline creation
        // (the Vulkan spec does not require it to outlive the pipeline),
        // so it is destroyed immediately either way — no per-pipeline
        // module bookkeeping needed.
        unsafe {
            self.device.destroy_shader_module(fragment_module, None);
            self.device.destroy_shader_module(vertex_module, None);
        }
        let pipeline = match result {
            Ok(pipelines) => pipelines[0],
            Err((_, result)) => return Err(result.into()),
        };
        self.material_pipelines.insert(key.0, pipeline);
        Ok(())
    }

    /// S2: bind one layer's material — upload up to `MAX_MATERIAL_TEXTURES`
    /// already-decoded RGBA8 textures (a `None` slot samples the shared
    /// 1x1 `dummy_texture`), allocate a UBO seeded with `uniforms`, and
    /// write both into a fresh descriptor set bound to `key`'s pipeline
    /// (which must already be registered via `register_material_pipeline`).
    /// Replaces any previous material binding at `layer_index` in place
    /// (tearing down its textures/UBO/descriptor set first) — a re-bind
    /// never leaks or exhausts the bounded descriptor pool.
    ///
    /// On any texture-upload failure this cleans up everything it had
    /// already uploaded for THIS call and returns `Err` without touching
    /// the layer's previous binding (if any) — the caller (main.rs) drops
    /// the whole material attempt on `Err`, so the layer keeps whatever it
    /// had (typically nothing yet, since binding happens once at load).
    /// Shared texture-slot resolution for `bind_material_layer` and
    /// `compile_effect_pass` (S3): upload a `Bytes` slot fresh (S1/S2's
    /// original contract), resolve a `RenderTarget` slot by name against
    /// `effect_targets` (falling back to `dummy_texture` when the name
    /// has no live entry — never a failure, matching this module's
    /// degrade-not-refuse contract for effect references), and leave a
    /// `None` slot on `dummy_texture` too. On an upload error for a
    /// `Bytes` slot, everything resolved so far for THIS call is torn
    /// down and the error propagates (mirrors `bind_material_layer`'s
    /// pre-S3 cleanup contract exactly).
    // Named alias would only be used at this one call site; `#[allow]` is
    // simpler than a one-call-site `type` item purely to satisfy the lint
    // (the same tradeoff `FoldedDeclarations` elsewhere in this crate
    // resolves the other way because it has multiple call sites).
    #[allow(clippy::type_complexity)]
    fn resolve_texture_slots(
        &mut self,
        textures: &[Option<MaterialTextureBind>],
    ) -> Result<
        (
            [vk::DescriptorImageInfo; MAX_MATERIAL_TEXTURES],
            Vec<(u32, LayerTexture)>,
        ),
        RenderError,
    > {
        let mut owned_textures: Vec<(u32, LayerTexture)> = Vec::new();
        let mut image_infos = [vk::DescriptorImageInfo::default(); MAX_MATERIAL_TEXTURES];
        for (slot, image_info) in image_infos.iter_mut().enumerate() {
            let view = match textures.get(slot).and_then(|entry| entry.as_ref()) {
                Some(MaterialTextureBind::Bytes(rgba, width, height)) => {
                    match upload_image_now(
                        &self.instance,
                        &self.device,
                        self.physical,
                        self.queue,
                        self.upload_buffer,
                        self.fence,
                        rgba,
                        *width,
                        *height,
                    ) {
                        Ok(texture) => {
                            let view = texture.view;
                            owned_textures.push((slot as u32, texture));
                            view
                        }
                        Err(error) => {
                            if is_fence_timeout(&error) {
                                return Err(error);
                            }
                            for (_, texture) in owned_textures {
                                unsafe {
                                    self.device.destroy_image_view(texture.view, None);
                                    self.device.destroy_image(texture.image, None);
                                    self.device.free_memory(texture.memory, None);
                                }
                            }
                            return Err(error);
                        }
                    }
                }
                Some(MaterialTextureBind::RenderTarget(name)) => self
                    .effect_targets
                    .get(name.as_str())
                    .map_or(self.dummy_texture.view, |fbo| fbo.view),
                None => self.dummy_texture.view,
            };
            *image_info = vk::DescriptorImageInfo::default()
                .sampler(self.sampler)
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        }
        Ok((image_infos, owned_textures))
    }

    pub fn bind_material_layer(
        &mut self,
        layer_index: usize,
        key: MaterialKey,
        textures: &[Option<MaterialTextureBind>],
        mut uniforms: MaterialUniforms,
    ) -> Result<(), RenderError> {
        if layer_index >= MAX_LAYERS {
            return Err(RenderError::Vulkan(format!(
                "layer index {layer_index} beyond the {MAX_LAYERS}-layer cap for materials"
            )));
        }
        let Some(&pipeline) = self.material_pipelines.get(&key.0) else {
            return Err(RenderError::Vulkan(
                "bind_material_layer: pipeline not registered".to_string(),
            ));
        };

        let (image_infos, owned_textures) = self.resolve_texture_slots(textures)?;

        let ubo_info = vk::BufferCreateInfo::default()
            .size(MATERIAL_UNIFORMS_SIZE as vk::DeviceSize)
            .usage(vk::BufferUsageFlags::UNIFORM_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let ubo_buffer = match unsafe { self.device.create_buffer(&ubo_info, None) } {
            Ok(buffer) => buffer,
            Err(error) => {
                for (_, texture) in owned_textures {
                    unsafe {
                        self.device.destroy_image_view(texture.view, None);
                        self.device.destroy_image(texture.image, None);
                        self.device.free_memory(texture.memory, None);
                    }
                }
                return Err(error.into());
            }
        };
        let ubo_requirements = unsafe { self.device.get_buffer_memory_requirements(ubo_buffer) };
        let ubo_memory = match allocate_host_visible(
            &self.instance,
            &self.device,
            self.physical,
            &ubo_requirements,
        ) {
            Ok(memory) => memory,
            Err(error) => {
                unsafe { self.device.destroy_buffer(ubo_buffer, None) };
                for (_, texture) in owned_textures {
                    unsafe {
                        self.device.destroy_image_view(texture.view, None);
                        self.device.destroy_image(texture.image, None);
                        self.device.free_memory(texture.memory, None);
                    }
                }
                return Err(error);
            }
        };
        unsafe { self.device.bind_buffer_memory(ubo_buffer, ubo_memory, 0) }?;
        let ubo_mapped = unsafe {
            self.device.map_memory(
                ubo_memory,
                0,
                MATERIAL_UNIFORMS_SIZE as vk::DeviceSize,
                vk::MemoryMapFlags::empty(),
            )?
        }
        .cast::<u8>();
        // g_ModelViewProjectionMatrix is overwritten every draw (`render`);
        // the caller's initial value only matters if this layer is bound
        // but never drawn (e.g. starts invisible) — identity is harmless.
        uniforms.mvp = build_orthographic_mvp([[1.0, 0.0], [0.0, 1.0]], [0.0, 0.0], 1.0, 1.0);
        unsafe {
            std::ptr::copy_nonoverlapping(
                std::ptr::from_ref(&uniforms).cast::<u8>(),
                ubo_mapped,
                MATERIAL_UNIFORMS_SIZE,
            );
        }
        // S2 review #4 (RECOMMENDED): `allocate_host_visible` prefers
        // `HOST_COHERENT` but falls back to plain `HOST_VISIBLE` if
        // unavailable (an uncommon but real Vulkan portability case,
        // e.g. some Mesa RADV/ANV memory-type layouts) — on that
        // fallback, an explicit flush is the only thing that makes this
        // write visible to the device. Mirrors `refresh_layer`'s
        // identical flush (vulkan.rs, same rationale). WHOLE_SIZE at
        // offset 0 always satisfies the nonCoherentAtomSize alignment
        // rule.
        let ubo_flush_range = vk::MappedMemoryRange::default()
            .memory(ubo_memory)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        unsafe {
            self.device
                .flush_mapped_memory_ranges(std::slice::from_ref(&ubo_flush_range))
        }?;

        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.material_descriptor_pool)
            .set_layouts(std::slice::from_ref(&self.material_descriptor_set_layout));
        let descriptor_set = match unsafe { self.device.allocate_descriptor_sets(&alloc_info) } {
            Ok(sets) => sets[0],
            Err(error) => {
                unsafe {
                    self.device.unmap_memory(ubo_memory);
                    self.device.destroy_buffer(ubo_buffer, None);
                    self.device.free_memory(ubo_memory, None);
                }
                for (_, texture) in owned_textures {
                    unsafe {
                        self.device.destroy_image_view(texture.view, None);
                        self.device.destroy_image(texture.image, None);
                        self.device.free_memory(texture.memory, None);
                    }
                }
                return Err(error.into());
            }
        };
        let mut writes: Vec<vk::WriteDescriptorSet> = Vec::with_capacity(MAX_MATERIAL_TEXTURES + 1);
        for (slot, image_info) in image_infos.iter().enumerate() {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_set)
                    .dst_binding(slot as u32)
                    .dst_array_element(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(std::slice::from_ref(image_info)),
            );
        }
        let buffer_info = vk::DescriptorBufferInfo::default()
            .buffer(ubo_buffer)
            .offset(0)
            .range(MATERIAL_UNIFORMS_SIZE as vk::DeviceSize);
        writes.push(
            vk::WriteDescriptorSet::default()
                .dst_set(descriptor_set)
                .dst_binding(MAX_MATERIAL_TEXTURES as u32)
                .dst_array_element(0)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .buffer_info(std::slice::from_ref(&buffer_info)),
        );
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };

        while self.material_bindings.len() <= layer_index {
            self.material_bindings.push(None);
        }
        if let Some(old) = self.material_bindings[layer_index].take() {
            unsafe {
                self.device.unmap_memory(old.ubo_memory);
                self.device.destroy_buffer(old.ubo_buffer, None);
                self.device.free_memory(old.ubo_memory, None);
                self.device
                    .free_descriptor_sets(
                        self.material_descriptor_pool,
                        std::slice::from_ref(&old.descriptor_set),
                    )
                    .expect("free_descriptor_sets: the material pool has FREE_DESCRIPTOR_SET");
                for (_, texture) in old.textures {
                    self.device.destroy_image_view(texture.view, None);
                    self.device.destroy_image(texture.image, None);
                    self.device.free_memory(texture.memory, None);
                }
            }
        }
        self.material_bindings[layer_index] = Some(MaterialBinding {
            pipeline,
            descriptor_set,
            textures: owned_textures,
            ubo_buffer,
            ubo_memory,
            ubo_mapped,
            uniforms,
        });
        Ok(())
    }

    /// Refresh one already-uploaded layer texture in place (M3g). A video
    /// layer re-uploads every decoded frame; `upload_layer` would create a
    /// new image, view, and descriptor set each time and free the old ones,
    /// which churns the bounded descriptor pool and the device allocator at
    /// frame rate. When the slot already holds a texture of exactly these
    /// dimensions this writes the new pixels into that image and leaves the
    /// view and descriptor set untouched; otherwise (first frame, or a
    /// resolution change) it falls back to `upload_layer`, which is the
    /// only path that may allocate.
    ///
    /// Sharing `self.fence` and `self.upload_buffer` with `render()` is safe
    /// for the same reason `sync_text`'s per-frame `upload_layer` is:
    /// `render()` waits its fence to completion before returning, so no
    /// submit is ever in flight when this runs on the worker thread.
    pub fn refresh_layer(
        &mut self,
        index: usize,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<(), RenderError> {
        let in_place = self
            .textures
            .get(index)
            .and_then(Option::as_ref)
            .is_some_and(|texture| texture.width == width && texture.height == height)
            && self
                .descriptor_sets
                .get(index)
                .and_then(Option::as_ref)
                .is_some();
        if !in_place {
            return self.upload_layer(index, rgba, width, height);
        }
        let expected = width as usize * height as usize * 4;
        if rgba.len() != expected {
            return Err(RenderError::Vulkan(format!(
                "refresh byte count {} does not match {width}x{height} RGBA8",
                rgba.len()
            )));
        }

        self.ensure_video_staging(rgba.len())?;
        let staging = self
            .video_staging
            .as_ref()
            .expect("ensure_video_staging returned Ok");
        let (buffer, memory, mapped) = (staging.buffer, staging.memory, staging.mapped);
        let image = self.textures[index]
            .as_ref()
            .expect("in_place checked the slot")
            .image;
        unsafe { std::ptr::copy_nonoverlapping(rgba.as_ptr(), mapped, rgba.len()) };
        // The staging stays mapped for its lifetime, so an explicit flush is
        // the only thing that makes the write visible to the device on a
        // host-visible-but-not-coherent memory type (allocate_host_visible
        // prefers coherent but falls back). WHOLE_SIZE at offset 0 always
        // satisfies the nonCoherentAtomSize alignment rule.
        let range = vk::MappedMemoryRange::default()
            .memory(memory)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        unsafe {
            self.device
                .flush_mapped_memory_ranges(std::slice::from_ref(&range))
        }?;

        unsafe { self.device.reset_fences(&[self.fence]) }?;
        let begin_info = vk::CommandBufferBeginInfo::default();
        unsafe {
            self.device
                .begin_command_buffer(self.upload_buffer, &begin_info)
        }?;
        let subresource = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        // SHADER_READ_ONLY_OPTIMAL -> TRANSFER_DST_OPTIMAL. Unlike the
        // upload path this cannot start from UNDEFINED: the image holds the
        // previous frame and every prior submit left it in the shader-read
        // layout. srcAccess SHADER_READ orders the last frame's sampler
        // reads before the overwrite.
        let to_transfer = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_access_mask(vk::AccessFlags::SHADER_READ)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(subresource);
        unsafe {
            self.device.cmd_pipeline_barrier(
                self.upload_buffer,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
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
                buffer,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                std::slice::from_ref(&region),
            );
        }
        let to_shader = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(subresource);
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
        let submit =
            vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&self.upload_buffer));
        unsafe { self.device.queue_submit(self.queue, &[submit], self.fence) }?;
        match unsafe {
            self.device
                .wait_for_fences(&[self.fence], true, FENCE_TIMEOUT_NS)
        } {
            Ok(()) => Ok(()),
            Err(_) => Err(RenderError::FenceTimeout),
        }
    }

    /// Create or grow the shared video staging buffer to hold at least
    /// `bytes`. Grow-only: a scene whose two decoders differ in resolution
    /// settles at the larger frame and never reallocates again. The buffer
    /// is mapped once here and stays mapped until `Drop`, so the per-frame
    /// path is a memcpy plus a flush.
    fn ensure_video_staging(&mut self, bytes: usize) -> Result<(), RenderError> {
        if self
            .video_staging
            .as_ref()
            .is_some_and(|staging| staging.bytes >= bytes)
        {
            return Ok(());
        }
        if let Some(old) = self.video_staging.take() {
            unsafe {
                self.device.unmap_memory(old.memory);
                self.device.destroy_buffer(old.buffer, None);
                self.device.free_memory(old.memory, None);
            }
        }
        let size = bytes.max(64) as vk::DeviceSize;
        let info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { self.device.create_buffer(&info, None) }?;
        let requirements = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        // Every failure past this point destroys what it created: leaving a
        // half-built staging behind would leak a frame-sized allocation on
        // each retry, and the video path retries every frame.
        let memory =
            match allocate_host_visible(&self.instance, &self.device, self.physical, &requirements)
            {
                Ok(memory) => memory,
                Err(error) => {
                    unsafe { self.device.destroy_buffer(buffer, None) };
                    return Err(error);
                }
            };
        if let Err(error) = unsafe { self.device.bind_buffer_memory(buffer, memory, 0) } {
            unsafe {
                self.device.destroy_buffer(buffer, None);
                self.device.free_memory(memory, None);
            }
            return Err(error.into());
        }
        let mapped = match unsafe {
            self.device
                .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
        } {
            Ok(pointer) => pointer.cast::<u8>(),
            Err(error) => {
                unsafe {
                    self.device.destroy_buffer(buffer, None);
                    self.device.free_memory(memory, None);
                }
                return Err(error.into());
            }
        };
        self.video_staging = Some(StagingBuffer {
            buffer,
            memory,
            mapped,
            bytes: size as usize,
        });
        Ok(())
    }

    /// Upload one text layer's quad vertex data (M3e): 6 verts per glyph
    /// of {pos.xy, uv.xy} — the same stride and layout as the unit quad.
    /// The buffer is host-visible and grown in place (create-or-grow), so
    /// per-change uploads never allocate from the device; `bytes` is
    /// bounded by the caller (text::MAX_TEXT_VERTEX_BYTES). A failed
    /// allocation returns an error the caller treats like a failed texture
    /// upload (layer skipped, renderer healthy).
    pub fn upload_text_vertices(&mut self, index: usize, bytes: &[u8]) -> Result<(), RenderError> {
        while self.text_vertex_buffers.len() <= index {
            self.text_vertex_buffers.push(None);
        }
        let entry = self.text_vertex_buffers[index].get_or_insert_with(|| LayerVertexBuffer {
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            bytes: 0,
        });
        if entry.bytes < bytes.len() {
            if entry.buffer != vk::Buffer::null() {
                unsafe { self.device.destroy_buffer(entry.buffer, None) };
                unsafe { self.device.free_memory(entry.memory, None) };
            }
            let size = bytes.len().max(64) as vk::DeviceSize;
            let info = vk::BufferCreateInfo::default()
                .size(size)
                .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let buffer = unsafe { self.device.create_buffer(&info, None) }?;
            let requirements = unsafe { self.device.get_buffer_memory_requirements(buffer) };
            let memory =
                allocate_host_visible(&self.instance, &self.device, self.physical, &requirements)?;
            unsafe { self.device.bind_buffer_memory(buffer, memory, 0) }?;
            entry.buffer = buffer;
            entry.memory = memory;
            entry.bytes = size as usize;
        }
        let mapped = unsafe {
            self.device.map_memory(
                entry.memory,
                0,
                bytes.len() as vk::DeviceSize,
                vk::MemoryMapFlags::empty(),
            )
        }?;
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapped.cast::<u8>(), bytes.len());
            self.device.unmap_memory(entry.memory);
        }
        Ok(())
    }

    /// Upload one particle system's vertex data (M3f): 6 verts per particle
    /// of {pos.xy, uv.xy, color.rgba, size, pad} — the stride-40 layout
    /// particles.rs::build_vertex_bytes writes, rebuilt every frame the
    /// simulation changes. Host-visible create-or-grow like the text
    /// buffers; `bytes` is bounded by the simulation cap (≤ MAX_PARTICLES
    /// × 6 × 40 B per system), so a system's buffer never grows past
    /// 983,040 B. A failed allocation returns an error the caller treats
    /// like a failed texture upload (system draw skipped, renderer
    /// healthy).
    pub fn upload_particle_vertices(
        &mut self,
        index: usize,
        bytes: &[u8],
    ) -> Result<(), RenderError> {
        while self.particle_vertex_buffers.len() <= index {
            self.particle_vertex_buffers.push(None);
        }
        let entry = self.particle_vertex_buffers[index].get_or_insert_with(|| LayerVertexBuffer {
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            bytes: 0,
        });
        if entry.bytes < bytes.len() {
            if entry.buffer != vk::Buffer::null() {
                unsafe { self.device.destroy_buffer(entry.buffer, None) };
                unsafe { self.device.free_memory(entry.memory, None) };
            }
            let size = bytes.len().max(64) as vk::DeviceSize;
            let info = vk::BufferCreateInfo::default()
                .size(size)
                .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let buffer = unsafe { self.device.create_buffer(&info, None) }?;
            let requirements = unsafe { self.device.get_buffer_memory_requirements(buffer) };
            let memory =
                allocate_host_visible(&self.instance, &self.device, self.physical, &requirements)?;
            unsafe { self.device.bind_buffer_memory(buffer, memory, 0) }?;
            entry.buffer = buffer;
            entry.memory = memory;
            entry.bytes = size as usize;
        }
        let mapped = unsafe {
            self.device.map_memory(
                entry.memory,
                0,
                bytes.len() as vk::DeviceSize,
                vk::MemoryMapFlags::empty(),
            )
        }?;
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapped.cast::<u8>(), bytes.len());
            self.device.unmap_memory(entry.memory);
        }
        Ok(())
    }

    fn destroy_owned_textures(&self, textures: Vec<(u32, LayerTexture)>) {
        for (_, texture) in textures {
            unsafe {
                self.device.destroy_image_view(texture.view, None);
                self.device.destroy_image(texture.image, None);
                self.device.free_memory(texture.memory, None);
            }
        }
    }

    /// S3: create the second offscreen render pass (`effect_render_pass`)
    /// every effect FBO renders through — same attachment shape as the
    /// main `render_pass` (`self.format`, `CLEAR`/`STORE`) except its
    /// `final_layout` is `SHADER_READ_ONLY_OPTIMAL` (a target meant to be
    /// SAMPLED by a later pass, not read back to a staging buffer).
    /// Created once, lazily, the first time a scene needs it — most
    /// scenes never do.
    fn ensure_effect_render_pass(&mut self) -> Result<(), RenderError> {
        if self.effect_render_pass != vk::RenderPass::null() {
            return Ok(());
        }
        let attachment = vk::AttachmentDescription::default()
            .format(self.format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
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
                .src_stage_mask(
                    vk::PipelineStageFlags::FRAGMENT_SHADER | vk::PipelineStageFlags::TRANSFER,
                )
                .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .src_access_mask(
                    vk::AccessFlags::SHADER_READ
                        | vk::AccessFlags::TRANSFER_READ
                        | vk::AccessFlags::TRANSFER_WRITE,
                )
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE),
            vk::SubpassDependency::default()
                .src_subpass(0)
                .dst_subpass(vk::SUBPASS_EXTERNAL)
                .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_stage_mask(vk::PipelineStageFlags::FRAGMENT_SHADER)
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ),
        ];
        let render_pass_info = vk::RenderPassCreateInfo::default()
            .attachments(std::slice::from_ref(&attachment))
            .subpasses(std::slice::from_ref(&subpass))
            .dependencies(&dependencies);
        self.effect_render_pass =
            unsafe { self.device.create_render_pass(&render_pass_info, None) }?;
        Ok(())
    }

    /// One empty render-pass instance (clear, no draws) against a freshly
    /// created FBO — establishes the "always cleared to transparent
    /// black, always `SHADER_READ_ONLY_OPTIMAL`" invariant every other
    /// effect-target operation in this module relies on, from the moment
    /// the FBO exists (including the very first frame, before any real
    /// pass has written it).
    ///
    /// Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
    /// src/WallpaperEngine/Render/CFBO.cpp:56-63 (the explicit
    /// clear-to-transparent-black fix: "Layer framebuffers must start
    /// transparent... otherwise effects rendering solid rectangles") @
    /// b016d7d1 — adapted (upstream clears once via `glClear` at FBO
    /// creation using the current GL context; this clears via one
    /// throwaway Vulkan render-pass instance).
    fn clear_effect_fbo(&mut self, fbo: &EffectFbo) -> Result<(), RenderError> {
        unsafe { self.device.reset_fences(&[self.fence]) }?;
        let begin_info = vk::CommandBufferBeginInfo::default();
        unsafe {
            self.device
                .begin_command_buffer(self.command_buffer, &begin_info)
        }?;
        let clear_value = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 0.0],
            },
        };
        let rp_info = vk::RenderPassBeginInfo::default()
            .render_pass(self.effect_render_pass)
            .framebuffer(fbo.framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: fbo.width,
                    height: fbo.height,
                },
            })
            .clear_values(std::slice::from_ref(&clear_value));
        unsafe {
            self.device.cmd_begin_render_pass(
                self.command_buffer,
                &rp_info,
                vk::SubpassContents::INLINE,
            );
            self.device.cmd_end_render_pass(self.command_buffer);
            self.device.end_command_buffer(self.command_buffer)?;
        }
        let submit =
            vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&self.command_buffer));
        unsafe {
            self.device
                .queue_submit(self.queue, std::slice::from_ref(&submit), self.fence)?;
        }
        match unsafe {
            self.device
                .wait_for_fences(&[self.fence], true, FENCE_TIMEOUT_NS)
        } {
            Ok(()) => Ok(()),
            Err(_) => Err(RenderError::FenceTimeout),
        }
    }

    /// Create one [`EffectFbo`] sized `width` x `height` (clamped to
    /// `MAX_EFFECT_TARGET_DIMENSION`) and clear it (`clear_effect_fbo`).
    /// On any failure past image/view/framebuffer creation, everything
    /// already created for this call is torn down before the error
    /// propagates — the caller (`prepare_effect_targets`) treats a
    /// non-fence error as "this one target doesn't exist," never a scene
    /// failure.
    fn create_effect_fbo(&mut self, width: u32, height: u32) -> Result<EffectFbo, RenderError> {
        let width = width.clamp(1, MAX_EFFECT_TARGET_DIMENSION);
        let height = height.clamp(1, MAX_EFFECT_TARGET_DIMENSION);
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(self.format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { self.device.create_image(&image_info, None) }?;
        let requirements = unsafe { self.device.get_image_memory_requirements(image) };
        let memory = match allocate(
            &self.instance,
            &self.device,
            self.physical,
            &requirements,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        ) {
            Ok(memory) => memory,
            Err(error) => {
                unsafe { self.device.destroy_image(image, None) };
                return Err(error);
            }
        };
        if let Err(error) = unsafe { self.device.bind_image_memory(image, memory, 0) } {
            unsafe {
                self.device.destroy_image(image, None);
                self.device.free_memory(memory, None);
            }
            return Err(error.into());
        }
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(self.format)
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
        let view = match unsafe { self.device.create_image_view(&view_info, None) } {
            Ok(view) => view,
            Err(error) => {
                unsafe {
                    self.device.destroy_image(image, None);
                    self.device.free_memory(memory, None);
                }
                return Err(error.into());
            }
        };
        let framebuffer_info = vk::FramebufferCreateInfo::default()
            .render_pass(self.effect_render_pass)
            .attachments(std::slice::from_ref(&view))
            .width(width)
            .height(height)
            .layers(1);
        let framebuffer = match unsafe { self.device.create_framebuffer(&framebuffer_info, None) } {
            Ok(framebuffer) => framebuffer,
            Err(error) => {
                unsafe {
                    self.device.destroy_image_view(view, None);
                    self.device.destroy_image(image, None);
                    self.device.free_memory(memory, None);
                }
                return Err(error.into());
            }
        };
        let fbo = EffectFbo {
            image,
            memory,
            view,
            framebuffer,
            width,
            height,
        };
        if let Err(error) = self.clear_effect_fbo(&fbo) {
            unsafe {
                self.device.destroy_framebuffer(fbo.framebuffer, None);
                self.device.destroy_image_view(fbo.view, None);
                self.device.destroy_image(fbo.image, None);
                self.device.free_memory(fbo.memory, None);
            }
            return Err(error);
        }
        Ok(fbo)
    }

    /// Try to create one named effect target if it does not already
    /// exist, subject to `MAX_EFFECT_TARGETS_PER_SCENE` and
    /// `MAX_EFFECT_TARGET_BYTES`. Returns whether a target was created.
    /// A `FenceTimeout` from the underlying clear propagates (fatal,
    /// matching every other fence-touching call site); any other
    /// creation failure just means this one target does not exist —
    /// never fatal.
    fn try_create_effect_target(
        &mut self,
        name: &str,
        width: u32,
        height: u32,
        budget_bytes: &mut u64,
    ) -> Result<bool, RenderError> {
        if self.effect_targets.contains_key(name)
            || self.effect_targets.len() >= MAX_EFFECT_TARGETS_PER_SCENE
        {
            return Ok(false);
        }
        let cost = u64::from(width.clamp(1, MAX_EFFECT_TARGET_DIMENSION))
            * u64::from(height.clamp(1, MAX_EFFECT_TARGET_DIMENSION))
            * 4;
        if budget_bytes.saturating_add(cost) > MAX_EFFECT_TARGET_BYTES {
            return Ok(false);
        }
        match self.create_effect_fbo(width, height) {
            Ok(fbo) => {
                *budget_bytes = budget_bytes.saturating_add(cost);
                self.effect_targets.insert(name.to_string(), fbo);
                Ok(true)
            }
            Err(error) => {
                if is_fence_timeout(&error) {
                    return Err(error);
                }
                Ok(false)
            }
        }
    }

    /// S3: create every effect render target a scene's resolved effect
    /// chains need — `_rt_FullFrameBuffer` (whenever `requests` is
    /// non-empty: at least one layer has a resolved effect chain) plus
    /// every distinct requested name (see [`EffectTargetRequest`]).
    /// Idempotent (a name already present, e.g. because two effects
    /// share an `fbos[]` name, is left alone — the documented S3 scope
    /// limit on `effect_targets`'s global namespace). Returns the number
    /// of targets actually created (diagnostics: `main.rs`'s
    /// `event=renderer.scene.effects` line).
    pub fn prepare_effect_targets(
        &mut self,
        requests: &[EffectTargetRequest],
    ) -> Result<usize, RenderError> {
        if requests.is_empty() {
            return Ok(0);
        }
        self.ensure_effect_render_pass()?;
        let mut created = 0usize;
        let mut budget_bytes: u64 = self
            .effect_targets
            .values()
            .map(|fbo| u64::from(fbo.width) * u64::from(fbo.height) * 4)
            .sum();
        if self.try_create_effect_target(
            FULL_FRAME_BUFFER,
            self.width,
            self.height,
            &mut budget_bytes,
        )? {
            created += 1;
        }
        for request in requests {
            if self.try_create_effect_target(
                &request.name,
                request.width,
                request.height,
                &mut budget_bytes,
            )? {
                created += 1;
            }
        }
        Ok(created)
    }

    /// S3: compile+bind ONE targeted effect pass — its own pipeline
    /// (against `effect_render_pass`, viewport = the TARGET FBO's own
    /// pixel size; deliberately NOT shared with `material_pipelines`,
    /// whose pipelines bake the CANVAS viewport and the main
    /// `render_pass` — this renderer's pipelines have no dynamic
    /// viewport state) plus its own descriptor set/UBO (the same
    /// 8-sampler+UBO shape every material pipeline uses, so
    /// `resolve_texture_slots` and the `MaterialUniforms` layout are
    /// shared unchanged). Bounded by `MAX_EFFECT_PASS_BINDINGS`. Returns
    /// the new binding's index into `effect_pass_bindings` — the caller
    /// records it in `effect_frame_actions` as `EffectFrameAction::Render`.
    pub fn compile_effect_pass(
        &mut self,
        vertex_spirv: &[u32],
        fragment_spirv: &[u32],
        blend_mode: BlendMode,
        target: &str,
        textures: &[Option<MaterialTextureBind>],
        mut uniforms: MaterialUniforms,
    ) -> Result<usize, RenderError> {
        if self.effect_pass_bindings.len() >= MAX_EFFECT_PASS_BINDINGS {
            return Err(RenderError::Vulkan(
                "compile_effect_pass: MAX_EFFECT_PASS_BINDINGS reached".to_string(),
            ));
        }
        let (width, height) = match self.effect_targets.get(target) {
            Some(fbo) => (fbo.width, fbo.height),
            None => {
                return Err(RenderError::Vulkan(format!(
                    "compile_effect_pass: target \"{target}\" has no live render target"
                )));
            }
        };
        let vertex_module = shader_module(&self.device, vertex_spirv)?;
        let fragment_module = match shader_module(&self.device, fragment_spirv) {
            Ok(module) => module,
            Err(error) => {
                unsafe { self.device.destroy_shader_module(vertex_module, None) };
                return Err(error);
            }
        };
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vertex_module)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fragment_module)
                .name(c"main"),
        ];
        let binding_desc = vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(20)
            .input_rate(vk::VertexInputRate::VERTEX);
        let attributes = [
            vk::VertexInputAttributeDescription::default()
                .binding(0)
                .location(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(0),
            vk::VertexInputAttributeDescription::default()
                .binding(0)
                .location(1)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(12),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(std::slice::from_ref(&binding_desc))
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
        let blend_attachment = blend_attachment_for(blend_mode);
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
            .layout(self.material_pipeline_layout)
            .render_pass(self.effect_render_pass)
            .subpass(0)
            .depth_stencil_state(&depth_stencil);
        let result = unsafe {
            self.device
                .create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
        };
        unsafe {
            self.device.destroy_shader_module(fragment_module, None);
            self.device.destroy_shader_module(vertex_module, None);
        }
        let pipeline = match result {
            Ok(pipelines) => pipelines[0],
            Err((_, result)) => return Err(result.into()),
        };

        let (image_infos, owned_textures) = match self.resolve_texture_slots(textures) {
            Ok(result) => result,
            Err(error) => {
                unsafe { self.device.destroy_pipeline(pipeline, None) };
                return Err(error);
            }
        };

        uniforms.mvp = build_orthographic_mvp([[1.0, 0.0], [0.0, 1.0]], [0.0, 0.0], 1.0, 1.0);
        let ubo_info = vk::BufferCreateInfo::default()
            .size(MATERIAL_UNIFORMS_SIZE as vk::DeviceSize)
            .usage(vk::BufferUsageFlags::UNIFORM_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let ubo_buffer = match unsafe { self.device.create_buffer(&ubo_info, None) } {
            Ok(buffer) => buffer,
            Err(error) => {
                self.destroy_owned_textures(owned_textures);
                unsafe { self.device.destroy_pipeline(pipeline, None) };
                return Err(error.into());
            }
        };
        let ubo_requirements = unsafe { self.device.get_buffer_memory_requirements(ubo_buffer) };
        let ubo_memory = match allocate_host_visible(
            &self.instance,
            &self.device,
            self.physical,
            &ubo_requirements,
        ) {
            Ok(memory) => memory,
            Err(error) => {
                unsafe { self.device.destroy_buffer(ubo_buffer, None) };
                self.destroy_owned_textures(owned_textures);
                unsafe { self.device.destroy_pipeline(pipeline, None) };
                return Err(error);
            }
        };
        unsafe { self.device.bind_buffer_memory(ubo_buffer, ubo_memory, 0) }?;
        let ubo_mapped = unsafe {
            self.device.map_memory(
                ubo_memory,
                0,
                MATERIAL_UNIFORMS_SIZE as vk::DeviceSize,
                vk::MemoryMapFlags::empty(),
            )?
        }
        .cast::<u8>();
        unsafe {
            std::ptr::copy_nonoverlapping(
                std::ptr::from_ref(&uniforms).cast::<u8>(),
                ubo_mapped,
                MATERIAL_UNIFORMS_SIZE,
            );
        }
        let ubo_flush_range = vk::MappedMemoryRange::default()
            .memory(ubo_memory)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        unsafe {
            self.device
                .flush_mapped_memory_ranges(std::slice::from_ref(&ubo_flush_range))?;
            // Unlike `bind_material_layer`'s UBO (updated every draw,
            // stays mapped for the renderer's lifetime), an effect pass's
            // UBO is written exactly once — unmap immediately rather than
            // keep a pointer nothing ever uses again (see
            // `EffectPassBinding`'s doc comment).
            self.device.unmap_memory(ubo_memory);
        }

        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.material_descriptor_pool)
            .set_layouts(std::slice::from_ref(&self.material_descriptor_set_layout));
        let descriptor_set = match unsafe { self.device.allocate_descriptor_sets(&alloc_info) } {
            Ok(sets) => sets[0],
            Err(error) => {
                unsafe {
                    self.device.destroy_buffer(ubo_buffer, None);
                    self.device.free_memory(ubo_memory, None);
                }
                self.destroy_owned_textures(owned_textures);
                unsafe { self.device.destroy_pipeline(pipeline, None) };
                return Err(error.into());
            }
        };
        let mut writes: Vec<vk::WriteDescriptorSet> = Vec::with_capacity(MAX_MATERIAL_TEXTURES + 1);
        for (slot, image_info) in image_infos.iter().enumerate() {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_set)
                    .dst_binding(slot as u32)
                    .dst_array_element(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(std::slice::from_ref(image_info)),
            );
        }
        let buffer_info = vk::DescriptorBufferInfo::default()
            .buffer(ubo_buffer)
            .offset(0)
            .range(MATERIAL_UNIFORMS_SIZE as vk::DeviceSize);
        writes.push(
            vk::WriteDescriptorSet::default()
                .dst_set(descriptor_set)
                .dst_binding(MAX_MATERIAL_TEXTURES as u32)
                .dst_array_element(0)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .buffer_info(std::slice::from_ref(&buffer_info)),
        );
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };

        self.effect_pass_bindings.push(EffectPassBinding {
            pipeline,
            descriptor_set,
            textures: owned_textures,
            ubo_buffer,
            ubo_memory,
            target: target.to_string(),
        });
        Ok(self.effect_pass_bindings.len() - 1)
    }

    /// S3: per-frame replay of every effect action recorded at scene load
    /// (`compile_material_layers`/`compile_effect_pass`, main.rs) — in
    /// order, re-render each targeted pass's fresh content or execute a
    /// `command`. Call BEFORE `render()` each frame so any LAYER's own
    /// material (not an effect pass — the chain's final untargeted pass,
    /// which becomes the layer's own material; see main.rs) samples this
    /// frame's fresh effect output, not last frame's. A single bounds
    /// check for the overwhelming majority of scenes, which have no
    /// effects at all.
    pub fn render_effect_chains(&mut self) -> Result<(), RenderError> {
        for index in 0..self.effect_frame_actions.len() {
            match self.effect_frame_actions[index] {
                EffectFrameAction::Render(binding_index) => {
                    self.render_effect_pass_binding(binding_index)?;
                }
                EffectFrameAction::Copy {
                    ref source,
                    ref target,
                } => {
                    let source = source.clone();
                    let target = target.clone();
                    self.copy_effect_target(&source, &target)?;
                }
            }
        }
        Ok(())
    }

    fn render_effect_pass_binding(&mut self, index: usize) -> Result<(), RenderError> {
        let Some(binding) = self.effect_pass_bindings.get(index) else {
            return Ok(()); // defensive: never happens by construction
        };
        let (pipeline, descriptor_set) = (binding.pipeline, binding.descriptor_set);
        let Some(fbo) = self.effect_targets.get(&binding.target) else {
            return Ok(()); // defensive: degrade, don't crash
        };
        let (framebuffer, width, height) = (fbo.framebuffer, fbo.width, fbo.height);

        unsafe { self.device.reset_fences(&[self.fence]) }?;
        let begin_info = vk::CommandBufferBeginInfo::default();
        unsafe {
            self.device
                .begin_command_buffer(self.command_buffer, &begin_info)
        }?;
        let clear_value = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 0.0],
            },
        };
        let rp_info = vk::RenderPassBeginInfo::default()
            .render_pass(self.effect_render_pass)
            .framebuffer(framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D { width, height },
            })
            .clear_values(std::slice::from_ref(&clear_value));
        unsafe {
            self.device.cmd_begin_render_pass(
                self.command_buffer,
                &rp_info,
                vk::SubpassContents::INLINE,
            );
            self.device.cmd_bind_pipeline(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline,
            );
            self.device.cmd_bind_descriptor_sets(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.material_pipeline_layout,
                0,
                std::slice::from_ref(&descriptor_set),
                &[],
            );
            self.device.cmd_bind_vertex_buffers(
                self.command_buffer,
                0,
                std::slice::from_ref(&self.material_vertex_buffer),
                &[0],
            );
            self.device.cmd_draw(self.command_buffer, 6, 1, 0, 0);
            self.device.cmd_end_render_pass(self.command_buffer);
            self.device.end_command_buffer(self.command_buffer)?;
        }
        let submit =
            vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&self.command_buffer));
        unsafe {
            self.device
                .queue_submit(self.queue, std::slice::from_ref(&submit), self.fence)?;
        }
        match unsafe {
            self.device
                .wait_for_fences(&[self.fence], true, FENCE_TIMEOUT_NS)
        } {
            Ok(()) => Ok(()),
            Err(_) => Err(RenderError::FenceTimeout),
        }
    }

    /// `command: copy` (and `swap`, executed identically — see
    /// `EffectFrameAction::Copy`'s doc comment for why): copy `source`'s
    /// CURRENT content into `target`'s image. Either name missing from
    /// `effect_targets`, or `source == target`, is a silent no-op —
    /// degrade, not fail, matching every other unresolved-effect-
    /// reference case in this module.
    fn copy_effect_target(&mut self, source: &str, target: &str) -> Result<(), RenderError> {
        if source == target {
            return Ok(());
        }
        let (Some(src), Some(dst)) = (
            self.effect_targets.get(source),
            self.effect_targets.get(target),
        ) else {
            return Ok(());
        };
        let (src_image, dst_image) = (src.image, dst.image);
        let copy_width = src.width.min(dst.width);
        let copy_height = src.height.min(dst.height);

        unsafe { self.device.reset_fences(&[self.fence]) }?;
        let begin_info = vk::CommandBufferBeginInfo::default();
        unsafe {
            self.device
                .begin_command_buffer(self.command_buffer, &begin_info)
        }?;
        let subresource = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let to_transfer_src = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_access_mask(vk::AccessFlags::SHADER_READ)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(src_image)
            .subresource_range(subresource);
        let to_transfer_dst = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_access_mask(vk::AccessFlags::SHADER_READ)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(dst_image)
            .subresource_range(subresource);
        unsafe {
            self.device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_transfer_src, to_transfer_dst],
            );
        }
        let subresource_layers = vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        };
        let region = vk::ImageCopy::default()
            .src_subresource(subresource_layers)
            .src_offset(vk::Offset3D::default())
            .dst_subresource(subresource_layers)
            .dst_offset(vk::Offset3D::default())
            .extent(vk::Extent3D {
                width: copy_width,
                height: copy_height,
                depth: 1,
            });
        unsafe {
            self.device.cmd_copy_image(
                self.command_buffer,
                src_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dst_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                std::slice::from_ref(&region),
            );
        }
        let back_src = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(src_image)
            .subresource_range(subresource);
        let back_dst = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(dst_image)
            .subresource_range(subresource);
        unsafe {
            self.device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[back_src, back_dst],
            );
            self.device.end_command_buffer(self.command_buffer)?;
        }
        let submit =
            vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&self.command_buffer));
        unsafe {
            self.device
                .queue_submit(self.queue, std::slice::from_ref(&submit), self.fence)?;
        }
        match unsafe {
            self.device
                .wait_for_fences(&[self.fence], true, FENCE_TIMEOUT_NS)
        } {
            Ok(()) => Ok(()),
            Err(_) => Err(RenderError::FenceTimeout),
        }
    }

    /// S3: refresh `_rt_FullFrameBuffer` with this frame's finished
    /// composite (`self.image`, already `TRANSFER_SRC_OPTIMAL` — the main
    /// render pass's own `final_layout`, no barrier needed on that side).
    /// Call AFTER `render()` succeeds. A no-op when no scene layer
    /// resolved an effect chain (the target was never created).
    ///
    /// Documented, bounded simplification versus upstream's strictly
    /// paint-order-dependent `_rt_FullFrameBuffer` visibility (see
    /// `effect_targets`'s doc comment): every effect chain that reads
    /// `_rt_FullFrameBuffer`/the `"previous"` sentinel at the START of its
    /// chain sees the PREVIOUS frame's fully composited output — a
    /// one-frame lag, imperceptible for a steady-state animated
    /// wallpaper — rather than a same-frame, per-object incremental
    /// snapshot.
    pub fn snapshot_full_frame_buffer(&mut self) -> Result<(), RenderError> {
        let Some(fbo) = self.effect_targets.get(FULL_FRAME_BUFFER) else {
            return Ok(());
        };
        let dst_image = fbo.image;
        let copy_width = fbo.width.min(self.width);
        let copy_height = fbo.height.min(self.height);

        unsafe { self.device.reset_fences(&[self.fence]) }?;
        let begin_info = vk::CommandBufferBeginInfo::default();
        unsafe {
            self.device
                .begin_command_buffer(self.command_buffer, &begin_info)
        }?;
        let subresource = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let to_transfer_dst = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_access_mask(vk::AccessFlags::SHADER_READ)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(dst_image)
            .subresource_range(subresource);
        unsafe {
            self.device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_transfer_dst),
            );
        }
        let subresource_layers = vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        };
        let region = vk::ImageCopy::default()
            .src_subresource(subresource_layers)
            .src_offset(vk::Offset3D::default())
            .dst_subresource(subresource_layers)
            .dst_offset(vk::Offset3D::default())
            .extent(vk::Extent3D {
                width: copy_width,
                height: copy_height,
                depth: 1,
            });
        unsafe {
            self.device.cmd_copy_image(
                self.command_buffer,
                self.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dst_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                std::slice::from_ref(&region),
            );
        }
        let back_dst = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(dst_image)
            .subresource_range(subresource);
        unsafe {
            self.device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&back_dst),
            );
            self.device.end_command_buffer(self.command_buffer)?;
        }
        let submit =
            vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&self.command_buffer));
        unsafe {
            self.device
                .queue_submit(self.queue, std::slice::from_ref(&submit), self.fence)?;
        }
        match unsafe {
            self.device
                .wait_for_fences(&[self.fence], true, FENCE_TIMEOUT_NS)
        } {
            Ok(()) => Ok(()),
            Err(_) => Err(RenderError::FenceTimeout),
        }
    }

    /// S3: record one `EffectFrameAction::Render` action for a targeted
    /// pass just compiled (`compile_effect_pass`'s returned index) plus
    /// every pending command action queued before it — called by
    /// `main.rs` in each layer's own chain order so the replay order in
    /// `render_effect_chains` matches.
    pub fn queue_effect_render(&mut self, binding_index: usize) {
        self.effect_frame_actions
            .push(EffectFrameAction::Render(binding_index));
    }

    /// S3: record one `command: copy`/`swap` action in chain order (see
    /// `queue_effect_render`).
    pub fn queue_effect_copy(&mut self, source: String, target: String) {
        self.effect_frame_actions
            .push(EffectFrameAction::Copy { source, target });
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
            // S2: a material draw goes through its own compiled pipeline
            // and 8-sampler+UBO descriptor set instead of the S1
            // single-sampler path below — `frame_draws` only ever sets
            // `material: true` after `bind_material_layer` succeeded, so
            // the `else` here (no binding) is a defense, not a normal
            // path.
            if draw.material {
                self.material_frame_counter += 1;
                let time = self.material_frame_counter as f32 / 60.0;
                let world_width = self.world_width;
                let world_height = self.world_height;
                let Some(binding) = self
                    .material_bindings
                    .get_mut(draw.layer_index)
                    .and_then(|slot| slot.as_mut())
                else {
                    continue;
                };
                // Per-instance dynamic uniforms: the model transform
                // (`materialshader::build_orthographic_mvp` — the same
                // world-extent math the S1 push-constant path applies, so
                // a material draw and the S1 quad draw of the same layer
                // land on identical pixels) plus g_Time/g_UserAlpha/
                // g_Brightness. Everything else in the UBO (textures,
                // material constants, points/parallax/pointer defaults)
                // was set once at `bind_material_layer` time.
                binding.uniforms.mvp =
                    build_orthographic_mvp(draw.m, draw.t, world_width, world_height);
                binding.uniforms.time_alpha_brightness = [time, draw.alpha, draw.brightness, 0.0];
                let uniforms_ptr = std::ptr::from_ref(&binding.uniforms).cast::<u8>();
                let ubo_mapped = binding.ubo_mapped;
                let ubo_memory = binding.ubo_memory;
                let pipeline = binding.pipeline;
                let descriptor_set = binding.descriptor_set;
                unsafe {
                    std::ptr::copy_nonoverlapping(uniforms_ptr, ubo_mapped, MATERIAL_UNIFORMS_SIZE);
                }
                // S2 review #4 (RECOMMENDED): same non-coherent-fallback
                // hazard as the initial write in `bind_material_layer` —
                // every per-draw uniform update needs its own flush, not
                // just the first one, since a non-coherent memory type
                // gives no implicit visibility guarantee across writes.
                let ubo_flush_range = vk::MappedMemoryRange::default()
                    .memory(ubo_memory)
                    .offset(0)
                    .size(vk::WHOLE_SIZE);
                unsafe {
                    self.device
                        .flush_mapped_memory_ranges(std::slice::from_ref(&ubo_flush_range))
                }?;
                unsafe {
                    self.device.cmd_bind_pipeline(
                        self.command_buffer,
                        vk::PipelineBindPoint::GRAPHICS,
                        pipeline,
                    );
                    self.device.cmd_bind_descriptor_sets(
                        self.command_buffer,
                        vk::PipelineBindPoint::GRAPHICS,
                        self.material_pipeline_layout,
                        0,
                        std::slice::from_ref(&descriptor_set),
                        &[],
                    );
                    self.device.cmd_bind_vertex_buffers(
                        self.command_buffer,
                        0,
                        std::slice::from_ref(&self.material_vertex_buffer),
                        &[0],
                    );
                    self.device.cmd_draw(self.command_buffer, 6, 1, 0, 0);
                }
                continue;
            }
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
            // M3f: the pipeline family follows the draw kind — particle
            // draws bind the particle variant (same per-mode blend state,
            // different shaders and vertex input).
            let pipeline = match draw.kind {
                DrawKind::Particles { .. } => self.particle_pipelines[variant],
                _ => self.pipelines[variant],
            };
            unsafe {
                self.device.cmd_bind_pipeline(
                    self.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    pipeline,
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
                self.world_width,
                self.world_height,
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
                // M3e/M3f: the vertex source depends on the draw kind —
                // image draws use the shared unit quad, text draws bind
                // the layer's own quad buffer (geometry rebuilt on change,
                // never per frame), particle draws bind the system's
                // per-frame vertex buffer. Binding inside the pass per
                // draw is what makes the per-layer vertex data work.
                let vertex_count = match draw.kind {
                    DrawKind::Image => {
                        self.device.cmd_bind_vertex_buffers(
                            self.command_buffer,
                            0,
                            std::slice::from_ref(&self.vertex_buffer),
                            &[0],
                        );
                        // The quad is two fan-ordered triangles in the
                        // vertex buffer ([v0,v1,v2, v0,v2,v3]); one
                        // 6-vertex TRIANGLE_LIST draw emits both. The
                        // original half-quad bug was NOT the draw shape —
                        // it was the vertex-buffer size: an element/byte
                        // mix-up sized the buffer at 16 bytes (one
                        // vertex), so the GPU's reads of vertices 2..5 ran
                        // out of bounds and the second triangle rasterized
                        // garbage (found via the isolated_draw probe; see
                        // `new`).
                        6
                    }
                    DrawKind::Text { vertex_count } => {
                        let Some(buffer) = self
                            .text_vertex_buffers
                            .get(draw.layer_index)
                            .and_then(|slot| slot.as_ref())
                        else {
                            // Defense: the draw list builder already skips
                            // text layers without vertex data.
                            continue;
                        };
                        self.device.cmd_bind_vertex_buffers(
                            self.command_buffer,
                            0,
                            std::slice::from_ref(&buffer.buffer),
                            &[0],
                        );
                        vertex_count
                    }
                    // M3f: particle systems bind their own per-frame vertex
                    // buffer (rebuild-on-change, never per draw); the draw
                    // list builder only emits these for systems with live
                    // vertices and an uploaded texture, so the missing
                    // buffer branch is a defense.
                    DrawKind::Particles { vertex_count } => {
                        let system = draw.layer_index.saturating_sub(MAX_LAYERS);
                        let Some(buffer) = self
                            .particle_vertex_buffers
                            .get(system)
                            .and_then(|slot| slot.as_ref())
                        else {
                            continue;
                        };
                        self.device.cmd_bind_vertex_buffers(
                            self.command_buffer,
                            0,
                            std::slice::from_ref(&buffer.buffer),
                            &[0],
                        );
                        vertex_count
                    }
                };
                self.device
                    .cmd_draw(self.command_buffer, vertex_count, 1, 0, 0);
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
            // S2: every material binding's textures/UBO, then the shared
            // material pipelines/layouts/pool/vertex buffer/dummy texture.
            // Descriptor sets are freed implicitly by
            // `destroy_descriptor_pool` below (no `FREE_DESCRIPTOR_SET`
            // per-set free needed at teardown).
            for binding in self.material_bindings.iter().flatten() {
                self.device.unmap_memory(binding.ubo_memory);
                self.device.destroy_buffer(binding.ubo_buffer, None);
                self.device.free_memory(binding.ubo_memory, None);
                for (_, texture) in &binding.textures {
                    self.device.destroy_image_view(texture.view, None);
                    self.device.destroy_image(texture.image, None);
                    self.device.free_memory(texture.memory, None);
                }
            }
            for pipeline in self.material_pipelines.values() {
                self.device.destroy_pipeline(*pipeline, None);
            }
            // S3: every targeted effect pass's own pipeline/UBO/textures
            // (never shared with `material_pipelines` — see
            // `compile_effect_pass`'s doc comment), then every live
            // effect render target, then the second render pass they all
            // share (if it was ever created).
            for binding in &self.effect_pass_bindings {
                // Already unmapped in `compile_effect_pass` (written once,
                // no lifetime-long mapped pointer kept — unlike
                // `material_bindings`, unmapped above).
                self.device.destroy_buffer(binding.ubo_buffer, None);
                self.device.free_memory(binding.ubo_memory, None);
                for (_, texture) in &binding.textures {
                    self.device.destroy_image_view(texture.view, None);
                    self.device.destroy_image(texture.image, None);
                    self.device.free_memory(texture.memory, None);
                }
                self.device.destroy_pipeline(binding.pipeline, None);
            }
            for fbo in self.effect_targets.values() {
                self.device.destroy_framebuffer(fbo.framebuffer, None);
                self.device.destroy_image_view(fbo.view, None);
                self.device.destroy_image(fbo.image, None);
                self.device.free_memory(fbo.memory, None);
            }
            if self.effect_render_pass != vk::RenderPass::null() {
                self.device
                    .destroy_render_pass(self.effect_render_pass, None);
            }
            self.device
                .destroy_pipeline_layout(self.material_pipeline_layout, None);
            self.device
                .destroy_descriptor_pool(self.material_descriptor_pool, None);
            self.device
                .destroy_descriptor_set_layout(self.material_descriptor_set_layout, None);
            self.device
                .destroy_buffer(self.material_vertex_buffer, None);
            self.device
                .free_memory(self.material_vertex_buffer_memory, None);
            self.device
                .destroy_image_view(self.dummy_texture.view, None);
            self.device.destroy_image(self.dummy_texture.image, None);
            self.device.free_memory(self.dummy_texture.memory, None);

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
            if let Some(staging) = self.video_staging.take() {
                self.device.unmap_memory(staging.memory);
                self.device.destroy_buffer(staging.buffer, None);
                self.device.free_memory(staging.memory, None);
            }
            for texture in self.textures.iter().flatten() {
                self.device.destroy_image_view(texture.view, None);
                self.device.destroy_image(texture.image, None);
                self.device.free_memory(texture.memory, None);
            }
            // M3e: per-layer text vertex buffers.
            for buffer in self.text_vertex_buffers.iter().flatten() {
                self.device.destroy_buffer(buffer.buffer, None);
                self.device.free_memory(buffer.memory, None);
            }
            // M3f: per-system particle vertex buffers and the particle
            // pipeline family.
            for buffer in self.particle_vertex_buffers.iter().flatten() {
                self.device.destroy_buffer(buffer.buffer, None);
                self.device.free_memory(buffer.memory, None);
            }
            for pipeline in &self.particle_pipelines {
                self.device.destroy_pipeline(*pipeline, None);
            }
            self.device
                .destroy_shader_module(self.particle_fragment_module, None);
            self.device
                .destroy_shader_module(self.particle_vertex_module, None);
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

/// S2: upload one RGBA8 image to a device-local sampled image and wait for
/// it, returning the raw `(image, memory, view)` triple with no descriptor
/// set — the caller owns binding it into whatever descriptor set makes
/// sense (a material's 8-sampler set, unlike `upload_layer`'s one-sampler
/// set). A free function (not a method) so `LayerRenderer::new_with` can
/// call it before `Self` exists (for the shared dummy texture) and
/// `bind_material_layer` can call it once `self` exists, from the same
/// code path either way.
///
/// Mirrors `upload_layer`'s image-create/stage/copy/barrier/view sequence;
/// duplicated rather than factored out of `upload_layer` to avoid
/// reworking that function's already-reviewed error/cleanup paths for a
/// second, structurally different caller (material texture bytes are
/// already decoded RGBA8 the same as layer textures, so the upload
/// mechanics are identical — only what happens to the result differs).
#[allow(clippy::too_many_arguments)]
fn upload_image_now(
    instance: &Instance,
    device: &Device,
    physical: vk::PhysicalDevice,
    queue: vk::Queue,
    upload_buffer: vk::CommandBuffer,
    fence: vk::Fence,
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Result<LayerTexture, RenderError> {
    let expected = width as usize * height as usize * 4;
    if rgba.len() != expected {
        return Err(RenderError::Vulkan(format!(
            "texture byte count {} does not match {width}x{height} RGBA8",
            rgba.len()
        )));
    }
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
    let image = unsafe { device.create_image(&image_info, None) }?;
    let mut memory: Option<vk::DeviceMemory> = None;
    let mut staging: Option<vk::Buffer> = None;
    let mut staging_memory: Option<vk::DeviceMemory> = None;
    let mut staging_mapped = false;
    let outcome = (|| -> Result<vk::ImageView, RenderError> {
        let requirements = unsafe { device.get_image_memory_requirements(image) };
        let allocated = allocate(
            instance,
            device,
            physical,
            &requirements,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )?;
        memory = Some(allocated);
        unsafe { device.bind_image_memory(image, allocated, 0) }?;

        let staging_info = vk::BufferCreateInfo::default()
            .size(rgba.len() as vk::DeviceSize)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let created_staging = unsafe { device.create_buffer(&staging_info, None) }?;
        staging = Some(created_staging);
        let staging_requirements =
            unsafe { device.get_buffer_memory_requirements(created_staging) };
        let created_staging_memory =
            allocate_host_visible(instance, device, physical, &staging_requirements)?;
        staging_memory = Some(created_staging_memory);
        unsafe { device.bind_buffer_memory(created_staging, created_staging_memory, 0) }?;
        let mapped = unsafe {
            device.map_memory(
                created_staging_memory,
                0,
                rgba.len() as vk::DeviceSize,
                vk::MemoryMapFlags::empty(),
            )?
        }
        .cast::<u8>();
        staging_mapped = true;
        unsafe { std::ptr::copy_nonoverlapping(rgba.as_ptr(), mapped, rgba.len()) };
        let staging = created_staging;
        let staging_memory = created_staging_memory;

        unsafe { device.reset_fences(&[fence]) }?;
        let begin_info = vk::CommandBufferBeginInfo::default();
        unsafe { device.begin_command_buffer(upload_buffer, &begin_info) }?;
        let to_transfer = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        unsafe {
            device.cmd_pipeline_barrier(
                upload_buffer,
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
            device.cmd_copy_buffer_to_image(
                upload_buffer,
                staging,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                std::slice::from_ref(&region),
            );
        }
        let to_shader = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        unsafe {
            device.cmd_pipeline_barrier(
                upload_buffer,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_shader),
            );
        }
        unsafe { device.end_command_buffer(upload_buffer) }?;
        let submit =
            vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&upload_buffer));
        unsafe { device.queue_submit(queue, &[submit], fence) }?;
        match unsafe { device.wait_for_fences(&[fence], true, FENCE_TIMEOUT_NS) } {
            Ok(()) => {}
            Err(_) => return Err(RenderError::FenceTimeout),
        }
        unsafe {
            device.unmap_memory(staging_memory);
            device.destroy_buffer(staging, None);
            device.free_memory(staging_memory, None);
        }
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
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
        Ok(unsafe { device.create_image_view(&view_info, None) }?)
    })();
    match outcome {
        Ok(view) => Ok(LayerTexture {
            image,
            // `bind_image_memory` above already succeeded, so `memory` is
            // always `Some` on this path — the allocation is the first
            // fallible step in the closure and every step after it either
            // returns `Err` (leaving `outcome` an `Err`, not reaching
            // here) or continues with `memory` already set.
            memory: memory.expect("memory bound before Ok(view)"),
            view,
            width,
            height,
        }),
        Err(error) => {
            if is_fence_timeout(&error) {
                // The queue submit may still be reading the staging
                // buffer/destination image — see `upload_layer`'s
                // identical note. Leak the handles to process teardown;
                // the caller (LayerRenderer::render's fence-timeout path)
                // terminates the process immediately.
                return Err(error);
            }
            if staging_mapped && let Some(memory) = staging_memory {
                unsafe { device.unmap_memory(memory) };
            }
            if let Some(buffer) = staging {
                unsafe { device.destroy_buffer(buffer, None) };
            }
            if let Some(memory) = staging_memory {
                unsafe { device.free_memory(memory, None) };
            }
            if let Some(memory) = memory {
                unsafe { device.free_memory(memory, None) };
            }
            unsafe { device.destroy_image(image, None) };
            Err(error)
        }
    }
}

/// The M3f particle vertex shader: pos + uv + color + size per vertex,
/// transformed by the shared push-constant model (world = mat2(m0.xy,
/// m1.xy)·pos + (m0.z, m1.z); NDC = world × 2 / viewport; NO y-flip
/// — see quad.vert for the orientation contract). Compiled with
/// glslangValidator -V --target-env vulkan1.2 from shaders/particle.vert.
#[rustfmt::skip]
const PARTICLE_VERT_SPIRV: &[u32] = &[
    0x07230203, 0x00010500, 0x0008000b, 0x0000004d, 0x00000000, 0x00020011, 0x00000001, 0x0006000b,
    0x00000001, 0x4c534c47, 0x6474732e, 0x3035342e, 0x00000000, 0x0003000e, 0x00000000, 0x00000001,
    0x000d000f, 0x00000000, 0x00000004, 0x6e69616d, 0x00000000, 0x0000000d, 0x00000023, 0x0000003c,
    0x00000044, 0x00000045, 0x00000047, 0x00000049, 0x0000004c, 0x00030003, 0x00000002, 0x000001c2,
    0x00040005, 0x00000004, 0x6e69616d, 0x00000000, 0x00040005, 0x00000009, 0x6c726f77, 0x00000064,
    0x00030005, 0x0000000b, 0x00004350, 0x00040006, 0x0000000b, 0x00000000, 0x0000306d, 0x00040006,
    0x0000000b, 0x00000001, 0x0000316d, 0x00060006, 0x0000000b, 0x00000002, 0x77656976, 0x74726f70,
    0x00000000, 0x00030005, 0x0000000d, 0x00006370, 0x00040005, 0x00000023, 0x736f5061, 0x00000000,
    0x00030005, 0x0000002f, 0x0063646e, 0x00060005, 0x0000003a, 0x505f6c67, 0x65567265, 0x78657472,
    0x00000000, 0x00060006, 0x0000003a, 0x00000000, 0x505f6c67, 0x7469736f, 0x006e6f69, 0x00070006,
    0x0000003a, 0x00000001, 0x505f6c67, 0x746e696f, 0x657a6953, 0x00000000, 0x00070006, 0x0000003a,
    0x00000002, 0x435f6c67, 0x4470696c, 0x61747369, 0x0065636e, 0x00070006, 0x0000003a, 0x00000003,
    0x435f6c67, 0x446c6c75, 0x61747369, 0x0065636e, 0x00030005, 0x0000003c, 0x00000000, 0x00030005,
    0x00000044, 0x00565576, 0x00030005, 0x00000045, 0x00565561, 0x00040005, 0x00000047, 0x6c6f4376,
    0x0000726f, 0x00040005, 0x00000049, 0x6c6f4361, 0x0000726f, 0x00040005, 0x0000004c, 0x7a695361,
    0x00000065, 0x00030047, 0x0000000b, 0x00000002, 0x00050048, 0x0000000b, 0x00000000, 0x00000023,
    0x00000000, 0x00050048, 0x0000000b, 0x00000001, 0x00000023, 0x00000010, 0x00050048, 0x0000000b,
    0x00000002, 0x00000023, 0x00000020, 0x00040047, 0x00000023, 0x0000001e, 0x00000000, 0x00030047,
    0x0000003a, 0x00000002, 0x00050048, 0x0000003a, 0x00000000, 0x0000000b, 0x00000000, 0x00050048,
    0x0000003a, 0x00000001, 0x0000000b, 0x00000001, 0x00050048, 0x0000003a, 0x00000002, 0x0000000b,
    0x00000003, 0x00050048, 0x0000003a, 0x00000003, 0x0000000b, 0x00000004, 0x00040047, 0x00000044,
    0x0000001e, 0x00000000, 0x00040047, 0x00000045, 0x0000001e, 0x00000001, 0x00040047, 0x00000047,
    0x0000001e, 0x00000001, 0x00040047, 0x00000049, 0x0000001e, 0x00000002, 0x00040047, 0x0000004c,
    0x0000001e, 0x00000003, 0x00020013, 0x00000002, 0x00030021, 0x00000003, 0x00000002, 0x00030016,
    0x00000006, 0x00000020, 0x00040017, 0x00000007, 0x00000006, 0x00000002, 0x00040020, 0x00000008,
    0x00000007, 0x00000007, 0x00040017, 0x0000000a, 0x00000006, 0x00000004, 0x0005001e, 0x0000000b,
    0x0000000a, 0x0000000a, 0x0000000a, 0x00040020, 0x0000000c, 0x00000009, 0x0000000b, 0x0004003b,
    0x0000000c, 0x0000000d, 0x00000009, 0x00040015, 0x0000000e, 0x00000020, 0x00000001, 0x0004002b,
    0x0000000e, 0x0000000f, 0x00000000, 0x00040020, 0x00000010, 0x00000009, 0x0000000a, 0x0004002b,
    0x0000000e, 0x00000014, 0x00000001, 0x00040018, 0x00000018, 0x00000007, 0x00000002, 0x0004002b,
    0x00000006, 0x00000019, 0x3f800000, 0x0004002b, 0x00000006, 0x0000001a, 0x00000000, 0x00040020,
    0x00000022, 0x00000001, 0x00000007, 0x0004003b, 0x00000022, 0x00000023, 0x00000001, 0x00040015,
    0x00000026, 0x00000020, 0x00000000, 0x0004002b, 0x00000026, 0x00000027, 0x00000002, 0x00040020,
    0x00000028, 0x00000009, 0x00000006, 0x0004002b, 0x00000006, 0x00000031, 0x40000000, 0x0004002b,
    0x0000000e, 0x00000033, 0x00000002, 0x0004002b, 0x00000026, 0x00000038, 0x00000001, 0x0004001c,
    0x00000039, 0x00000006, 0x00000038, 0x0006001e, 0x0000003a, 0x0000000a, 0x00000006, 0x00000039,
    0x00000039, 0x00040020, 0x0000003b, 0x00000003, 0x0000003a, 0x0004003b, 0x0000003b, 0x0000003c,
    0x00000003, 0x00040020, 0x00000041, 0x00000003, 0x0000000a, 0x00040020, 0x00000043, 0x00000003,
    0x00000007, 0x0004003b, 0x00000043, 0x00000044, 0x00000003, 0x0004003b, 0x00000022, 0x00000045,
    0x00000001, 0x0004003b, 0x00000041, 0x00000047, 0x00000003, 0x00040020, 0x00000048, 0x00000001,
    0x0000000a, 0x0004003b, 0x00000048, 0x00000049, 0x00000001, 0x00040020, 0x0000004b, 0x00000001,
    0x00000006, 0x0004003b, 0x0000004b, 0x0000004c, 0x00000001, 0x00050036, 0x00000002, 0x00000004,
    0x00000000, 0x00000003, 0x000200f8, 0x00000005, 0x0004003b, 0x00000008, 0x00000009, 0x00000007,
    0x0004003b, 0x00000008, 0x0000002f, 0x00000007, 0x00050041, 0x00000010, 0x00000011, 0x0000000d,
    0x0000000f, 0x0004003d, 0x0000000a, 0x00000012, 0x00000011, 0x0007004f, 0x00000007, 0x00000013,
    0x00000012, 0x00000012, 0x00000000, 0x00000001, 0x00050041, 0x00000010, 0x00000015, 0x0000000d,
    0x00000014, 0x0004003d, 0x0000000a, 0x00000016, 0x00000015, 0x0007004f, 0x00000007, 0x00000017,
    0x00000016, 0x00000016, 0x00000000, 0x00000001, 0x00050051, 0x00000006, 0x0000001b, 0x00000013,
    0x00000000, 0x00050051, 0x00000006, 0x0000001c, 0x00000013, 0x00000001, 0x00050051, 0x00000006,
    0x0000001d, 0x00000017, 0x00000000, 0x00050051, 0x00000006, 0x0000001e, 0x00000017, 0x00000001,
    0x00050050, 0x00000007, 0x0000001f, 0x0000001b, 0x0000001c, 0x00050050, 0x00000007, 0x00000020,
    0x0000001d, 0x0000001e, 0x00050050, 0x00000018, 0x00000021, 0x0000001f, 0x00000020, 0x0004003d,
    0x00000007, 0x00000024, 0x00000023, 0x00050091, 0x00000007, 0x00000025, 0x00000021, 0x00000024,
    0x00060041, 0x00000028, 0x00000029, 0x0000000d, 0x0000000f, 0x00000027, 0x0004003d, 0x00000006,
    0x0000002a, 0x00000029, 0x00060041, 0x00000028, 0x0000002b, 0x0000000d, 0x00000014, 0x00000027,
    0x0004003d, 0x00000006, 0x0000002c, 0x0000002b, 0x00050050, 0x00000007, 0x0000002d, 0x0000002a,
    0x0000002c, 0x00050081, 0x00000007, 0x0000002e, 0x00000025, 0x0000002d, 0x0003003e, 0x00000009,
    0x0000002e, 0x0004003d, 0x00000007, 0x00000030, 0x00000009, 0x0005008e, 0x00000007, 0x00000032,
    0x00000030, 0x00000031, 0x00050041, 0x00000010, 0x00000034, 0x0000000d, 0x00000033, 0x0004003d,
    0x0000000a, 0x00000035, 0x00000034, 0x0007004f, 0x00000007, 0x00000036, 0x00000035, 0x00000035,
    0x00000000, 0x00000001, 0x00050088, 0x00000007, 0x00000037, 0x00000032, 0x00000036, 0x0003003e,
    0x0000002f, 0x00000037, 0x0004003d, 0x00000007, 0x0000003d, 0x0000002f, 0x00050051, 0x00000006,
    0x0000003e, 0x0000003d, 0x00000000, 0x00050051, 0x00000006, 0x0000003f, 0x0000003d, 0x00000001,
    0x00070050, 0x0000000a, 0x00000040, 0x0000003e, 0x0000003f, 0x0000001a, 0x00000019, 0x00050041,
    0x00000041, 0x00000042, 0x0000003c, 0x0000000f, 0x0003003e, 0x00000042, 0x00000040, 0x0004003d,
    0x00000007, 0x00000046, 0x00000045, 0x0003003e, 0x00000044, 0x00000046, 0x0004003d, 0x0000000a,
    0x0000004a, 0x00000049, 0x0003003e, 0x00000047, 0x0000004a, 0x000100fd, 0x00010038,
];

/// The M3f particle fragment shader: sample the system texture,
/// multiply by the per-particle color carried in the vertex attributes
/// (instance factors folded in on the CPU), apply the M3d effects
/// (brightness × tint), scale alpha by the pushed layer alpha.
/// Compiled with glslangValidator -V --target-env vulkan1.2 from
/// shaders/particle.frag.
#[rustfmt::skip]
const PARTICLE_FRAG_SPIRV: &[u32] = &[
    0x07230203, 0x00010500, 0x0008000b, 0x0000004c, 0x00000000, 0x00020011, 0x00000001, 0x0006000b,
    0x00000001, 0x4c534c47, 0x6474732e, 0x3035342e, 0x00000000, 0x0003000e, 0x00000000, 0x00000001,
    0x000a000f, 0x00000004, 0x00000004, 0x6e69616d, 0x00000000, 0x0000000d, 0x00000011, 0x00000015,
    0x0000001b, 0x0000003e, 0x00030010, 0x00000004, 0x00000007, 0x00030003, 0x00000002, 0x000001c2,
    0x00040005, 0x00000004, 0x6e69616d, 0x00000000, 0x00030005, 0x00000009, 0x00000063, 0x00030005,
    0x0000000d, 0x00786574, 0x00030005, 0x00000011, 0x00565576, 0x00040005, 0x00000015, 0x6c6f4376,
    0x0000726f, 0x00030005, 0x00000019, 0x00004350, 0x00040006, 0x00000019, 0x00000000, 0x0000306d,
    0x00040006, 0x00000019, 0x00000001, 0x0000316d, 0x00060006, 0x00000019, 0x00000002, 0x77656976,
    0x74726f70, 0x00000000, 0x00050006, 0x00000019, 0x00000003, 0x65666665, 0x00737463, 0x00030005,
    0x0000001b, 0x00006370, 0x00050005, 0x0000003e, 0x4374756f, 0x726f6c6f, 0x00000000, 0x00040047,
    0x0000000d, 0x00000021, 0x00000000, 0x00040047, 0x0000000d, 0x00000022, 0x00000000, 0x00040047,
    0x00000011, 0x0000001e, 0x00000000, 0x00040047, 0x00000015, 0x0000001e, 0x00000001, 0x00030047,
    0x00000019, 0x00000002, 0x00050048, 0x00000019, 0x00000000, 0x00000023, 0x00000000, 0x00050048,
    0x00000019, 0x00000001, 0x00000023, 0x00000010, 0x00050048, 0x00000019, 0x00000002, 0x00000023,
    0x00000020, 0x00050048, 0x00000019, 0x00000003, 0x00000023, 0x00000030, 0x00040047, 0x0000003e,
    0x0000001e, 0x00000000, 0x00020013, 0x00000002, 0x00030021, 0x00000003, 0x00000002, 0x00030016,
    0x00000006, 0x00000020, 0x00040017, 0x00000007, 0x00000006, 0x00000004, 0x00040020, 0x00000008,
    0x00000007, 0x00000007, 0x00090019, 0x0000000a, 0x00000006, 0x00000001, 0x00000000, 0x00000000,
    0x00000000, 0x00000001, 0x00000000, 0x0003001b, 0x0000000b, 0x0000000a, 0x00040020, 0x0000000c,
    0x00000000, 0x0000000b, 0x0004003b, 0x0000000c, 0x0000000d, 0x00000000, 0x00040017, 0x0000000f,
    0x00000006, 0x00000002, 0x00040020, 0x00000010, 0x00000001, 0x0000000f, 0x0004003b, 0x00000010,
    0x00000011, 0x00000001, 0x00040020, 0x00000014, 0x00000001, 0x00000007, 0x0004003b, 0x00000014,
    0x00000015, 0x00000001, 0x0006001e, 0x00000019, 0x00000007, 0x00000007, 0x00000007, 0x00000007,
    0x00040020, 0x0000001a, 0x00000009, 0x00000019, 0x0004003b, 0x0000001a, 0x0000001b, 0x00000009,
    0x00040015, 0x0000001c, 0x00000020, 0x00000001, 0x0004002b, 0x0000001c, 0x0000001d, 0x00000003,
    0x00040015, 0x0000001e, 0x00000020, 0x00000000, 0x0004002b, 0x0000001e, 0x0000001f, 0x00000000,
    0x00040020, 0x00000020, 0x00000009, 0x00000006, 0x00040017, 0x00000023, 0x00000006, 0x00000003,
    0x00040020, 0x00000027, 0x00000007, 0x00000006, 0x0004002b, 0x0000001e, 0x0000002a, 0x00000001,
    0x0004002b, 0x0000001e, 0x0000002d, 0x00000002, 0x00040020, 0x00000030, 0x00000009, 0x00000007,
    0x00040020, 0x0000003d, 0x00000003, 0x00000007, 0x0004003b, 0x0000003d, 0x0000003e, 0x00000003,
    0x0004002b, 0x0000001e, 0x00000041, 0x00000003, 0x0004002b, 0x0000001c, 0x00000044, 0x00000001,
    0x00050036, 0x00000002, 0x00000004, 0x00000000, 0x00000003, 0x000200f8, 0x00000005, 0x0004003b,
    0x00000008, 0x00000009, 0x00000007, 0x0004003d, 0x0000000b, 0x0000000e, 0x0000000d, 0x0004003d,
    0x0000000f, 0x00000012, 0x00000011, 0x00050057, 0x00000007, 0x00000013, 0x0000000e, 0x00000012,
    0x0003003e, 0x00000009, 0x00000013, 0x0004003d, 0x00000007, 0x00000016, 0x00000015, 0x0004003d,
    0x00000007, 0x00000017, 0x00000009, 0x00050085, 0x00000007, 0x00000018, 0x00000017, 0x00000016,
    0x0003003e, 0x00000009, 0x00000018, 0x00060041, 0x00000020, 0x00000021, 0x0000001b, 0x0000001d,
    0x0000001f, 0x0004003d, 0x00000006, 0x00000022, 0x00000021, 0x0004003d, 0x00000007, 0x00000024,
    0x00000009, 0x0008004f, 0x00000023, 0x00000025, 0x00000024, 0x00000024, 0x00000000, 0x00000001,
    0x00000002, 0x0005008e, 0x00000023, 0x00000026, 0x00000025, 0x00000022, 0x00050041, 0x00000027,
    0x00000028, 0x00000009, 0x0000001f, 0x00050051, 0x00000006, 0x00000029, 0x00000026, 0x00000000,
    0x0003003e, 0x00000028, 0x00000029, 0x00050041, 0x00000027, 0x0000002b, 0x00000009, 0x0000002a,
    0x00050051, 0x00000006, 0x0000002c, 0x00000026, 0x00000001, 0x0003003e, 0x0000002b, 0x0000002c,
    0x00050041, 0x00000027, 0x0000002e, 0x00000009, 0x0000002d, 0x00050051, 0x00000006, 0x0000002f,
    0x00000026, 0x00000002, 0x0003003e, 0x0000002e, 0x0000002f, 0x00050041, 0x00000030, 0x00000031,
    0x0000001b, 0x0000001d, 0x0004003d, 0x00000007, 0x00000032, 0x00000031, 0x0008004f, 0x00000023,
    0x00000033, 0x00000032, 0x00000032, 0x00000001, 0x00000002, 0x00000003, 0x0004003d, 0x00000007,
    0x00000034, 0x00000009, 0x0008004f, 0x00000023, 0x00000035, 0x00000034, 0x00000034, 0x00000000,
    0x00000001, 0x00000002, 0x00050085, 0x00000023, 0x00000036, 0x00000035, 0x00000033, 0x00050041,
    0x00000027, 0x00000037, 0x00000009, 0x0000001f, 0x00050051, 0x00000006, 0x00000038, 0x00000036,
    0x00000000, 0x0003003e, 0x00000037, 0x00000038, 0x00050041, 0x00000027, 0x00000039, 0x00000009,
    0x0000002a, 0x00050051, 0x00000006, 0x0000003a, 0x00000036, 0x00000001, 0x0003003e, 0x00000039,
    0x0000003a, 0x00050041, 0x00000027, 0x0000003b, 0x00000009, 0x0000002d, 0x00050051, 0x00000006,
    0x0000003c, 0x00000036, 0x00000002, 0x0003003e, 0x0000003b, 0x0000003c, 0x0004003d, 0x00000007,
    0x0000003f, 0x00000009, 0x0008004f, 0x00000023, 0x00000040, 0x0000003f, 0x0000003f, 0x00000000,
    0x00000001, 0x00000002, 0x00050041, 0x00000027, 0x00000042, 0x00000009, 0x00000041, 0x0004003d,
    0x00000006, 0x00000043, 0x00000042, 0x00060041, 0x00000020, 0x00000045, 0x0000001b, 0x00000044,
    0x00000041, 0x0004003d, 0x00000006, 0x00000046, 0x00000045, 0x00050085, 0x00000006, 0x00000047,
    0x00000043, 0x00000046, 0x00050051, 0x00000006, 0x00000048, 0x00000040, 0x00000000, 0x00050051,
    0x00000006, 0x00000049, 0x00000040, 0x00000001, 0x00050051, 0x00000006, 0x0000004a, 0x00000040,
    0x00000002, 0x00070050, 0x00000007, 0x0000004b, 0x00000048, 0x00000049, 0x0000004a, 0x00000047,
    0x0003003e, 0x0000003e, 0x0000004b, 0x000100fd, 0x00010038,
];

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

/// S2's material pipeline vertex format: `a_Position` (vec3, z always 0)
/// plus `a_TexCoord` (vec2) — the attribute list every
/// `genericimage*`-family vertex shader declares with default combos.
/// Same 6-vertex layout and uv convention as `UNIT_QUAD`, just with the
/// extra `z = 0.0` component WE's own vertex shaders expect.
const MATERIAL_UNIT_QUAD: [f32; 30] = [
    -0.5, -0.5, 0.0, 0.0, 0.0, //
    0.5, -0.5, 0.0, 1.0, 0.0, //
    0.5, 0.5, 0.0, 1.0, 1.0, //
    0.5, 0.5, 0.0, 1.0, 1.0, //
    -0.5, 0.5, 0.0, 0.0, 1.0, //
    -0.5, -0.5, 0.0, 0.0, 0.0,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_timeout_is_the_only_process_fatal_upload_error() {
        assert!(is_fence_timeout(&RenderError::FenceTimeout));
        assert!(!is_fence_timeout(&RenderError::Vulkan(
            "out of memory".into()
        )));
    }

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

    /// S2 end-to-end: a synthetic material shader (no texture sampling —
    /// a fixed solid-color fragment shader, the same shape smoke-scene.sh's
    /// new case uses) preprocessed, compiled, registered, and bound to a
    /// layer, drawn fullscreen and read back. Confirms the whole material
    /// path — shaderpre -> shaderc -> the 8-sampler+UBO descriptor set ->
    /// the draw loop's per-frame UBO patch — produces the exact color the
    /// fragment shader hard-codes, independent of any texture. Skip-by-
    /// default like `isolated_draw`.
    #[test]
    fn material_pipeline_draws_a_synthetic_solid_color() {
        let Ok(binding) = std::env::var("KWE_TEST_DEVICE") else {
            eprintln!(
                "material_pipeline_draws_a_synthetic_solid_color: skipped (set KWE_TEST_DEVICE to run)"
            );
            return;
        };
        let mut renderer = LayerRenderer::new(Some(&binding), 64, 48).expect("create renderer");

        let vertex_source = "attribute vec3 a_Position;\nattribute vec2 a_TexCoord;\nuniform mat4 g_ModelViewProjectionMatrix;\nvarying vec2 v_TexCoord;\nvoid main() {\n    gl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix);\n    v_TexCoord = a_TexCoord;\n}\n";
        let fragment_source = "varying vec2 v_TexCoord;\nvoid main() {\n    gl_FragColor = vec4(0.2, 0.6, 0.8, 1.0) + 0.0 * vec4(v_TexCoord, 0.0, 0.0);\n}\n";
        let mut locations = std::collections::BTreeMap::new();
        let mut include: Box<crate::shaderpre::IncludeLookup<'static>> = Box::new(|_: &str| None);
        let vertex_pre = crate::shaderpre::preprocess(
            crate::shaderpre::Stage::Vertex,
            "synthetic.vert",
            vertex_source,
            &std::collections::BTreeMap::new(),
            &[],
            &mut locations,
            &mut include,
        )
        .expect("vertex preprocesses");
        let fragment_pre = crate::shaderpre::preprocess(
            crate::shaderpre::Stage::Fragment,
            "synthetic.frag",
            fragment_source,
            &std::collections::BTreeMap::new(),
            &[],
            &mut locations,
            &mut include,
        )
        .expect("fragment preprocesses");
        let vertex_spirv = crate::materialshader::compile_stage(
            &vertex_pre.source,
            crate::materialshader::Stage::Vertex,
            "synthetic.vert",
        )
        .expect("vertex compiles");
        let fragment_spirv = crate::materialshader::compile_stage(
            &fragment_pre.source,
            crate::materialshader::Stage::Fragment,
            "synthetic.frag",
        )
        .expect("fragment compiles");

        let key = MaterialKey::compute(
            "synthetic",
            &fragment_pre.combos,
            BlendMode::Normal.variant_index(),
        );
        renderer
            .register_material_pipeline(
                key.clone(),
                &vertex_spirv,
                &fragment_spirv,
                BlendMode::Normal,
            )
            .expect("register pipeline");
        renderer
            .bind_material_layer(
                0,
                key,
                &[None, None, None, None, None, None, None, None],
                MaterialUniforms::default(),
            )
            .expect("bind material layer");

        let draws = [LayerDraw {
            kind: DrawKind::Image,
            layer_index: 0,
            scene_order: 0,
            m: [[64.0, 0.0], [0.0, 48.0]],
            t: [0.0, 0.0],
            alpha: 1.0,
            blend_mode: BlendMode::Normal,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
            material: true,
        }];
        let pixels = renderer
            .render([0.0, 0.0, 0.0, 1.0], &draws)
            .expect("render once");
        assert_eq!(pixels.len(), 64 * 48 * 4);
        // B8G8R8A8 readback: B=0.8*255≈204, G=0.6*255≈153, R=0.2*255≈51.
        for pixel in pixels.chunks_exact(4) {
            assert!((203..=205).contains(&pixel[0]), "B={}", pixel[0]);
            assert!((152..=154).contains(&pixel[1]), "G={}", pixel[1]);
            assert!((50..=52).contains(&pixel[2]), "R={}", pixel[2]);
            assert_eq!(pixel[3], 255);
        }
    }

    /// S3 end-to-end: a synthetic effect pass renders a deterministic
    /// solid colour into a named FBO (`prepare_effect_targets` +
    /// `compile_effect_pass` + `render_effect_chains`), and a SECOND
    /// material — the layer's own, bound the ordinary
    /// `bind_material_layer` way — samples that FBO by name
    /// (`MaterialTextureBind::RenderTarget`) and draws it fullscreen.
    /// Proves the whole FBO chain end to end: target creation+clear,
    /// pass compilation against `effect_render_pass`, the per-frame
    /// replay, and `resolve_texture_slots`' `RenderTarget` lookup all
    /// work together on a real device. Skip-by-default: needs a Vulkan
    /// device — run with `KWE_TEST_DEVICE` set.
    #[test]
    fn effect_chain_renders_through_an_intermediate_fbo() {
        let Ok(binding) = std::env::var("KWE_TEST_DEVICE") else {
            eprintln!(
                "effect_chain_renders_through_an_intermediate_fbo: skipped (set KWE_TEST_DEVICE to run)"
            );
            return;
        };
        let mut renderer = LayerRenderer::new(Some(&binding), 32, 24).expect("create renderer");

        let vertex_source = "attribute vec3 a_Position;\nattribute vec2 a_TexCoord;\nuniform mat4 g_ModelViewProjectionMatrix;\nvarying vec2 v_TexCoord;\nvoid main() {\n    gl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix);\n    v_TexCoord = a_TexCoord;\n}\n";
        let solid_fragment_source = "varying vec2 v_TexCoord;\nvoid main() {\n    gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0) + 0.0 * vec4(v_TexCoord, 0.0, 0.0);\n}\n";
        let sample_fragment_source = "uniform sampler2D g_Texture0;\nvarying vec2 v_TexCoord;\nvoid main() {\n    gl_FragColor = texSample2D(g_Texture0, v_TexCoord);\n}\n";

        let compile = |source_vert: &str, source_frag: &str, label: &str| {
            let mut locations = std::collections::BTreeMap::new();
            let mut include: Box<crate::shaderpre::IncludeLookup<'static>> =
                Box::new(|_: &str| None);
            let vertex_pre = crate::shaderpre::preprocess(
                crate::shaderpre::Stage::Vertex,
                &format!("{label}.vert"),
                source_vert,
                &std::collections::BTreeMap::new(),
                &[],
                &mut locations,
                &mut include,
            )
            .expect("vertex preprocesses");
            let fragment_pre = crate::shaderpre::preprocess(
                crate::shaderpre::Stage::Fragment,
                &format!("{label}.frag"),
                source_frag,
                &std::collections::BTreeMap::new(),
                &[],
                &mut locations,
                &mut include,
            )
            .expect("fragment preprocesses");
            let vertex_spirv = crate::materialshader::compile_stage(
                &vertex_pre.source,
                crate::materialshader::Stage::Vertex,
                &format!("{label}.vert"),
            )
            .expect("vertex compiles");
            let fragment_spirv = crate::materialshader::compile_stage(
                &fragment_pre.source,
                crate::materialshader::Stage::Fragment,
                &format!("{label}.frag"),
            )
            .expect("fragment compiles");
            (vertex_spirv, fragment_spirv)
        };

        // 1. Create the target FBO and compile+bind a pass that draws a
        //    solid opaque red into it.
        let created = renderer
            .prepare_effect_targets(&[EffectTargetRequest {
                name: "_rt_TestTarget".to_string(),
                width: 8,
                height: 8,
            }])
            .expect("prepare effect targets");
        // The requested target plus the automatically-created
        // `_rt_FullFrameBuffer` (created whenever `requests` is
        // non-empty — at least one layer has a resolved effect chain).
        assert_eq!(created, 2, "the requested target plus _rt_FullFrameBuffer");

        let (solid_vertex, solid_fragment) = compile(vertex_source, solid_fragment_source, "solid");
        let binding_index = renderer
            .compile_effect_pass(
                &solid_vertex,
                &solid_fragment,
                BlendMode::Normal,
                "_rt_TestTarget",
                &[None, None, None, None, None, None, None, None],
                MaterialUniforms::default(),
            )
            .expect("compile effect pass");
        renderer.queue_effect_render(binding_index);

        // 2. Replay the effect chain once — this is what a real frame
        //    does before the main composite pass.
        renderer
            .render_effect_chains()
            .expect("render effect chains");

        // 3. Bind a normal layer material that samples "_rt_TestTarget"
        //    at texture slot 0, and draw it fullscreen.
        let (sample_vertex, sample_fragment) =
            compile(vertex_source, sample_fragment_source, "sample");
        let key = MaterialKey::compute(
            "sample",
            &std::collections::BTreeMap::new(),
            BlendMode::Normal.variant_index(),
        );
        renderer
            .register_material_pipeline(
                key.clone(),
                &sample_vertex,
                &sample_fragment,
                BlendMode::Normal,
            )
            .expect("register pipeline");
        renderer
            .bind_material_layer(
                0,
                key,
                &[
                    Some(MaterialTextureBind::RenderTarget(
                        "_rt_TestTarget".to_string(),
                    )),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                ],
                MaterialUniforms::default(),
            )
            .expect("bind material layer");

        let draws = [LayerDraw {
            kind: DrawKind::Image,
            layer_index: 0,
            scene_order: 0,
            m: [[32.0, 0.0], [0.0, 24.0]],
            t: [0.0, 0.0],
            alpha: 1.0,
            blend_mode: BlendMode::Normal,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
            material: true,
        }];
        let pixels = renderer
            .render([0.0, 0.0, 0.0, 1.0], &draws)
            .expect("render once");
        assert_eq!(pixels.len(), 32 * 24 * 4);
        // B8G8R8A8 readback of opaque red: B=0, G=0, R=255, A=255.
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(pixel, &[0, 0, 255, 255]);
        }
    }

    /// S3: `_rt_FullFrameBuffer` and every effect target are created
    /// cleared to transparent black — sampling one that has NEVER been
    /// rendered into (a chain that references an FBO before anything
    /// writes it, or a scene with an effect target no pass ever targets)
    /// must never crash and must sample a defined, transparent value.
    #[test]
    fn unwritten_effect_target_samples_transparent_black_not_garbage() {
        let Ok(binding) = std::env::var("KWE_TEST_DEVICE") else {
            eprintln!(
                "unwritten_effect_target_samples_transparent_black_not_garbage: skipped (set KWE_TEST_DEVICE to run)"
            );
            return;
        };
        let mut renderer = LayerRenderer::new(Some(&binding), 16, 16).expect("create renderer");
        renderer
            .prepare_effect_targets(&[EffectTargetRequest {
                name: "_rt_Untouched".to_string(),
                width: 4,
                height: 4,
            }])
            .expect("prepare effect targets");

        let vertex_source = "attribute vec3 a_Position;\nattribute vec2 a_TexCoord;\nuniform mat4 g_ModelViewProjectionMatrix;\nvarying vec2 v_TexCoord;\nvoid main() {\n    gl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix);\n    v_TexCoord = a_TexCoord;\n}\n";
        let sample_fragment_source = "uniform sampler2D g_Texture0;\nvarying vec2 v_TexCoord;\nvoid main() {\n    gl_FragColor = texSample2D(g_Texture0, v_TexCoord) + vec4(0.0, 0.0, 0.0, 1.0);\n}\n";
        let mut locations = std::collections::BTreeMap::new();
        let mut include: Box<crate::shaderpre::IncludeLookup<'static>> = Box::new(|_: &str| None);
        let vertex_pre = crate::shaderpre::preprocess(
            crate::shaderpre::Stage::Vertex,
            "untouched.vert",
            vertex_source,
            &std::collections::BTreeMap::new(),
            &[],
            &mut locations,
            &mut include,
        )
        .expect("vertex preprocesses");
        let fragment_pre = crate::shaderpre::preprocess(
            crate::shaderpre::Stage::Fragment,
            "untouched.frag",
            sample_fragment_source,
            &std::collections::BTreeMap::new(),
            &[],
            &mut locations,
            &mut include,
        )
        .expect("fragment preprocesses");
        let vertex_spirv = crate::materialshader::compile_stage(
            &vertex_pre.source,
            crate::materialshader::Stage::Vertex,
            "untouched.vert",
        )
        .expect("vertex compiles");
        let fragment_spirv = crate::materialshader::compile_stage(
            &fragment_pre.source,
            crate::materialshader::Stage::Fragment,
            "untouched.frag",
        )
        .expect("fragment compiles");
        let key = MaterialKey::compute(
            "untouched",
            &fragment_pre.combos,
            BlendMode::Normal.variant_index(),
        );
        renderer
            .register_material_pipeline(
                key.clone(),
                &vertex_spirv,
                &fragment_spirv,
                BlendMode::Normal,
            )
            .expect("register pipeline");
        renderer
            .bind_material_layer(
                0,
                key,
                &[
                    Some(MaterialTextureBind::RenderTarget(
                        "_rt_Untouched".to_string(),
                    )),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                ],
                MaterialUniforms::default(),
            )
            .expect("bind material layer");
        let draws = [LayerDraw {
            kind: DrawKind::Image,
            layer_index: 0,
            scene_order: 0,
            m: [[16.0, 0.0], [0.0, 16.0]],
            t: [0.0, 0.0],
            alpha: 1.0,
            blend_mode: BlendMode::Normal,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
            material: true,
        }];
        // Never crashes/panics/hangs — the point of the test.
        let pixels = renderer
            .render([0.0, 0.0, 0.0, 1.0], &draws)
            .expect("render once");
        assert_eq!(pixels.len(), 16 * 16 * 4);
        // The shader adds opaque alpha unconditionally: any sampled RGB
        // (transparent black's RGB channels are 0) composited straight
        // through with no blending contribution from the (all-zero)
        // clear color underneath is black, alpha 255.
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(pixel, &[0, 0, 0, 255]);
        }
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
            kind: DrawKind::Image,
            layer_index: 0,
            scene_order: 0,
            m: [[64.0, 0.0], [0.0, 48.0]],
            t: [0.0, 0.0],
            alpha: 1.0,
            blend_mode: BlendMode::Normal,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
            material: false,
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
            kind: DrawKind::Image,
            layer_index: 0,
            scene_order: 0,
            m: [[64.0, 0.0], [0.0, 48.0]],
            t: [0.0, 0.0],
            alpha: 1.0,
            blend_mode: BlendMode::Normal,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
            material: false,
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

    /// The M3e atlas-rebuild leak regression: re-uploading the same layer
    /// index must replace in place, never accumulate. Without the
    /// destroy-before-overwrite fix, every rebuild allocated a NEW
    /// descriptor set from the bounded pool (MAX_LAYERS sets) plus a new
    /// image — after MAX_LAYERS re-uploads the pool was exhausted and
    /// every later upload failed, silently killing text layers (a
    /// ~240-image-layer scene left 16 sets and died on the first rebuild).
    /// With the fix all re-uploads succeed, the pool never grows, and the
    /// drop-accounting counter stays at the live entry count.
    #[test]
    fn texture_reuploads_replace_in_place_without_exhausting_the_pool() {
        let Ok(binding) = std::env::var("KWE_TEST_DEVICE") else {
            eprintln!("texture_reuploads_replace_in_place: skipped (set KWE_TEST_DEVICE to run)");
            return;
        };
        let mut renderer = LayerRenderer::new(Some(&binding), 64, 48).expect("create renderer");
        let rgba = [255u8, 0, 0, 255];
        // 320 re-uploads of one index — more than the 256-set pool, so a
        // leaked set per rebuild would exhaust the pool mid-loop and the
        // expect() below would fail.
        for _ in 0..MAX_LAYERS + 64 {
            renderer
                .upload_layer(0, &rgba, 1, 1)
                .expect("re-upload must not exhaust the descriptor pool");
        }
        assert_eq!(
            renderer.live_uploads, 1,
            "re-uploads replace in place, never accumulate"
        );
        // Fresh indices still allocate: the freed sets were not hoarded.
        renderer
            .upload_layer(7, &rgba, 1, 1)
            .expect("fresh index allocates");
        assert_eq!(renderer.live_uploads, 2);
    }

    /// M3g: the per-frame video path. `refresh_layer` must update the same
    /// image, view, and descriptor set in place — a video at 30 fps runs
    /// this once per frame, and creating a set per frame would exhaust the
    /// bounded pool in nine seconds. A dimension change is the one case
    /// that falls back to a full `upload_layer`.
    #[test]
    fn refresh_layer_updates_in_place_and_reallocates_only_on_resize() {
        let Ok(binding) = std::env::var("KWE_TEST_DEVICE") else {
            eprintln!("refresh_layer_updates_in_place: skipped (set KWE_TEST_DEVICE to run)");
            return;
        };
        let mut renderer = LayerRenderer::new(Some(&binding), 8, 8).expect("create renderer");
        // A 2×2 red frame establishes the slot the normal way.
        let red = [255u8, 0, 0, 255].repeat(4);
        renderer.upload_layer(0, &red, 2, 2).expect("first frame");
        assert_eq!(renderer.live_uploads, 1);

        // Byte-count validation happens before any GPU work.
        assert!(
            renderer.refresh_layer(0, &red[..8], 2, 2).is_err(),
            "a short frame must be refused, not copied"
        );

        // 320 same-size refreshes: more than the 256-set pool, so a set
        // allocated per refresh would exhaust it mid-loop.
        let blue = [0u8, 0, 255, 255].repeat(4);
        for _ in 0..MAX_LAYERS + 64 {
            renderer
                .refresh_layer(0, &blue, 2, 2)
                .expect("refresh must not exhaust the descriptor pool");
        }
        assert_eq!(
            renderer.live_uploads, 1,
            "in-place refreshes never allocate a new slot"
        );

        // The refreshed content is what actually draws: a fullscreen quad
        // of the blue frame reads back blue, not the red first frame.
        let draws = [LayerDraw {
            kind: DrawKind::Image,
            layer_index: 0,
            scene_order: 0,
            m: [[8.0, 0.0], [0.0, 8.0]],
            t: [0.0, 0.0],
            alpha: 1.0,
            blend_mode: BlendMode::Normal,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
            material: false,
        }];
        let pixels = renderer
            .render([0.0, 0.0, 0.0, 0.0], &draws)
            .expect("render the refreshed frame");
        // The B8G8R8A8 readback is B,G,R,A in memory order, so the blue
        // texel reads back [255, 0, 0, 255] — the red first frame would
        // read back [0, 0, 255, 255].
        assert_eq!(
            &pixels[0..4],
            &[255, 0, 0, 255],
            "the refresh, not the first upload, is on screen"
        );

        // A dimension change cannot update in place: it falls back to the
        // full upload path, which replaces the slot rather than leaking it.
        let green = [0u8, 255, 0, 255].repeat(16);
        renderer
            .refresh_layer(0, &green, 4, 4)
            .expect("resize falls back to upload_layer");
        assert_eq!(
            renderer.live_uploads, 1,
            "the resize replaced the slot in place"
        );
        // Refreshing a slot that was never uploaded also falls back, and
        // the fallback is what allocates.
        renderer
            .refresh_layer(3, &green, 4, 4)
            .expect("empty slot falls back to upload_layer");
        assert_eq!(renderer.live_uploads, 2);
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
            kind: DrawKind::Image,
            layer_index: 0,
            scene_order: 0,
            m: [[64.0, 0.0], [0.0, 48.0]],
            t: [0.0, 0.0],
            alpha: 191.0 / 255.0,
            blend_mode: BlendMode::Normal,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
            material: false,
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
                kind: DrawKind::Image,
                layer_index: 0,
                scene_order: 0,
                m: [[64.0, 0.0], [0.0, 48.0]],
                t: [0.0, 0.0],
                alpha: 1.0,
                blend_mode: mode,
                brightness: 1.0,
                tint: [1.0, 1.0, 1.0, 1.0],
                material: false,
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
            kind: DrawKind::Image,
            layer_index: 0,
            scene_order: 0,
            m: [[64.0, 0.0], [0.0, 48.0]],
            t: [0.0, 0.0],
            alpha: 1.0,
            blend_mode: BlendMode::Normal,
            brightness: 2.0,
            tint: [1.0, 0.25, 0.5, 1.0],
            material: false,
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
            kind: DrawKind::Image,
            layer_index: 0,
            scene_order: 0,
            m: [[64.0, 0.0], [0.0, 48.0]],
            t: [0.0, 0.0],
            alpha: 0.5,
            blend_mode: BlendMode::Multiply,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
            material: false,
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
