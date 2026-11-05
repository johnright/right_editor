use std::sync::Arc;

use anyhow::{Context, Result};
use winit::window::Window;

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    pub surface_config: wgpu::SurfaceConfiguration,
    /// needed for ClearType; without it text falls back to grayscale
    pub dual_source: bool,
}

pub enum Frame {
    Ready(wgpu::SurfaceTexture),
    /// minimized/occluded, timeout, or the surface was just reconfigured
    Skip,
}

impl Gpu {
    pub fn new(
        window: Arc<Window>,
        present_mode: Option<wgpu::PresentMode>,
        transparent: bool,
    ) -> Result<Self> {
        // DX12 first, on its own: DXGI flip model is the lowest-latency path we
        // have. Vulkan only as a fallback. WGPU_BACKEND still overrides.
        let candidates: &[wgpu::Backends] = if cfg!(windows) {
            &[wgpu::Backends::DX12, wgpu::Backends::VULKAN]
        } else {
            &[wgpu::Backends::PRIMARY]
        };
        let size = window.inner_size();
        let (surface, adapter) = candidates
            .iter()
            .find_map(|&backends| {
                let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
                desc.backends = backends;
                // HWND swapchains can't do alpha, a DirectComposition visual can
                if transparent {
                    desc.backend_options.dx12.presentation_system =
                        wgpu::Dx12SwapchainKind::DxgiFromVisual;
                }
                let instance = wgpu::Instance::new(desc.with_env());
                let surface = instance.create_surface(window.clone()).ok()?;
                let adapter =
                    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                        power_preference: wgpu::PowerPreference::None,
                        force_fallback_adapter: false,
                        compatible_surface: Some(&surface),
                        ..Default::default()
                    }))
                    .ok()?;
                Some((surface, adapter))
            })
            .context("no compatible GPU adapter")?;
        let info = adapter.get_info();
        log::info!(
            "GPU: {} ({:?}, driver {})",
            info.name,
            info.backend,
            info.driver_info
        );

        let dual_source = adapter
            .features()
            .contains(wgpu::Features::DUAL_SOURCE_BLENDING);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("right_editor"),
            required_features: if dual_source {
                wgpu::Features::DUAL_SOURCE_BLENDING
            } else {
                wgpu::Features::empty()
            },
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .context("creating the device")?;

        let caps = surface.get_capabilities(&adapter);
        log::debug!(
            "surface formats: {:?}, present modes: {:?}",
            caps.formats,
            caps.present_modes
        );
        // non-sRGB on purpose: nvim colors go out untouched and text blends in
        // sRGB space, like terminals do
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        // Mailbox: no tearing, but present doesn't block on vsync and the newest
        // frame wins
        let present_mode = match present_mode {
            Some(mode) if caps.present_modes.contains(&mode) => mode,
            None if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) => {
                wgpu::PresentMode::Mailbox
            }
            None => wgpu::PresentMode::Fifo,
            Some(mode) => {
                log::warn!("present mode {mode:?} not supported, using Fifo");
                wgpu::PresentMode::Fifo
            }
        };
        let alpha_mode = if transparent
            && caps
                .alpha_modes
                .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
        {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            caps.alpha_modes[0]
        };

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            // one frame in flight, so what hits the screen is what we just drew
            // TODO: present still stalls 8-23 ms on hybrid Intel/NVIDIA laptops
            // (cross-adapter copy). 3 buffers and forcing the iGPU didn't help.
            desired_maximum_frame_latency: 1,
            alpha_mode,
            view_formats: vec![],
            ..surface
                .get_default_config(&adapter, 1, 1)
                .context("unsupported surface")?
        };
        surface.configure(&device, &surface_config);
        log::info!(
            "surface: {format:?}, {present_mode:?}, {alpha_mode:?}, {}x{}",
            size.width,
            size.height
        );

        Ok(Self {
            device,
            queue,
            surface,
            surface_config,
            dual_source,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);
    }

    pub fn acquire(&mut self) -> Result<Frame> {
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(tex) => Ok(Frame::Ready(tex)),
            wgpu::CurrentSurfaceTexture::Suboptimal(tex) => {
                Ok(Frame::Ready(tex))
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                Ok(Frame::Skip)
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.surface_config);
                Ok(Frame::Skip)
            }
            other => anyhow::bail!("surface acquisition error: {other:?}"),
        }
    }
}
