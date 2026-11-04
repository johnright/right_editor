use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};

use super::raster::TextParams;

pub const KIND_SOLID: u32 = 0;
pub const KIND_MASK: u32 = 1;
pub const KIND_COLOR: u32 = 2;
pub const KIND_SUBPIXEL: u32 = 3;
pub const KIND_UNDERCURL: u32 = 4;
pub const KIND_DOTTED: u32 = 5;
pub const KIND_DASHED: u32 = 6;
pub const KIND_SHADOW: u32 = 7;

/// One quad, in pixels. 32 bytes, so even a 200x60 frame is a few hundred KB.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Instance {
    pub pos: [f32; 2],
    pub size: [f32; 2],
    pub uv: [f32; 2],
    /// RGBA, r in the low byte for unpack4x8unorm
    pub color: u32,
    pub kind: u32,
}

impl Instance {
    pub fn solid(x: f32, y: f32, w: f32, h: f32, rgb: u32) -> Self {
        Self {
            pos: [x, y],
            size: [w, h],
            uv: [0.0; 2],
            color: pack_rgb(rgb),
            kind: KIND_SOLID,
        }
    }
}

pub fn pack_rgb(rgb: u32) -> u32 {
    pack_rgba(rgb, 0xff)
}

pub fn pack_rgba(rgb: u32, alpha: u8) -> u32 {
    let (r, g, b) = ((rgb >> 16) & 0xff, (rgb >> 8) & 0xff, rgb & 0xff);
    r | (g << 8) | (b << 16) | ((alpha as u32) << 24)
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub range: std::ops::Range<u32>,
    pub scissor: Option<[u32; 4]>,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    screen: [f32; 2],
    enhanced_contrast: f32,
    grayscale_enhanced_contrast: f32,
    gamma_ratios: [f32; 4],
    // x = shadow blur in px
    shadow: [f32; 4],
}

pub struct CellPipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    globals: wgpu::Buffer,
    instances: wgpu::Buffer,
    capacity: usize,
    params: TextParams,
}

impl CellPipeline {
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        atlas: &wgpu::TextureView,
        dual_source: bool,
        params: TextParams,
    ) -> Self {
        // dual-source: per-channel blend weights, which ClearType needs
        let (source, blend) = if dual_source {
            let source = format!(
                "enable dual_source_blending;\n{}{}",
                include_str!("shaders/cell.wgsl"),
                include_str!("shaders/cell_dual.wgsl")
            );
            let component = |dst_factor| wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor,
                operation: wgpu::BlendOperation::Add,
            };
            let blend = wgpu::BlendState {
                color: component(wgpu::BlendFactor::OneMinusSrc1),
                alpha: component(wgpu::BlendFactor::OneMinusSrc1Alpha),
            };
            (source, blend)
        } else {
            let source = format!(
                "{}{}",
                include_str!("shaders/cell.wgsl"),
                include_str!("shaders/cell_single.wgsl")
            );
            (source, wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING)
        };
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cell"),
            source: wgpu::ShaderSource::Wgsl(Cow::Owned(source)),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cell"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cell"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let attributes = wgpu::vertex_attr_array![
            0 => Float32x2, 1 => Float32x2, 2 => Float32x2, 3 => Uint32, 4 => Uint32
        ];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cell"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<Instance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &attributes,
                })],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(blend),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cell"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(atlas),
                },
            ],
        });
        let capacity = 16 * 1024;
        let instances = Self::instance_buffer(device, capacity);
        Self {
            pipeline,
            bind_group,
            globals,
            instances,
            capacity,
            params,
        }
    }

    fn instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (capacity * size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        instances: &[Instance],
        screen: [f32; 2],
        shadow_blur: f32,
    ) {
        if instances.len() > self.capacity {
            self.capacity = instances.len().next_power_of_two();
            self.instances = Self::instance_buffer(device, self.capacity);
        }
        queue.write_buffer(
            &self.globals,
            0,
            bytemuck::bytes_of(&Globals {
                screen,
                enhanced_contrast: self.params.enhanced_contrast,
                grayscale_enhanced_contrast: self.params.grayscale_enhanced_contrast,
                gamma_ratios: self.params.gamma_ratios,
                shadow: [shadow_blur, 0.0, 0.0, 0.0],
            }),
        );
        if !instances.is_empty() {
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(instances));
        }
    }

    /// Usually one segment, plus one per region that's mid-scroll.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, segments: &[Segment], screen: [u32; 2]) {
        if segments.iter().all(|s| s.range.is_empty()) {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.instances.slice(..));
        for segment in segments.iter().filter(|s| !s.range.is_empty()) {
            let [x, y, w, h] = segment.scissor.unwrap_or([0, 0, screen[0], screen[1]]);
            let (x, y) = (x.min(screen[0]), y.min(screen[1]));
            let (w, h) = (w.min(screen[0] - x), h.min(screen[1] - y));
            if w == 0 || h == 0 {
                continue;
            }
            pass.set_scissor_rect(x, y, w, h);
            pass.draw(0..4, segment.range.clone());
        }
    }
}

/// While animating, the cursor's corners move independently, so it's an
/// arbitrary quad. We draw its bounding box and compute coverage from the
/// distance to the four edges, which keeps slanted edges antialiased.
pub struct CursorPipeline {
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CursorUniform {
    // px, clockwise from top-left. The shader relies on the winding.
    corners: [[f32; 2]; 4],
    color: [f32; 4],
    screen: [f32; 2],
    _pad: [f32; 2],
}

const CURSOR_SHADER: &str = "
struct Cursor {
    corners: array<vec4<f32>, 2>,
    color: vec4<f32>,
    screen: vec2<f32>,
};

@group(0) @binding(0) var<uniform> cursor: Cursor;

fn corner(i: u32) -> vec2<f32> {
    let v = cursor.corners[i / 2u];
    return select(v.xy, v.zw, (i % 2u) == 1u);
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // Rectangle containing the quadrilateral, widened by one pixel for the soft edges.
    var lo = corner(0u);
    var hi = corner(0u);
    for (var i = 1u; i < 4u; i++) {
        lo = min(lo, corner(i));
        hi = max(hi, corner(i));
    }
    lo -= vec2<f32>(1.0);
    hi += vec2<f32>(1.0);
    let t = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let px = mix(lo, hi, t);
    return vec4<f32>(px.x / cursor.screen.x * 2.0 - 1.0, 1.0 - px.y / cursor.screen.y * 2.0, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    // Signed distance from the nearest side (positive inside): with the corners
    // clockwise and y pointing down, the inward normal of a->b is (-e.y, e.x).
    var inside = 1e6;
    for (var i = 0u; i < 4u; i++) {
        let a = corner(i);
        let e = corner((i + 1u) % 4u) - a;
        let len = length(e);
        if len > 0.001 {
            let n = vec2<f32>(-e.y, e.x) / len;
            inside = min(inside, dot(pos.xy - a, n));
        }
    }
    let coverage = clamp(inside + 0.5, 0.0, 1.0);
    return vec4<f32>(cursor.color.rgb, cursor.color.a * coverage);
}
";

impl CursorPipeline {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cursor"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(CURSOR_SHADER)),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cursor"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cursor"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cursor"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cursor"),
            size: size_of::<CursorUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cursor"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            }],
        });
        Self {
            pipeline,
            uniform,
            bind_group,
        }
    }

    pub fn upload(&self, queue: &wgpu::Queue, corners: [[f32; 2]; 4], rgb: u32, screen: [f32; 2]) {
        let c = |shift: u32| ((rgb >> shift) & 0xff) as f32 / 255.0;
        let uniform = CursorUniform {
            corners,
            color: [c(16), c(8), c(0), 1.0],
            screen,
            _pad: [0.0; 2],
        };
        queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&uniform));
    }

    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, screen: [u32; 2]) {
        pass.set_scissor_rect(0, 0, screen[0], screen[1]);
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..4, 0..1);
    }
}
