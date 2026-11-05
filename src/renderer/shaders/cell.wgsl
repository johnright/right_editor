// Shared by every quad: backgrounds, decorations, shadows, atlas glyphs.
//
// Text blending copies DirectWrite (as Windows Terminal and Zed do): coverage
// gets gamma/contrast correction based on the text color, in the surface's
// sRGB space.
//
// No fragment entry point here, cell_dual.wgsl or cell_single.wgsl gets
// appended depending on dual-source support.

struct Globals {
    screen: vec2<f32>,
    enhanced_contrast: f32,
    grayscale_enhanced_contrast: f32,
    gamma_ratios: vec4<f32>,
    shadow: vec4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas: texture_2d<f32>;

const KIND_SOLID: u32 = 0u;
const KIND_MASK: u32 = 1u;
const KIND_COLOR: u32 = 2u;
const KIND_SUBPIXEL: u32 = 3u;
const KIND_UNDERCURL: u32 = 4u;
const KIND_DOTTED: u32 = 5u;
const KIND_DASHED: u32 = 6u;
const KIND_SHADOW: u32 = 7u;

struct Instance {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) color: u32,
    @location(4) kind: u32,
};

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
    @location(3) @interpolate(flat) size: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, inst: Instance) -> VertexOut {
    let corner = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let px = inst.pos + corner * inst.size;
    var out: VertexOut;
    out.clip = vec4<f32>(px.x / globals.screen.x * 2.0 - 1.0, 1.0 - px.y / globals.screen.y * 2.0, 0.0, 1.0);
    out.uv = inst.uv + corner * inst.size;
    out.color = unpack4x8unorm(inst.color);
    out.kind = inst.kind;
    out.size = inst.size;
    return out;
}

fn enhance_contrast(alpha: f32, k: f32) -> f32 {
    return alpha * (k + 1.0) / (alpha * k + 1.0);
}

fn enhance_contrast3(alpha: vec3<f32>, k: f32) -> vec3<f32> {
    return alpha * (k + 1.0) / (alpha * k + 1.0);
}

fn alpha_correction(a: f32, f: f32, g: vec4<f32>) -> f32 {
    return a + a * (1.0 - a) * ((g.x * f + g.y) * a + (g.z * f + g.w));
}

fn alpha_correction3(a: vec3<f32>, f: vec3<f32>, g: vec4<f32>) -> vec3<f32> {
    return a + a * (1.0 - a) * ((g.x * f + g.y) * a + (g.z * f + g.w));
}

const TAU: f32 = 6.2831853;

fn decoration_coverage(in: VertexOut) -> f32 {
    if in.kind == KIND_UNDERCURL {
        // sine, stroke = h/4. AA from the distance to the curve, divided by the
        // slope term so steep parts don't get thinner
        let h = in.size.y;
        let stroke = h / 4.0;
        let amp = (h - stroke) / 2.0;
        let period = 2.0 * h;
        let phase = in.uv.x * TAU / period;
        let wave = h / 2.0 + amp * sin(phase);
        let slope = amp * TAU / period * cos(phase);
        let dist = abs(in.uv.y - wave) / sqrt(1.0 + slope * slope);
        return clamp(stroke / 2.0 + 0.5 - dist, 0.0, 1.0);
    }
    let stroke = max(in.size.y, 1.0);
    if in.kind == KIND_DOTTED {
        return select(0.0, 1.0, fract(in.uv.x / (2.0 * stroke)) < 0.5);
    }
    // dashed: 3 on, 2 off
    return select(0.0, 1.0, fract(in.uv.x / (5.0 * stroke)) < 0.6);
}

struct Shaded {
    // premultiplied
    color: vec4<f32>,
    // how much of dst to cover, per channel
    weight: vec4<f32>,
};

fn shade(in: VertexOut) -> Shaded {
    var out: Shaded;
    let fg = in.color.rgb;
    if in.kind == KIND_SOLID {
        out.color = vec4<f32>(fg * in.color.a, in.color.a);
        out.weight = vec4<f32>(in.color.a);
        return out;
    }
    // the quad is already grown by `blur` on each side; fade with distance
    // from the inner rect
    if in.kind == KIND_SHADOW {
        let blur = max(globals.shadow.x, 1.0);
        let half = in.size / 2.0 - vec2<f32>(blur);
        let q = abs(in.uv - in.size / 2.0) - half;
        let dist = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0);
        let t = clamp(dist / blur, 0.0, 1.0);
        // quadratic, looks closer to a gaussian than linear
        let a = in.color.a * (1.0 - t) * (1.0 - t);
        out.color = vec4<f32>(fg * a, a);
        out.weight = vec4<f32>(a);
        return out;
    }
    // patterned underlines: uv.x is absolute x, uv.y is local
    if in.kind >= KIND_UNDERCURL {
        let a = decoration_coverage(in) * in.color.a;
        out.color = vec4<f32>(fg * a, a);
        out.weight = vec4<f32>(a);
        return out;
    }
    // glyphs are pixel-aligned, so textureLoad instead of a sampler
    let texel = textureLoad(atlas, vec2<i32>(floor(in.uv)), 0);
    if in.kind == KIND_COLOR {
        out.color = vec4<f32>(texel.rgb * texel.a, texel.a);
        out.weight = vec4<f32>(texel.a);
        return out;
    }
    if in.kind == KIND_SUBPIXEL {
        // no light-on-dark backoff like Windows Terminal's: looked dim next to Zed
        let k = globals.enhanced_contrast;
        let a = alpha_correction3(enhance_contrast3(texel.rgb, k), fg, globals.gamma_ratios) * in.color.a;
        out.color = vec4<f32>(fg * a, max(a.r, max(a.g, a.b)));
        out.weight = vec4<f32>(a, out.color.a);
        return out;
    }
    let k = globals.grayscale_enhanced_contrast;
    let intensity = dot(fg, vec3<f32>(0.25, 0.5, 0.25));
    let a = alpha_correction(enhance_contrast(texel.a, k), intensity, globals.gamma_ratios) * in.color.a;
    out.color = vec4<f32>(fg * a, a);
    out.weight = vec4<f32>(a);
    return out;
}
