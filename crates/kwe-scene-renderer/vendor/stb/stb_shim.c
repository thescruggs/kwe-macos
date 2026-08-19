// SPDX-License-Identifier: Apache-2.0
// M3e text-layer rasterizer shim: a tiny opaque wrapper around the vendored
// stb_truetype.h (public domain; pinned revision 6e9f34d5 — see
// THIRD_PARTY.yml). The Rust side (src/text.rs) never depends on the C
// header's struct layout: every call goes through kwe_font* functions over
// an opaque `kwe_font` handle the shim sizes for the caller.
//
// The header's STBTT_STATIC keeps all stb symbols private to this
// translation unit; NDEBUG disables the rasterizer's asserts (a hostile
// font must never be able to abort the worker through a checked
// STBTT_assert). The single deliberate bound beyond stb's own: glyph
// outlines with more than KWE_MAX_GLYPH_VERTS vertices are refused instead
// of flattened (bounded everything — the vertex count scales with the font
// file, which the resolver caps at 64 MiB).
#define NDEBUG
#define STBTT_STATIC
#define STB_TRUETYPE_IMPLEMENTATION
#include "stb_truetype.h"

#ifndef KWE_MAX_GLYPH_VERTS
#define KWE_MAX_GLYPH_VERTS 65536
#endif

typedef struct {
    stbtt_fontinfo info;
} kwe_font;

// The number of bytes a kwe_font occupies (Rust allocates an aligned
// buffer of exactly this size).
size_t kwe_font_size(void) {
    return sizeof(kwe_font);
}

// stbtt_InitFont, with one convenience: a .ttc collection is opened at its
// first font offset instead of being rejected at byte 0. stb_truetype has
// no IsCollection helper — a collection is detected by its "ttcf" tag and
// stbtt_GetFontOffsetForIndex resolves the offset (0 is a valid offset, so
// only negative results mean "not a font").
//
// The buffer length bounds the shim's own reads; stb_truetype itself has
// no length parameter, so anything below a full sfnt header (tag + count,
// 12 bytes) is rejected before it can be touched. Callers feed this
// function either real font files read with bounds or short garbage
// buffers; a 12+ byte buffer with an unknown tag is rejected by
// stbtt_InitFont's stbtt__isfont tag check before any table walk.
int kwe_font_init(kwe_font *font, const unsigned char *data, size_t data_len,
                  int fontstart) {
    if (font == NULL || data == NULL || data_len < 12) {
        return 0;
    }
    int is_collection = data_len >= 4 && data[0] == 't' && data[1] == 't' &&
                        data[2] == 'c' && data[3] == 'f';
    if (is_collection) {
        int offset = stbtt_GetFontOffsetForIndex(data, 0);
        if (offset < 0) {
            return 0;
        }
        fontstart = offset;
    }
    return stbtt_InitFont(&font->info, data, fontstart);
}

int kwe_font_glyph_index(const kwe_font *font, int codepoint) {
    return stbtt_FindGlyphIndex(&font->info, codepoint);
}

void kwe_font_glyph_h_metrics(const kwe_font *font, int glyph, int *advance,
                              int *left_side_bearing) {
    stbtt_GetGlyphHMetrics(&font->info, glyph, advance, left_side_bearing);
}

float kwe_font_scale_for_pixel_height(const kwe_font *font, float height) {
    return stbtt_ScaleForPixelHeight(&font->info, height);
}

// The glyph's pixel-space box (bitmap coordinates, y down) at scale
// (scale_x, scale_y) with no subpixel shift. A space glyph reports a
// zero-sized box.
void kwe_font_glyph_bitmap_box(const kwe_font *font, int glyph, float scale_x,
                               float scale_y, int *ix0, int *iy0, int *ix1,
                               int *iy1) {
    stbtt_GetGlyphBitmapBoxSubpixel(&font->info, glyph, scale_x, scale_y, 0.0f,
                                    0.0f, ix0, iy0, ix1, iy1);
}

// Rasterize one glyph into the caller's buffer. The buffer is (out_w x
// out_h) with the given row stride and covers the bitmap region that starts
// at (ix0, iy0) — i.e. bitmap pixel (ix0 + bx, iy0 + by) lands at out[bx +
// by * stride]. Returns 1 when the glyph rendered, 0 when it was refused
// (oversized outline) or empty. `out` may be NULL when both dimensions are
// zero.
int kwe_font_render_glyph(const kwe_font *font, unsigned char *out, int out_w,
                          int out_h, int out_stride, float scale_x,
                          float scale_y, int glyph, int ix0, int iy0) {
    stbtt_vertex *vertices = NULL;
    int num_verts = stbtt_GetGlyphShape(&font->info, glyph, &vertices);
    if (num_verts > KWE_MAX_GLYPH_VERTS) {
        stbtt_FreeShape(&font->info, vertices);
        return 0;
    }
    if (out_w > 0 && out_h > 0) {
        stbtt__bitmap bitmap;
        bitmap.pixels = out;
        bitmap.w = out_w;
        bitmap.h = out_h;
        bitmap.stride = out_stride;
        stbtt_Rasterize(&bitmap, 0.35f, vertices, num_verts, scale_x, scale_y,
                        0.0f, 0.0f, ix0, iy0, 1, font->info.userdata);
    }
    stbtt_FreeShape(&font->info, vertices);
    return 1;
}

// The font's family name (Windows English name record, Mac Roman and raw
// Unicode as fallbacks — the records real fonts carry), copied into `buf`
// null-terminated and bounded by `buflen`. Returns the copied length, or 0
// when the font has no name table / matching record.
//
// stb returns the name record's RAW bytes: Windows (platform 3) and raw
// Unicode (platform 0 encoding 3) records are UTF-16BE (the ASCII family
// names arrive as "N\0o\0t\0o\0..." — copying them verbatim would make
// every family unreadable), while Mac (platform 1) records are single-byte
// (Mac Roman). The UTF-16 records are decoded to UTF-8 here, bounded and
// lossy (family names are short ASCII; surrogate halves are skipped, and
// the buffer never overflows).
int kwe_font_family_name(const kwe_font *font, char *buf, size_t buflen) {
    if (buf == NULL || buflen == 0) {
        return 0;
    }
    const char *name = NULL;
    int length = 0;
    int single_byte = 0;
    name = stbtt_GetFontNameString(&font->info, &length, 3, 1, 0x409, 1);
    if (name == NULL) {
        name = stbtt_GetFontNameString(&font->info, &length, 3, 10, 0x409, 1);
    }
    if (name == NULL) {
        name = stbtt_GetFontNameString(&font->info, &length, 1, 0, 0, 1);
        single_byte = 1;
    }
    if (name == NULL) {
        name = stbtt_GetFontNameString(&font->info, &length, 0, 3, 0, 1);
    }
    if (name == NULL || length <= 0) {
        return 0;
    }
    size_t out = 0;
    if (single_byte) {
        size_t copy = (size_t)length < buflen - 1 ? (size_t)length : buflen - 1;
        memcpy(buf, name, copy);
        out = copy;
    } else {
        for (int i = 0; i + 1 < length && out + 3 < buflen; i += 2) {
            unsigned int cp = ((unsigned char)name[i] << 8) | (unsigned char)name[i + 1];
            if (cp >= 0xD800 && cp <= 0xDFFF) {
                continue;  // surrogate halves are not a family name
            }
            if (cp < 0x80) {
                buf[out++] = (char)cp;
            } else if (cp < 0x800) {
                buf[out++] = (char)(0xC0 | (cp >> 6));
                buf[out++] = (char)(0x80 | (cp & 0x3F));
            } else {
                buf[out++] = (char)(0xE0 | (cp >> 12));
                buf[out++] = (char)(0x80 | ((cp >> 6) & 0x3F));
                buf[out++] = (char)(0x80 | (cp & 0x3F));
            }
        }
    }
    buf[out] = '\0';
    return (int)out;
}
