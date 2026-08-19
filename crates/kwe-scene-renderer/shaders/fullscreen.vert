#version 450
// M3a minimal publish path: a fullscreen triangle covering the viewport.
// Original KWE shader (SPDX: Apache-2.0). Generated with:
//   glslangValidator -V --target-env vulkan1.2 -o fullscreen.vert.spv fullscreen.vert
layout(location = 0) out vec2 v_uv;
void main() {
    vec2 pos[3] = vec2[3](vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    gl_Position = vec4(pos[gl_VertexIndex], 0.0, 1.0);
    v_uv = pos[gl_VertexIndex] * 0.5 + 0.5;
}
