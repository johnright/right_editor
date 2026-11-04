
// no dual-source: plain premultiplied alpha, so grayscale text only
@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    return shade(in).color;
}
