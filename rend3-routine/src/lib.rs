/// PBR Render Routine for rend3.
/// target: todo!(), clear: todo!(), resolve: todo!()  target: todo!(), clear: todo!(), resolve: todo!()  target: todo!(), clear: todo!(), resolve: todo!()  target: todo!(), clear: todo!(), resolve: todo!()
/// Contains [`PbrMaterial`] and the [`PbrRenderRoutine`] which serve as the default render routines.
///
/// Tries to strike a balance between photorealism and performance.
use std::sync::Arc;

use glam::{UVec2, Vec4};
use rend3::{
    format_sso,
    managers::{DirectionalLightManager, MaterialManager, TextureManager},
    types::{TextureFormat, TextureHandle, TextureUsages},
    util::bind_merge::BindGroupBuilder,
    DepthHandle, ModeData, ReadyData, RenderGraph, RenderPassDepthTarget, RenderPassTarget, RenderPassTargets,
    RenderTargetDescriptor, Renderer, RendererMode,
};
use wgpu::{BindGroup, Buffer, Color, Device};

pub use utils::*;

use crate::{
    culling::CulledObjectSet,
    material::{PbrMaterial, TransparencyType},
};

pub mod common;
pub mod culling;
pub mod directional;
pub mod forward;
pub mod material;
pub mod shaders;
pub mod skybox;
pub mod tonemapping;
pub mod uniforms;
mod utils;
pub mod vertex;

struct CulledPerMaterial {
    inner: CulledObjectSet,
    per_material: BindGroup,
}

/// Render routine that renders the using PBR materials and gpu based culling.
pub struct PbrRenderRoutine {
    pub interfaces: common::interfaces::ShaderInterfaces,
    pub samplers: common::samplers::Samplers,
    pub cpu_culler: culling::cpu::CpuCuller,
    pub gpu_culler: ModeData<(), culling::gpu::GpuCuller>,
    pub primary_passes: PrimaryPasses,

    pub ambient: Vec4,

    pub render_texture_options: RenderTextureOptions,
}

impl PbrRenderRoutine {
    pub fn new(renderer: &Renderer, render_texture_options: RenderTextureOptions) -> Self {
        profiling::scope!("PbrRenderRoutine::new");

        let interfaces = common::interfaces::ShaderInterfaces::new(&renderer.device, renderer.mode);

        let samplers = common::samplers::Samplers::new(&renderer.device);

        let cpu_culler = culling::cpu::CpuCuller::new();
        let gpu_culler = renderer
            .mode
            .into_data(|| (), || culling::gpu::GpuCuller::new(&renderer.device));

        let primary_passes = {
            let d2_textures = renderer.d2_texture_manager.read();
            let directional_light = renderer.directional_light_manager.read();
            let mut materials = renderer.material_manager.write();
            // TODO(material): figure out a better way for zero materials to work
            materials.ensure_archetype::<PbrMaterial>(&renderer.device, renderer.mode);
            PrimaryPasses::new(PrimaryPassesNewArgs {
                mode: renderer.mode,
                device: &renderer.device,
                d2_textures: &d2_textures,
                directional_lights: &directional_light,
                materials: &materials,
                interfaces: &interfaces,
                samples: render_texture_options.samples,
            })
        };

        Self {
            interfaces,
            samplers,
            cpu_culler,
            gpu_culler,
            primary_passes,

            ambient: Vec4::new(0.0, 0.0, 0.0, 1.0),

            render_texture_options,
        }
    }

    pub fn set_ambient_color(&mut self, color: Vec4) {
        self.ambient = color;
    }

    pub fn resize(&mut self, renderer: &Renderer, options: RenderTextureOptions) {
        profiling::scope!("PbrRenderRoutine::resize");
        let different_sample_count = self.render_texture_options.samples != options.samples;
        if different_sample_count {
            let d2_textures = renderer.d2_texture_manager.read();
            let directional_light = renderer.directional_light_manager.read();
            let materials = renderer.material_manager.read();
            self.primary_passes = PrimaryPasses::new(PrimaryPassesNewArgs {
                mode: renderer.mode,
                device: &renderer.device,
                d2_textures: &d2_textures,
                directional_lights: &directional_light,
                materials: &materials,
                interfaces: &self.interfaces,
                samples: options.samples,
            });
        }
        self.render_texture_options = options;
    }

    pub fn add_pre_cull_to_graph<'node>(&'node self, graph: &mut RenderGraph<'node>) {
        let mut declare_pre_cull = |node_name: &'node str, buf_name: &'node str, transparency: TransparencyType| {
            let mut builder = graph.add_node(node_name);
            let handle = builder.add_data_output::<_, Buffer>(buf_name);

            builder.build(move |_pt, renderer, _encoder_or_pass, _temps, _ready, graph_data| {
                let objects = graph_data
                    .object_manager
                    .get_objects::<PbrMaterial>(transparency as u64);
                let objects =
                    common::sorting::sort_objects(objects, graph_data.camera_manager, transparency.to_sorting());
                let buffer = culling::gpu::build_cull_data(&renderer.device, &objects);
                graph_data.set_data::<Buffer>(handle, Some(buffer));
            });
        };

        declare_pre_cull(
            "Opaque Pre-Cull",
            "Opaque Pre-Cull Data",
            material::TransparencyType::Opaque,
        );
        declare_pre_cull(
            "Cutout Pre-Cull",
            "Cutout Pre-Cull Data",
            material::TransparencyType::Cutout,
        );
        declare_pre_cull(
            "Blend Pre-Cull",
            "Blend Pre-Cull Data",
            material::TransparencyType::Blend,
        );

        let mut builder = graph.add_node("build uniform data");
        let shadow_handle = builder.add_data_output::<_, BindGroup>("Shadow Uniform Data");
        let forward_handle = builder.add_data_output::<_, BindGroup>("Forward Uniform Data");
        builder.build(move |_pt, renderer, _encoder_or_pass, _temps, _ready, graph_data| {
            let mut bgb = BindGroupBuilder::new();

            self.samplers.add_to_bg(&mut bgb);

            let uniform_buffer = uniforms::create_shader_uniform(uniforms::CreateShaderUniformArgs {
                device: &renderer.device,
                camera: graph_data.camera_manager,
                interfaces: &self.interfaces,
                ambient: self.ambient,
            });

            bgb.append_buffer(&uniform_buffer);

            let shadow_uniform_bg = bgb.build(
                &renderer.device,
                Some("shadow uniform bg"),
                &self.interfaces.shadow_uniform_bgl,
            );

            graph_data.directional_light_manager.add_to_bg(&mut bgb);

            let forward_uniform_bg = bgb.build(
                &renderer.device,
                Some("forward uniform bg"),
                &self.interfaces.forward_uniform_bgl,
            );

            graph_data.set_data(shadow_handle, Some(shadow_uniform_bg));
            graph_data.set_data(forward_handle, Some(forward_uniform_bg));
        })
    }

    pub fn add_shadow_culling_to_graph<'node>(&'node self, graph: &mut RenderGraph<'node>, ready: &ReadyData) {
        for idx in 0..ready.directional_light_cameras.len() {
            for (transparency, pre_cull_name, cull_name) in [
                (
                    TransparencyType::Opaque,
                    "Opaque Pre-Cull Data",
                    format_sso!("Opaque S{} Cull", idx),
                ),
                (
                    TransparencyType::Cutout,
                    "Cutout Pre-Cull Data",
                    format_sso!("Cutout S{} Cull", idx),
                ),
            ] {
                let mut builder = graph.add_node(cull_name.clone());

                let pre_cull_handle = self
                    .gpu_culler
                    .mode()
                    .into_data(|| (), || builder.add_data_input(pre_cull_name));
                let cull_handle = builder.add_data_output::<_, CulledPerMaterial>(cull_name.clone());

                builder.build(move |_pt, renderer, encoder_or_rpass, temps, ready, graph_data| {
                    let encoder = encoder_or_rpass.get_encoder();

                    let culling_input =
                        pre_cull_handle.map_gpu(|handle| graph_data.get_data::<Buffer>(temps, handle).unwrap());

                    let count = graph_data
                        .object_manager
                        .get_objects::<PbrMaterial>(transparency as u64)
                        .len();

                    let culled_objects = match self.gpu_culler {
                        ModeData::CPU(_) => self.cpu_culler.cull(culling::cpu::CpuCullerCullArgs {
                            device: &renderer.device,
                            camera: &ready.directional_light_cameras[idx],
                            interfaces: &self.interfaces,
                            objects: graph_data.object_manager,
                            transparency,
                        }),
                        ModeData::GPU(ref gpu_culler) => gpu_culler.cull(culling::gpu::GpuCullerCullArgs {
                            device: &renderer.device,
                            encoder,
                            interfaces: &self.interfaces,
                            camera: &ready.directional_light_cameras[idx],
                            input_buffer: culling_input.into_gpu(),
                            input_count: count,
                            transparency,
                        }),
                    };

                    let mut per_material_bgb = BindGroupBuilder::new();
                    per_material_bgb.append_buffer(&culled_objects.output_buffer);

                    if renderer.mode == RendererMode::GPUPowered {
                        graph_data
                            .material_manager
                            .add_to_bg_gpu::<PbrMaterial>(&mut per_material_bgb);
                    }

                    let per_material_bg =
                        per_material_bgb.build(&renderer.device, None, &self.interfaces.per_material_bgl);

                    graph_data.set_data(
                        cull_handle,
                        Some(CulledPerMaterial {
                            inner: culled_objects,
                            per_material: per_material_bg,
                        }),
                    );
                });
            }
        }
    }

    pub fn add_shadow_rendering_to_graph<'node>(&'node self, graph: &mut RenderGraph<'node>, ready: &ReadyData) {
        for idx in 0..ready.directional_light_cameras.len() {
            for (transparency, cull_name) in [
                (TransparencyType::Opaque, format_sso!("Opaque S{} Cull", idx)),
                (TransparencyType::Cutout, format_sso!("Cutout S{} Cull", idx)),
            ] {
                let mut builder = graph.add_node(format_sso!("{} S{} Render", transparency.to_debug_str(), idx));

                let shadow_uniform_handle = builder.add_data_input::<_, BindGroup>("Shadow Uniform Data");
                let culled_handle = builder.add_data_input::<_, CulledPerMaterial>(cull_name);
                let shadow_output_handle = builder.add_shadow_output(idx);

                let rpass_handle = builder.add_renderpass(RenderPassTargets {
                    targets: vec![],
                    depth_stencil: Some(RenderPassDepthTarget {
                        target: DepthHandle::Shadow(shadow_output_handle),
                        depth_clear: Some(1.0),
                        stencil_clear: None,
                    }),
                });

                let pt_handle = builder.passthrough_data(self);

                builder.build(move |pt, _renderer, encoder_or_pass, temps, ready, graph_data| {
                    let this = pt.get(pt_handle);
                    let rpass = encoder_or_pass.get_rpass(rpass_handle);
                    let shadow_uniform = graph_data.get_data(temps, shadow_uniform_handle).unwrap();
                    let culled = graph_data.get_data(temps, culled_handle).unwrap();

                    graph_data.mesh_manager.buffers().bind(rpass);
                    rpass.set_pipeline(&this.primary_passes.shadow_passes.opaque_pipeline);
                    rpass.set_bind_group(0, shadow_uniform, &[]);
                    rpass.set_bind_group(1, &culled.per_material, &[]);

                    match culled.inner.calls {
                        ModeData::CPU(ref draws) => culling::cpu::run(rpass, draws, graph_data.material_manager, 2),
                        ModeData::GPU(ref data) => {
                            rpass.set_bind_group(2, ready.d2_texture.bg.as_gpu(), &[]);
                            culling::gpu::run(rpass, data);
                        }
                    }
                });
            }
        }
    }

    pub fn add_culling_to_graph<'node>(&'node self, graph: &mut RenderGraph<'node>) {
        for (transparency, pre_cull_name, post_cull_name) in [
            (TransparencyType::Opaque, "Opaque Pre-Cull Data", "Opaque Forward Cull"),
            (TransparencyType::Cutout, "Cutout Pre-Cull Data", "Cutout Forward Cull"),
            (TransparencyType::Blend, "Blend Pre-Cull Data", "Blend Forward Cull"),
        ] {
            let mut builder = graph.add_node(post_cull_name);

            let pre_cull_handle = self
                .gpu_culler
                .mode()
                .into_data(|| (), || builder.add_data_input::<_, Buffer>(pre_cull_name));

            let cull_handle = builder.add_data_output::<_, CulledPerMaterial>(post_cull_name);

            builder.build(move |_pt, renderer, encoder_or_pass, temps, _ready, graph_data| {
                let encoder = encoder_or_pass.get_encoder();

                let culling_input = pre_cull_handle.map_gpu(|handle| graph_data.get_data(temps, handle).unwrap());

                let culler = self.gpu_culler.as_ref().map_cpu(|_| &self.cpu_culler);

                let pass = match transparency {
                    TransparencyType::Opaque => &self.primary_passes.opaque_pass,
                    TransparencyType::Cutout => &self.primary_passes.cutout_pass,
                    TransparencyType::Blend => &self.primary_passes.transparent_pass,
                };

                let culled_objects = pass.cull(forward::ForwardPassCullArgs {
                    device: &renderer.device,
                    encoder,
                    culler,
                    interfaces: &self.interfaces,
                    camera: graph_data.camera_manager,
                    objects: graph_data.object_manager,
                    culling_input,
                });

                let mut per_material_bgb = BindGroupBuilder::new();
                per_material_bgb.append_buffer(&culled_objects.output_buffer);

                if renderer.mode == RendererMode::GPUPowered {
                    graph_data
                        .material_manager
                        .add_to_bg_gpu::<PbrMaterial>(&mut per_material_bgb);
                }

                let per_material_bg = per_material_bgb.build(&renderer.device, None, &self.interfaces.per_material_bgl);

                graph_data.set_data(
                    cull_handle,
                    Some(CulledPerMaterial {
                        inner: culled_objects,
                        per_material: per_material_bg,
                    }),
                );
            });
        }
    }

    pub fn add_prepass_to_graph<'node>(&'node self, graph: &mut RenderGraph<'node>) {
        for (transparency, cull_name, pass_name) in [
            (TransparencyType::Opaque, "Opaque Forward Cull", "Opaque Prepass"),
            (TransparencyType::Cutout, "Cutout Forward Cull", "Cutout Prepass"),
        ] {
            let mut builder = graph.add_node(pass_name);

            let hdr_color_handle = builder.add_render_target_output(
                "hdr color",
                RenderTargetDescriptor {
                    dim: self.render_texture_options.resolution,
                    format: TextureFormat::Rgba16Float,
                    usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
                },
            );

            let hdr_depth_handle = builder.add_render_target_output(
                "hdr depth",
                RenderTargetDescriptor {
                    dim: self.render_texture_options.resolution,
                    format: TextureFormat::Depth32Float,
                    usage: TextureUsages::RENDER_ATTACHMENT,
                },
            );

            let forward_uniform_handle = builder.add_data_input::<_, BindGroup>("Forward Uniform Data");
            let cull_handle = builder.add_data_input::<_, CulledPerMaterial>(cull_name);

            let rpass_handle = builder.add_renderpass(RenderPassTargets {
                targets: vec![RenderPassTarget {
                    target: hdr_color_handle,
                    clear: Color::BLACK,
                    resolve: None,
                }],
                depth_stencil: Some(RenderPassDepthTarget {
                    target: DepthHandle::RenderTarget(hdr_depth_handle),
                    depth_clear: Some(0.0),
                    stencil_clear: None,
                }),
            });

            let pt_handle = builder.passthrough_data(self);

            builder.build(move |pt, renderer, encoder_or_pass, temps, ready, graph_data| {
                let this = pt.get(pt_handle);
                let rpass = encoder_or_pass.get_rpass(rpass_handle);
                let forward_uniform_bg = graph_data.get_data(temps, forward_uniform_handle).unwrap();
                let culled = graph_data.get_data(temps, cull_handle).unwrap();

                let pass = match transparency {
                    TransparencyType::Opaque => &this.primary_passes.opaque_pass,
                    TransparencyType::Cutout => &this.primary_passes.cutout_pass,
                    TransparencyType::Blend => unreachable!(),
                };

                let d2_texture_output_bg_ref = ready.d2_texture.bg.as_ref().map(|_| (), |a| &**a);

                pass.prepass(forward::ForwardPassPrepassArgs {
                    device: &renderer.device,
                    rpass,
                    materials: graph_data.material_manager,
                    meshes: graph_data.mesh_manager.buffers(),
                    forward_uniform_bg,
                    per_material_bg: &culled.per_material,
                    texture_bg: d2_texture_output_bg_ref,
                    culled_objects: &culled.inner,
                });
            });
        }
    }

    pub fn add_forward_to_graph<'node>(&'node self, graph: &mut RenderGraph<'node>) {
        for (transparency, cull_name, pass_name) in [
            (TransparencyType::Opaque, "Opaque Forward Cull", "Opaque Forward"),
            (TransparencyType::Cutout, "Cutout Forward Cull", "Cutout Forward"),
            (TransparencyType::Blend, "Blend Forward Cull", "Blend Forward"),
        ] {
            let mut builder = graph.add_node(pass_name);

            let hdr_color_handle = builder.add_render_target_output(
                "hdr color",
                RenderTargetDescriptor {
                    dim: self.render_texture_options.resolution,
                    format: TextureFormat::Rgba16Float,
                    usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
                },
            );

            let hdr_depth_handle = builder.add_render_target_output(
                "hdr depth",
                RenderTargetDescriptor {
                    dim: self.render_texture_options.resolution,
                    format: TextureFormat::Depth32Float,
                    usage: TextureUsages::RENDER_ATTACHMENT,
                },
            );

            let rpass_handle = builder.add_renderpass(RenderPassTargets {
                targets: vec![RenderPassTarget {
                    target: hdr_color_handle,
                    clear: Color::BLACK,
                    resolve: None,
                }],
                depth_stencil: Some(RenderPassDepthTarget {
                    target: DepthHandle::RenderTarget(hdr_depth_handle),
                    depth_clear: Some(0.0),
                    stencil_clear: None,
                }),
            });

            let _ = builder.add_shadow_array_input();

            let forward_uniform_handle = builder.add_data_input::<_, BindGroup>("Forward Uniform Data");
            let cull_handle = builder.add_data_input::<_, CulledPerMaterial>(cull_name);

            let pt_handle = builder.passthrough_data(self);

            builder.build(move |pt, renderer, encoder_or_pass, temps, ready, graph_data| {
                let this = pt.get(pt_handle);
                let rpass = encoder_or_pass.get_rpass(rpass_handle);
                let forward_uniform_bg = graph_data.get_data(temps, forward_uniform_handle).unwrap();
                let culled = graph_data.get_data(temps, cull_handle).unwrap();

                let pass = match transparency {
                    TransparencyType::Opaque => &this.primary_passes.opaque_pass,
                    TransparencyType::Cutout => &this.primary_passes.cutout_pass,
                    TransparencyType::Blend => &this.primary_passes.transparent_pass,
                };

                let d2_texture_output_bg_ref = ready.d2_texture.bg.as_ref().map(|_| (), |a| &**a);

                pass.draw(forward::ForwardPassDrawArgs {
                    device: &renderer.device,
                    rpass,
                    materials: graph_data.material_manager,
                    meshes: graph_data.mesh_manager.buffers(),
                    samplers: &this.samplers,
                    forward_uniform_bg,
                    per_material_bg: &culled.per_material,
                    texture_bg: d2_texture_output_bg_ref,
                    culled_objects: &culled.inner,
                });
            });
        }
    }
}

pub struct PrimaryPassesNewArgs<'a> {
    pub mode: RendererMode,
    pub device: &'a Device,

    pub d2_textures: &'a TextureManager,
    pub directional_lights: &'a DirectionalLightManager,
    pub materials: &'a MaterialManager,

    pub interfaces: &'a common::interfaces::ShaderInterfaces,

    pub samples: SampleCount,
}

pub struct PrimaryPasses {
    pub shadow_passes: directional::DirectionalShadowPass,
    pub opaque_pass: forward::ForwardPass,
    pub cutout_pass: forward::ForwardPass,
    pub transparent_pass: forward::ForwardPass,
}
impl PrimaryPasses {
    pub fn new(args: PrimaryPassesNewArgs<'_>) -> Self {
        profiling::scope!("PrimaryPasses::new");

        let gpu_d2_texture_bgl = args.mode.into_data(|| (), || args.d2_textures.gpu_bgl());
        let forward_pass_args = common::forward_pass::BuildForwardPassShaderArgs {
            mode: args.mode,
            device: args.device,
            interfaces: args.interfaces,
            texture_bgl: gpu_d2_texture_bgl,
            materials: args.materials,
            samples: args.samples,
            transparency: TransparencyType::Opaque,
            baking: common::forward_pass::Baking::Disabled,
        };
        let opaque_pipeline = Arc::new(common::forward_pass::build_forward_pass_pipeline(
            forward_pass_args.clone(),
        ));

        let shadow_pipelines =
            common::depth_pass::build_depth_pass_pipeline(common::depth_pass::BuildDepthPassShaderArgs {
                mode: args.mode,
                device: args.device,
                interfaces: args.interfaces,
                texture_bgl: gpu_d2_texture_bgl,
                materials: args.materials,
                samples: SampleCount::One,
                ty: common::depth_pass::DepthPassType::Shadow,
            });
        let depth_pipelines =
            common::depth_pass::build_depth_pass_pipeline(common::depth_pass::BuildDepthPassShaderArgs {
                mode: args.mode,
                device: args.device,
                interfaces: args.interfaces,
                texture_bgl: gpu_d2_texture_bgl,
                materials: args.materials,
                samples: args.samples,
                ty: common::depth_pass::DepthPassType::Prepass,
            });
        let cutout_pipeline = Arc::new(common::forward_pass::build_forward_pass_pipeline(
            common::forward_pass::BuildForwardPassShaderArgs {
                transparency: TransparencyType::Cutout,
                ..forward_pass_args.clone()
            },
        ));
        let transparent_pipeline = Arc::new(common::forward_pass::build_forward_pass_pipeline(
            common::forward_pass::BuildForwardPassShaderArgs {
                transparency: TransparencyType::Blend,
                ..forward_pass_args.clone()
            },
        ));
        Self {
            shadow_passes: directional::DirectionalShadowPass::new(
                Arc::clone(&shadow_pipelines.cutout),
                Arc::clone(&shadow_pipelines.opaque),
            ),
            transparent_pass: forward::ForwardPass::new(
                Some(Arc::clone(&depth_pipelines.opaque)),
                transparent_pipeline,
                TransparencyType::Blend,
            ),
            cutout_pass: forward::ForwardPass::new(
                Some(Arc::clone(&depth_pipelines.cutout)),
                cutout_pipeline,
                TransparencyType::Cutout,
            ),
            opaque_pass: forward::ForwardPass::new(
                Some(Arc::clone(&depth_pipelines.opaque)),
                opaque_pipeline,
                TransparencyType::Opaque,
            ),
        }
    }
}

pub struct SkyboxRoutine {
    pub interfaces: common::interfaces::ShaderInterfaces,
    pub skybox_pass: skybox::SkyboxPass,
    pub options: RenderTextureOptions,

    pub skybox_texture: Option<TextureHandle>,
}

impl SkyboxRoutine {
    pub fn new(renderer: &Renderer, options: RenderTextureOptions) -> Self {
        // TODO: clean up
        let interfaces = common::interfaces::ShaderInterfaces::new(&renderer.device, renderer.mode);

        let skybox_pipeline = common::skybox_pass::build_skybox_pipeline(common::skybox_pass::BuildSkyboxShaderArgs {
            mode: renderer.mode,
            device: &renderer.device,
            interfaces: &interfaces,
            samples: SampleCount::One,
        });

        let skybox_pass = skybox::SkyboxPass::new(skybox_pipeline);

        Self {
            skybox_pass,
            options,
            interfaces,
            skybox_texture: None,
        }
    }

    pub fn set_background_texture(&mut self, texture: Option<TextureHandle>) {
        self.skybox_texture = texture;
    }

    pub fn resize(&mut self, renderer: &Renderer, options: RenderTextureOptions) {
        if self.options.samples != options.samples {
            let skybox_pipeline =
                common::skybox_pass::build_skybox_pipeline(common::skybox_pass::BuildSkyboxShaderArgs {
                    mode: renderer.mode,
                    device: &renderer.device,
                    interfaces: &self.interfaces,
                    samples: options.samples,
                });
            self.skybox_pass = skybox::SkyboxPass::new(skybox_pipeline);
        }

        self.options = options;
    }

    pub fn ready(&mut self, renderer: &Renderer) {
        let d2c_texture_manager = renderer.d2c_texture_manager.read();
        self.skybox_pass.update_skybox(skybox::UpdateSkyboxArgs {
            device: &renderer.device,
            interfaces: &self.interfaces,
            d2c_texture_manager: &d2c_texture_manager,
            new_skybox_handle: self.skybox_texture.clone(),
        });
    }

    pub fn add_to_graph<'node>(&'node self, graph: &mut RenderGraph<'node>) {
        let mut builder = graph.add_node("Skybox");

        let hdr_color_handle = builder.add_render_target_output(
            "hdr color",
            RenderTargetDescriptor {
                dim: self.options.resolution,
                format: TextureFormat::Rgba16Float,
                usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
            },
        );

        let hdr_depth_handle = builder.add_render_target_input("hdr depth");

        let rpass_handle = builder.add_renderpass(RenderPassTargets {
            targets: vec![RenderPassTarget {
                target: hdr_color_handle,
                clear: Color::BLACK,
                resolve: None,
            }],
            depth_stencil: Some(RenderPassDepthTarget {
                target: DepthHandle::RenderTarget(hdr_depth_handle),
                depth_clear: Some(0.0),
                stencil_clear: None,
            }),
        });

        let forward_uniform_handle = builder.add_data_input::<_, BindGroup>("Forward Uniform Data");
        let pt_handle = builder.passthrough_data(self);

        builder.build(move |pt, _renderer, encoder_or_pass, temps, _ready, graph_data| {
            let this = pt.get(pt_handle);
            let rpass = encoder_or_pass.get_rpass(rpass_handle);

            let forward_uniform_bg = graph_data.get_data(temps, forward_uniform_handle).unwrap();

            this.skybox_pass.draw_skybox(skybox::SkyboxPassDrawArgs {
                rpass,
                forward_uniform_bg,
            });
        });
    }
}

pub struct TonemappingRoutine {
    pub interfaces: common::interfaces::ShaderInterfaces,
    pub tonemapping_pass: tonemapping::TonemappingPass,
    pub size: UVec2,
}

impl TonemappingRoutine {
    pub fn new(renderer: &Renderer, size: UVec2, output_format: TextureFormat) -> Self {
        // TODO: clean up
        let interfaces = common::interfaces::ShaderInterfaces::new(&renderer.device, renderer.mode);

        let tonemapping_pass = tonemapping::TonemappingPass::new(tonemapping::TonemappingPassNewArgs {
            device: &renderer.device,
            interfaces: &interfaces,
            output_format,
        });

        Self {
            tonemapping_pass,
            size,
            interfaces,
        }
    }

    pub fn resize(&mut self, size: UVec2) {
        self.size = size;
    }

    pub fn add_to_graph<'node>(&'node self, graph: &mut RenderGraph<'node>) {
        let mut builder = graph.add_node("Tonemapping");

        let hdr_color_handle = builder.add_render_target_input("hdr color");

        let forward_uniform_handle = builder.add_data_input::<_, BindGroup>("Forward Uniform Data");
        let output_handle = builder.add_surface_output();

        builder.build(move |_pt, renderer, encoder_or_pass, temps, _ready, graph_data| {
            let encoder = encoder_or_pass.get_encoder();
            let forward_uniform_bg = graph_data.get_data(temps, forward_uniform_handle).unwrap();
            let hdr_color = graph_data.get_render_target(hdr_color_handle);
            let output = graph_data.get_render_target(output_handle);

            self.tonemapping_pass.blit(tonemapping::TonemappingPassBlitArgs {
                device: &renderer.device,
                encoder,
                interfaces: &self.interfaces,
                forward_uniform_bg,
                source: hdr_color,
                target: output,
            });
        });
    }
}
