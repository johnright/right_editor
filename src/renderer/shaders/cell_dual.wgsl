
// dst = color + dst * (1 - weight), per channel
struct FragmentOut {
    @location(0) @blend_src(0) color: vec4<f32>,
    @location(0) @blend_src(1) weight: vec4<f32>,
};

@fragment
fn fs_main(in: VertexOut) -> FragmentOut {
    let shaded = shade(in);
    return FragmentOut(shaded.color, shaded.weight);
}
