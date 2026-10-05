// Vector glyphs, after Eric Lengyel's Slug algorithm (https://jcgt.org/published/0006/02/02/,
// reference shaders MIT-licensed). Concatenated after `shaders_storage.wgsl`: the glyph
// encodings live in a storage buffer, laid out as `encode_vector_glyph` in gpui describes.

struct VectorSprite {
    order: u32,
    glyph: u32,
    bounds: Bounds,
    content_mask: Bounds,
    color: Hsla,
    origin: vec2<f32>,
    font_size: f32,
    pad: u32,
}

@group(1) @binding(0) var<storage, read> b_vector_sprites: array<VectorSprite>;
@group(2) @binding(2) var<storage, read> b_vector_glyphs: array<u32>;

struct VectorSpriteVarying {
    @builtin(position) position: vec4<f32>,
    // The pixel's position relative to the glyph origin, in ems, y up.
    @location(0) em_position: vec2<f32>,
    @location(1) @interpolate(flat) color: vec4<f32>,
    @location(2) @interpolate(flat) glyph: u32,
    @location(3) @interpolate(flat) pixels_per_em: f32,
    @location(4) clip_distances: vec4<f32>,
}

@vertex
fn vs_vector_sprite(@builtin(vertex_index) vertex_id: u32, @builtin(instance_index) instance_id: u32) -> VectorSpriteVarying {
    let unit_vertex = vec2<f32>(f32(vertex_id & 1u), 0.5 * f32(vertex_id & 2u));
    let sprite = b_vector_sprites[instance_id];
    let position = unit_vertex * sprite.bounds.size + sprite.bounds.origin;

    var out = VectorSpriteVarying();
    out.position = to_device_position(unit_vertex, sprite.bounds);
    out.em_position = vec2<f32>(position.x - sprite.origin.x, sprite.origin.y - position.y) / sprite.font_size;
    out.color = gpui_color_to_framebuffer(sprite.color);
    out.glyph = sprite.glyph;
    out.pixels_per_em = sprite.font_size;
    out.clip_distances = distance_from_clip_rect(unit_vertex, sprite.bounds, sprite.content_mask);
    return out;
}

fn glyph_float(index: u32) -> f32 {
    return bitcast<f32>(b_vector_glyphs[index]);
}

// Which of a curve's two roots cross the ray, from the signs of the curve's three control
// point heights relative to it: bit 0 for the first root, bit 8 for the second.
fn vector_root_code(y1: f32, y2: f32, y3: f32) -> u32 {
    let i1 = bitcast<u32>(y1) >> 31u;
    let i2 = bitcast<u32>(y2) >> 30u;
    let i3 = bitcast<u32>(y3) >> 29u;
    var shift = (i2 & 2u) | (i1 & ~2u);
    shift = (i3 & 4u) | (shift & ~4u);
    return (0x2E74u >> shift) & 0x0101u;
}

// Where a curve crosses the ray, as distances along the ray. `p12` holds the first two
// control points and `p3` the last, relative to the pixel, with the ray along x.
fn vector_solve(p12: vec4<f32>, p3: vec2<f32>) -> vec2<f32> {
    let a = p12.xy - p12.zw * 2.0 + p3;
    let b = p12.xy - p12.zw;
    let d = sqrt(max(b.y * b.y - a.y * p12.y, 0.0));
    var t1 = (b.y - d) / a.y;
    var t2 = (b.y + d) / a.y;
    if (abs(a.y) < 1.0 / 65536.0) {
        t1 = p12.y * 0.5 / b.y;
        t2 = t1;
    }
    return vec2<f32>((a.x * t1 - b.x * 2.0) * t1 + p12.x, (a.x * t2 - b.x * 2.0) * t2 + p12.x);
}

struct RayCoverage {
    coverage: f32,
    weight: f32,
}

// Casts a ray along +x from the pixel through the curves of the band whose header is at
// `header`. With `vertical`, the curves' axes are swapped first, which casts the ray along +y.
fn vector_cast(glyph: u32, header: u32, pixel: vec2<f32>, pixels_per_em: f32, vertical: bool) -> RayCoverage {
    var result = RayCoverage(0.0, 0.0);
    let count = b_vector_glyphs[header];
    let list = glyph + b_vector_glyphs[header + 1u];
    for (var i = 0u; i < count; i++) {
        let curve = glyph + b_vector_glyphs[list + i];
        var p1 = vec2<f32>(glyph_float(curve), glyph_float(curve + 1u)) - pixel;
        var p2 = vec2<f32>(glyph_float(curve + 2u), glyph_float(curve + 3u)) - pixel;
        var p3 = vec2<f32>(glyph_float(curve + 4u), glyph_float(curve + 5u)) - pixel;
        if (vertical) {
            p1 = p1.yx;
            p2 = p2.yx;
            p3 = p3.yx;
        }
        // Curves are sorted by how far along the ray they reach, so the rest lie behind.
        if (max(max(p1.x, p2.x), p3.x) * pixels_per_em < -0.5) {
            break;
        }
        let code = vector_root_code(p1.y, p2.y, p3.y);
        if (code != 0u) {
            let r = vector_solve(vec4<f32>(p1, p2), p3) * pixels_per_em;
            if ((code & 1u) != 0u) {
                result.coverage += clamp(r.x + 0.5, 0.0, 1.0);
                result.weight = max(result.weight, clamp(1.0 - abs(r.x) * 2.0, 0.0, 1.0));
            }
            if (code > 1u) {
                result.coverage -= clamp(r.y + 0.5, 0.0, 1.0);
                result.weight = max(result.weight, clamp(1.0 - abs(r.y) * 2.0, 0.0, 1.0));
            }
        }
    }
    return result;
}

@fragment
fn fs_vector_sprite(input: VectorSpriteVarying) -> @location(0) vec4<f32> {
    if (any(input.clip_distances < vec4<f32>(0.0))) {
        return vec4<f32>(0.0);
    }
    let glyph = input.glyph;
    let pixel = input.em_position;
    let bounds_min = vec2<f32>(glyph_float(glyph), glyph_float(glyph + 1u));
    let bounds_max = vec2<f32>(glyph_float(glyph + 2u), glyph_float(glyph + 3u));
    // Vertical bands split x and horizontal bands split y.
    let band_counts = vec2<u32>(b_vector_glyphs[glyph + 5u], b_vector_glyphs[glyph + 4u]);
    let band_position = (pixel - bounds_min) / (bounds_max - bounds_min) * vec2<f32>(band_counts);
    let band = vec2<u32>(clamp(band_position, vec2<f32>(0.0), vec2<f32>(band_counts - 1u)));

    // Two-word band headers start at word 6: horizontal bands, then vertical ones.
    let headers = glyph + 6u;
    let x = vector_cast(glyph, headers + 2u * band.y, pixel, input.pixels_per_em, false);
    let y = vector_cast(glyph, headers + 2u * (band_counts.y + band.x), pixel, input.pixels_per_em, true);

    // The ray along y sees curve directions mirrored, so its coverage counts the other way.
    let combined = abs(x.coverage * x.weight - y.coverage * y.weight) / max(x.weight + y.weight, 1.0 / 65536.0);
    let coverage = clamp(max(combined, min(abs(x.coverage), abs(y.coverage))), 0.0, 1.0);
    let alpha = apply_contrast_and_gamma_correction(coverage, input.color.rgb, gamma_params.grayscale_enhanced_contrast, gamma_params.gamma_ratios);
    return blend_color(input.color, alpha);
}
