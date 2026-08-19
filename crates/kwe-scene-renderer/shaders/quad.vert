#version 450
// M3c layer compositor: the unit quad (one vertex buffer of 4 × (pos, uv)).
// Original KWE shader (SPDX: Apache-2.0). Generated with:
//   glslangValidator -V --target-env vulkan1.2 -o quad.vert.spv quad.vert
// Per-layer push constants carry the model matrix:
//   m0 = (a, c, tx, 0)      column 0 of the linear part + translate x
//   m1 = (b, d, ty, alpha)  column 1 + translate y (+ the layer alpha for
//                            the fragment shader, which shares the block)
//   viewport = (w, h, 0, 0) scene size in pixels
// world = mat2(m0.xy, m1.xy) * pos + vec2(m0.z, m1.z) — the layer model
// computed in Rust (layers.rs): R(theta)·S(scale)·diag(size)·pos + origin,
// pos in [-0.5, 0.5]^2, scene units with +y down. NDC inverts y.
layout(location = 0) in vec2 aPos;
layout(location = 1) in vec2 aUV;

layout(push_constant) uniform PC {
    vec4 m0;
    vec4 m1;
    vec4 viewport;
} pc;

layout(location = 0) out vec2 vUV;

void main() {
    vec2 world = mat2(pc.m0.xy, pc.m1.xy) * aPos + vec2(pc.m0.z, pc.m1.z);
    vec2 ndc = (world * 2.0) / pc.viewport.xy;
    // No y-flip: scene y grows down, NDC y grows up, so scene y=0 maps to
    // NDC y=-1 (the framebuffer's bottom). Color attachments stored with
    // VK_IMAGE_TILING_OPTIMAL come back bottom-first in the readback (the
    // protocol's row 0 is the scene's top row), so rendering the scene
    // bottom-first on the framebuffer delivers it upright. This was verified
    // with a shader that skipped the transform (the readback mirrored it);
    // the M3a fullscreen clear was orientation-invariant and never exposed it.
    gl_Position = vec4(ndc, 0.0, 1.0);
    vUV = aUV;
}
