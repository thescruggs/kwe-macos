#version 450
// M3f particle compositor: one batched draw per particle system. The CPU
// simulation (particles.rs) writes 6 vertices per particle — a quad in
// scene pixel units (y down) whose corners are already expanded around the
// particle center and size — plus the per-particle color and size as vertex
// attributes. The shader transforms the corners exactly like quad.vert: the
// particle draw pushes the identity model (particle positions are scene
// coordinates baked by the CPU), so the same push-constant layout serves
// both shaders.
// Original KWE shader (SPDX: Apache-2.0). Generated with:
//   glslangValidator -V --target-env vulkan1.2 -o particle.vert.spv particle.vert
// Vertex input (binding 0, stride 40): pos.xy, uv.xy, color.rgba, size.
layout(location = 0) in vec2 aPos;
layout(location = 1) in vec2 aUV;
layout(location = 2) in vec4 aColor;
layout(location = 3) in float aSize;

layout(push_constant) uniform PC {
    vec4 m0;
    vec4 m1;
    vec4 viewport;
} pc;

layout(location = 0) out vec2 vUV;
layout(location = 1) out vec4 vColor;

void main() {
    vec2 world = mat2(pc.m0.xy, pc.m1.xy) * aPos + vec2(pc.m0.z, pc.m1.z);
    vec2 ndc = (world * 2.0) / pc.viewport.xy;
    gl_Position = vec4(ndc, 0.0, 1.0);
    vUV = aUV;
    vColor = aColor;
}
