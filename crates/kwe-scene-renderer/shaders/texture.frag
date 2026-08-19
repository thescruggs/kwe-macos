#version 450
// M3c layer compositor: sample the layer's texture (combined image sampler,
// linear clamp) and output straight alpha scaled by the layer alpha from the
// push constants. The pipeline's src-over blend combines the result with the
// frame; the shader never premultiplies. Original KWE shader (SPDX:
// Apache-2.0). Generated with:
//   glslangValidator -V --target-env vulkan1.2 -o texture.frag.spv texture.frag
layout(set = 0, binding = 0) uniform sampler2D tex;

layout(push_constant) uniform PC {
    vec4 m0;
    vec4 m1;
    vec4 viewport;
} pc;

layout(location = 0) in vec2 vUV;
layout(location = 0) out vec4 outColor;

void main() {
    vec4 c = texture(tex, vUV);
    outColor = vec4(c.rgb, c.a * pc.m1.w);
}
