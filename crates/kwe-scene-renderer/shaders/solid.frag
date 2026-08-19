#version 450
// M3a minimal publish path: sample the push-constant clear color at every
// pixel (the "sample" in the clear-color sample pipeline). Original KWE
// shader (SPDX: Apache-2.0). Generated with:
//   glslangValidator -V --target-env vulkan1.2 -o solid.frag.spv solid.frag
layout(location = 0) out vec4 out_color;
layout(push_constant) uniform PushColor { vec4 color; } pc;
void main() {
    out_color = pc.color;
}
