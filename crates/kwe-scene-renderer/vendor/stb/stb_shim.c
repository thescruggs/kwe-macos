// SPDX-License-Identifier: Apache-2.0
// M3e text-layer rasterizer shim: a tiny opaque wrapper around the vendored
// stb_truetype.h (public domain OR MIT; pinned revision 6e9f34d5 — see
// THIRD_PARTY.yml). The Rust side (src/text.rs) never depends on the C
// header's struct layout: every call goes through kwe_font* functions over
// an opaque `kwe_font` handle the shim sizes for the caller.
//
// The header's STBTT_STATIC keeps all stb symbols private to this
// translation unit; NDEBUG disables the rasterizer's asserts (a hostile
// font must never be able to abort the worker through a checked
// STBTT_assert).
//
// stb_truetype explicitly does NO range checking of the offsets found in
// the file ("NO SECURITY GUARANTEE -- DO NOT USE THIS ON UNTRUSTED FONT
// FILES" in the upstream header), so the shim validates the sfnt structure
// BEFORE any stb call: the offset table, every table record's
// (offset, length) inside the buffer, a .ttc collection's embedded
// offsets, and — where glyf outlines exist — the loca table against
// maxp/head with every glyph range inside glyf. stb can then only walk
// data the validation has bounded; a hostile font is refused at open, it
// never reaches stb.
#define NDEBUG
#define STBTT_STATIC
#define STB_TRUETYPE_IMPLEMENTATION
#include "stb_truetype.h"

#include <string.h>

#ifndef KWE_MAX_GLYPH_VERTS
#define KWE_MAX_GLYPH_VERTS 65536
#endif

typedef struct {
    stbtt_fontinfo info;
    // Validated bounds of the glyf/loca tables (both 0 and loca_valid=0
    // when the font has no glyf outlines — CFF/bitmap-only fonts, which
    // stb cannot rasterize anyway). Set by kwe_font_init from the table
    // records, never from stb's own pointers: stb stores offsets only,
    // not lengths, so the lengths live here.
    size_t glyf_off, glyf_len;
    size_t loca_off, loca_len;
    int loca_valid;
} kwe_font;

// The number of bytes a kwe_font occupies (Rust allocates an aligned
// buffer of exactly this size).
size_t kwe_font_size(void) {
    return sizeof(kwe_font);
}

// sfnt numbers are big-endian.
static unsigned int kwe_u32(const unsigned char *p) {
    return ((unsigned int)p[0] << 24) | ((unsigned int)p[1] << 16) |
           ((unsigned int)p[2] << 8) | (unsigned int)p[3];
}
static unsigned int kwe_u16(const unsigned char *p) {
    return ((unsigned int)p[0] << 8) | (unsigned int)p[1];
}
static int kwe_i16(const unsigned char *p) {
    unsigned int v = kwe_u16(p);
    return (int)(v >= 0x8000u ? v - 0x10000u : v);
}

// Tags of the tables the rasterizer trusts.
#define KWE_TAG_GLYF 0x676C7966u
#define KWE_TAG_LOCA 0x6C6F6361u
#define KWE_TAG_MAXP 0x6D617870u
#define KWE_TAG_HEAD 0x68656164u
#define KWE_TAG_HMTX 0x686D7478u

// Validate one sfnt at `start` (an offset table plus all its table
// records) against `data_len`, and when `font` is non-NULL record the
// validated glyf/loca/hmtx bounds for the render-time per-glyph check.
// Returns 0 when anything lies outside the buffer or the outline tables
// are inconsistent — the font is then refused entirely.
static int kwe_sfnt_tables_ok(const unsigned char *data, size_t data_len,
                              size_t start, kwe_font *font) {
    if (start >= data_len || data_len - start < 12) {
        return 0;
    }
    const unsigned char *ot = data + start;
    unsigned int tag = kwe_u32(ot);
    if (tag != 0x00010000u && tag != 0x4F54544Fu /* OTTO */ &&
        tag != 0x74727565u /* true */ && tag != 0x74797031u /* typ1 */) {
        return 0;
    }
    unsigned int num_tables = kwe_u16(ot + 4);
    // 12 + numTables*16 must fit inside the buffer (checked in size_t
    // math; numTables is at most 65535).
    if ((size_t)num_tables > (data_len - (start + 12)) / 16) {
        return 0;
    }
    size_t glyf_off = 0, glyf_len = 0, loca_off = 0, loca_len = 0;
    size_t maxp_off = 0, maxp_len = 0, head_off = 0, head_len = 0;
    size_t hmtx_off = 0, hmtx_len = 0;
    for (unsigned int i = 0; i < num_tables; i++) {
        const unsigned char *rec = ot + 12 + (size_t)i * 16;
        unsigned int off = kwe_u32(rec + 8);
        unsigned int len = kwe_u32(rec + 12);
        size_t end = (size_t)off + (size_t)len;
        if (end > data_len) {
            return 0;  // (offset + length) lies outside the buffer
        }
        switch (kwe_u32(rec)) {
            case KWE_TAG_GLYF: glyf_off = off; glyf_len = len; break;
            case KWE_TAG_LOCA: loca_off = off; loca_len = len; break;
            case KWE_TAG_MAXP: maxp_off = off; maxp_len = len; break;
            case KWE_TAG_HEAD: head_off = off; head_len = len; break;
            case KWE_TAG_HMTX: hmtx_off = off; hmtx_len = len; break;
        }
    }
    if (glyf_off) {
        // The outline tables exist: loca must be provably safe before
        // GetGlyphShape is ever allowed to trust it. A font with glyf but
        // without the full head/maxp/loca set is malformed and refused (it
        // cannot be validated cheaply); hmtx must cover every glyph's
        // metric (stb reads hmtx[glyph*4] without length checks).
        if (!loca_off || !maxp_off || !head_off || !hmtx_off ||
            maxp_len < 6 || head_len < 52) {
            return 0;
        }
        unsigned int num_glyphs = kwe_u16(data + maxp_off + 4);
        if (hmtx_len < (size_t)num_glyphs * 4) {
            return 0;
        }
        size_t entry = kwe_i16(data + head_off + 50) == 0 ? 2u : 4u;
        if ((size_t)(num_glyphs + 1) > loca_len / entry) {
            return 0;  // loca cannot hold one entry per glyph
        }
        // Monotone loca, every glyph range inside glyf (bounded work: at
        // most 65536 u16/u32 reads).
        size_t prev = 0;
        for (unsigned int g = 0; g <= num_glyphs; g++) {
            size_t off = entry == 2u
                             ? (size_t)kwe_u16(data + loca_off + (size_t)g * 2) * 2u
                             : (size_t)kwe_u32(data + loca_off + (size_t)g * 4);
            if (off < prev || off > glyf_len) {
                return 0;
            }
            prev = off;
        }
        if (font != NULL) {
            font->glyf_off = glyf_off;
            font->glyf_len = glyf_len;
            font->loca_off = loca_off;
            font->loca_len = loca_len;
            font->loca_valid = 1;
        }
    }
    return 1;
}

// Recompute the glyph's outline range from the validated loca table and
// check the outline header fits the range, so stbtt_GetGlyphShape (which
// trusts the font's claims and allocates) can only walk data inside the
// glyf table. Returns 1 when the glyph is safe to hand to stb (or has no
// outline — whitespace, CFF/bitmap fonts), 0 when the claims are
// inconsistent (the glyph is then rendered as nothing).
static int kwe_glyph_outline_ok(const kwe_font *font, int glyph) {
    if (!font->loca_valid) {
        return 1;  // no glyf outlines: stb reads none
    }
    if (glyph < 0 || glyph >= font->info.numGlyphs) {
        return 0;
    }
    size_t entry = font->info.indexToLocFormat == 0 ? 2u : 4u;
    if ((size_t)(glyph + 2) > font->loca_len / entry) {
        return 0;
    }
    const unsigned char *loca = font->info.data + font->loca_off;
    size_t start = entry == 2u
                       ? (size_t)kwe_u16(loca + (size_t)glyph * 2) * 2u
                       : (size_t)kwe_u32(loca + (size_t)glyph * 4);
    size_t end = entry == 2u
                     ? (size_t)kwe_u16(loca + (size_t)(glyph + 1) * 2) * 2u
                     : (size_t)kwe_u32(loca + (size_t)(glyph + 1) * 4);
    if (start > end || end > font->glyf_len) {
        return 0;
    }
    if (start == end) {
        return 1;  // empty glyph (whitespace)
    }
    const unsigned char *glyf = font->info.data + font->glyf_off;
    size_t range = end - start;
    int contours = kwe_i16(glyf + start);
    if (contours >= 0) {
        // Simple glyph: 10-byte header plus 2*contours end-point bytes
        // must fit the range — the point walk stb performs is then
        // bounded by the contour claim, which stays inside the glyph's
        // own range.
        if (range < 10u + 2u * (size_t)contours) {
            return 0;
        }
    } else {
        // Composite glyph: walk the component list — each component is
        // flags + glyphIndex + args (+ optional transforms) — and require
        // every component to end inside the glyph's range. stb's internal
        // recursion is capped at depth 10, so this bounds the vertex
        // allocation and the walk.
        size_t p = start + 10;
        for (;;) {
            if (p + 4 > start + range) {
                return 0;
            }
            unsigned int flags = kwe_u16(glyf + p);
            p += 4;
            if (flags & 0x0001u) {
                p += 4;  // ARG_1_AND_2_ARE_WORDS
            } else {
                p += 2;
            }
            if (flags & 0x0008u) {
                p += 2;  // WE_HAVE_A_SCALE
            } else if (flags & 0x0040u) {
                p += 4;  // WE_HAVE_AN_X_AND_Y_SCALE
            } else if (flags & 0x0080u) {
                p += 8;  // WE_HAVE_A_TWO_BY_TWO
            }
            if (!(flags & 0x0020u)) {  // WE_HAVE_MORE_COMPONENTS
                break;
            }
        }
    }
    return 1;
}

// stbtt_InitFont, with a validated front door: the buffer's sfnt structure
// is checked by kwe_sfnt_tables_ok before any stb call, and a .ttc
// collection is opened at its first font offset (stb_truetype has no
// IsCollection helper — a collection is detected by its "ttcf" tag).
// stbtt_GetFontOffsetForIndex itself trusts the collection's claims, so
// the shim resolves the offsets itself: every embedded offset must lie
// inside the buffer with a valid offset table, and the FIRST font (the
// one actually opened) must pass the full record walk.
int kwe_font_init(kwe_font *font, const unsigned char *data, size_t data_len,
                  int fontstart) {
    if (font == NULL || data == NULL || data_len < 12) {
        return 0;
    }
    memset(font, 0, sizeof(*font));
    int is_collection = data[0] == 't' && data[1] == 't' && data[2] == 'c' &&
                        data[3] == 'f';
    if (is_collection) {
        unsigned int num_fonts = kwe_u32(data + 8);
        if (num_fonts == 0) {
            return 0;
        }
        if ((size_t)num_fonts > (data_len - 12) / 4) {
            return 0;  // a 12-byte ttcf stub cannot claim fonts
        }
        size_t first = 0;
        for (unsigned int i = 0; i < num_fonts; i++) {
            size_t off = (size_t)kwe_u32(data + 12 + (size_t)i * 4);
            if (off < 12 || off >= data_len) {
                return 0;  // embedded offset outside the buffer
            }
            if (i == 0) {
                first = off;
            } else if (!kwe_sfnt_tables_ok(data, data_len, off, NULL)) {
                return 0;
            }
        }
        if (!kwe_sfnt_tables_ok(data, data_len, first, font)) {
            return 0;
        }
        fontstart = (int)first;
    } else if (!kwe_sfnt_tables_ok(data, data_len, (size_t)fontstart, font)) {
        return 0;
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
// zero-sized box; a glyph whose outline claims fail the validated bounds
// also reports a zero box (rendered as nothing).
void kwe_font_glyph_bitmap_box(const kwe_font *font, int glyph, float scale_x,
                               float scale_y, int *ix0, int *iy0, int *ix1,
                               int *iy1) {
    if (!kwe_glyph_outline_ok(font, glyph)) {
        *ix0 = *iy0 = *ix1 = *iy1 = 0;
        return;
    }
    stbtt_GetGlyphBitmapBoxSubpixel(&font->info, glyph, scale_x, scale_y, 0.0f,
                                    0.0f, ix0, iy0, ix1, iy1);
}

// Rasterize one glyph into the caller's buffer. The buffer is (out_w x
// out_h) with the given row stride and covers the bitmap region that starts
// at (ix0, iy0) — i.e. bitmap pixel (ix0 + bx, iy0 + by) lands at out[bx +
// by * stride]. Returns 1 when the glyph rendered, 0 when it was refused
// (oversized outline, invalid claims) or empty. `out` may be NULL when both
// dimensions are zero.
int kwe_font_render_glyph(const kwe_font *font, unsigned char *out, int out_w,
                          int out_h, int out_stride, float scale_x,
                          float scale_y, int glyph, int ix0, int iy0) {
    if (!kwe_glyph_outline_ok(font, glyph)) {
        return 0;
    }
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
