#version 450
// M3c+M3d layer compositor: sample the layer's texture (combined image
// sampler, linear clamp), apply the M3d color effects (brightness × tint
// on the sampled RGB, alpha scaled by the effective layer alpha — the
// layer alpha folded with the tint alpha, pushed in m1.w). S7b (B1):
// PREMULTIPLIED output — matches the S6 material-fragment convention and
// the pipeline's blend state (`vulkan.rs::blend_attachment_for`), which is
// written for premultiplied input: `Normal` (ONE, ONE_MINUS_SRC_ALPHA) is
// then true src-over, and `Add` (ONE, ONE) is exactly upstream's additive
// blend (`CPass.cpp:134-136` glBlendFuncSeparate(SRC_ALPHA, ONE) applied to
// straight color ≡ (ONE, ONE) applied to premultiplied color).
// Original KWE shader (SPDX: GPL-3.0-or-later). Generated with:
//   glslangValidator -V --target-env vulkan1.2 -o texture.frag.spv texture.frag
layout(set = 0, binding = 0) uniform sampler2D tex;

layout(push_constant) uniform PC {
    vec4 m0;
    vec4 m1;
    vec4 viewport;
    vec4 effects; // (brightness, tint.r, tint.g, tint.b) — M3d
} pc;

layout(location = 0) in vec2 vUV;
layout(location = 0) out vec4 outColor;

void main() {
    vec4 c = texture(tex, vUV);
    // M3d: the color effects apply to the sampled texel BEFORE blending —
    // the blend mode lives in the pipeline's blend state, not here.
    c.rgb *= pc.effects.x;      // brightness (0..=10, default 1)
    c.rgb *= pc.effects.yzw;    // tint RGB (0..=1, default 1)
    float a = c.a * pc.m1.w;    // alpha = texel · layer alpha · tint alpha
    outColor = vec4(c.rgb * a, a); // premultiplied output (S7b/B1)
}
