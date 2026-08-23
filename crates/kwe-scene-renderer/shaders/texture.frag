#version 450
// M3c+M3d layer compositor: sample the layer's texture (combined image
// sampler, linear clamp), apply the M3d color effects (brightness × tint
// on the sampled RGB, alpha scaled by the effective layer alpha — the
// layer alpha folded with the tint alpha, pushed in m1.w), and output
// straight color. The pipeline's blend state (the layer's blend-mode
// variant) combines the result with the frame; the shader never
// premultiplies. Original KWE shader (SPDX: GPL-3.0-or-later). Generated with:
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
    outColor = vec4(c.rgb, c.a * pc.m1.w); // alpha = texel · layer alpha · tint alpha
}
