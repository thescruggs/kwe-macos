#version 450
// M3f particle compositor: sample the particle system's texture (combined
// image sampler, linear clamp), multiply by the per-particle color carried
// in the vertex attributes (instance factors like colorn/alpha are folded
// into it on the CPU), then apply the M3d color effects like texture.frag —
// brightness × tint on the sampled RGB, alpha scaled by the pushed layer
// alpha (1.0 for particle draws). S7b (B1): PREMULTIPLIED output — matches
// the S6 material-fragment convention and the pipeline's blend state
// (`vulkan.rs::blend_attachment_for`), which is written for premultiplied
// input: `Normal` (ONE, ONE_MINUS_SRC_ALPHA) is then true src-over, and
// `Add` (ONE, ONE) is exactly upstream's additive blend
// (`CPass.cpp:134-136` glBlendFuncSeparate(SRC_ALPHA, ONE) applied to
// straight color ≡ (ONE, ONE) applied to premultiplied color). Straight
// output under a premultiplied blend table made a fading additive particle
// never fade (its RGB added at full strength regardless of alpha) and made
// translucent particles too bright.
// Original KWE shader (SPDX: GPL-3.0-or-later). Generated with:
//   glslangValidator -V --target-env vulkan1.2 -o particle.frag.spv particle.frag
layout(set = 0, binding = 0) uniform sampler2D tex;

layout(push_constant) uniform PC {
    vec4 m0;
    vec4 m1;
    vec4 viewport;
    vec4 effects; // (brightness, tint.r, tint.g, tint.b) — M3d
} pc;

layout(location = 0) in vec2 vUV;
layout(location = 1) in vec4 vColor;
layout(location = 0) out vec4 outColor;

void main() {
    vec4 c = texture(tex, vUV);
    c *= vColor;                // per-particle color (colorStart..colorEnd)
    c.rgb *= pc.effects.x;      // brightness (0..=10, default 1)
    c.rgb *= pc.effects.yzw;    // tint RGB (0..=1, default 1)
    float a = c.a * pc.m1.w;    // alpha = texel · color.a · layer alpha
    outColor = vec4(c.rgb * a, a); // premultiplied output (S7b/B1)
}
