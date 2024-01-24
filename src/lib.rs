use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use rendering::{SimpleRenderer, Window, EventLoop, ShaderManager, MeshManager, Mesh, ShaderData, ModelData, VPData, ShaderType, Shader};
use types::buffers::UpdatableBuffer;
use vulkano::buffer::{Buffer, BufferCreateInfo, BufferUsage, Subbuffer, BufferContents};
use vulkano::command_buffer::allocator::StandardCommandBufferAllocator;
use vulkano::command_buffer::{AutoCommandBufferBuilder, CommandBufferUsage, PrimaryAutoCommandBuffer, RenderPassBeginInfo, SubpassBeginInfo, SubpassContents, CopyBufferInfo};
use vulkano::descriptor_set::{PersistentDescriptorSet, WriteDescriptorSet};
use vulkano::descriptor_set::allocator::StandardDescriptorSetAllocator;
use vulkano::device::physical::{PhysicalDevice, PhysicalDeviceType};
use vulkano::device::{Device, DeviceCreateInfo, DeviceExtensions, Queue, QueueCreateInfo, QueueFlags};
use vulkano::format::Format;
use vulkano::image::view::ImageView;
use vulkano::image::{Image, ImageCreateInfo, ImageType, ImageUsage, SampleCount};
use vulkano::instance::{Instance, InstanceCreateInfo};
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter, StandardMemoryAllocator};
use vulkano::pipeline::graphics::color_blend::{ColorBlendAttachmentState, ColorBlendState};
use vulkano::pipeline::graphics::depth_stencil::{DepthState, DepthStencilState};
use vulkano::pipeline::graphics::input_assembly::InputAssemblyState;
use vulkano::pipeline::graphics::multisample::MultisampleState;
use vulkano::pipeline::graphics::rasterization::RasterizationState;
use vulkano::pipeline::graphics::vertex_input::{Vertex, VertexDefinition};
use vulkano::pipeline::graphics::viewport::{Viewport, ViewportState};
use vulkano::pipeline::graphics::GraphicsPipelineCreateInfo;
use vulkano::pipeline::layout::PipelineDescriptorSetLayoutCreateInfo;
use vulkano::pipeline::{GraphicsPipeline, PipelineLayout, PipelineShaderStageCreateInfo, Pipeline, PipelineBindPoint};
use vulkano::render_pass::{Framebuffer, FramebufferCreateInfo, RenderPass, Subpass};
use vulkano::shader::spirv::{bytes_to_words, ImageFormat};
use vulkano::shader::{ShaderModule, ShaderModuleCreateInfo};
use vulkano::swapchain::{self, Surface, Swapchain, SwapchainCreateInfo, SwapchainPresentInfo};
use vulkano::sync::future::{FenceSignalFuture, NowFuture};
use vulkano::sync::{self, GpuFuture};
use vulkano::{Validated, VulkanError};

use winit::event::{Event, WindowEvent};
use winit::event_loop::{ControlFlow};

pub mod types;
use types::vectors::*;
use types::matrices::*;
pub mod rendering;

pub fn read_file_to_words(path: &str) -> Vec<u32> {
    let mut file = File::open(path).unwrap();
    let mut buffer = vec![0u8; file.metadata().unwrap().len() as usize];
    file.read(buffer.as_mut_slice()).unwrap();
    bytes_to_words(buffer.as_slice()).unwrap().to_vec()
}

fn load_shader_module(
    shaders: &HashMap<String, ShaderData>,
    device: &Arc<Device>,
    name: &str,
) -> Arc<ShaderModule> {
    unsafe {
        ShaderModule::new(
            device.clone(),
            ShaderModuleCreateInfo::new(shaders.get(name).unwrap().shader_code.as_slice()),
        )
        .unwrap()
    }
}

pub fn select_physical_device(
    instance: &Arc<Instance>,
    surface: &Arc<Surface>,
    device_extensions: &DeviceExtensions,
) -> (Arc<PhysicalDevice>, u32) {
    instance
        .enumerate_physical_devices()
        .expect("failed to enumerate physical devices")
        .filter(|p| p.supported_extensions().contains(device_extensions))
        .filter_map(|p| {
            p.queue_family_properties()
                .iter()
                .enumerate()
                .position(|(i, q)| {
                    q.queue_flags.contains(QueueFlags::GRAPHICS)
                        && p.surface_support(i as u32, surface).unwrap_or(false)
                })
                .map(|q| (p, q as u32))
        })
        .min_by_key(|(p, _)| match p.properties().device_type {
            PhysicalDeviceType::DiscreteGpu => 0,
            PhysicalDeviceType::IntegratedGpu => 1,
            PhysicalDeviceType::VirtualGpu => 2,
            PhysicalDeviceType::Cpu => 3,
            _ => 4,
        })
        .expect("no device available")
}

fn get_render_pass(device: Arc<Device>, swapchain: Arc<Swapchain>) -> Arc<RenderPass> {
    vulkano::single_pass_renderpass!(
        device,
        attachments: {
            inter: {
                format: swapchain.image_format(), // set the format the same as the swapchain
                samples: 8,
                load_op: Clear,
                store_op: Store,
            },
            color: {
                format: swapchain.image_format(), // set the format the same as the swapchain
                samples: 1,
                load_op: Clear,
                store_op: Store,
            },
            depth: {
                format: Format::D32_SFLOAT,
                samples: 8,
                load_op: Clear,
                store_op: DontCare,
            }
        },
        pass: {
            color: [inter],
            color_resolve: [color],
            depth_stencil: {depth},
        },
    )
    .unwrap()
}

fn get_framebuffers(
    images: &[Arc<Image>],
    render_pass: Arc<RenderPass>,
    mamory_allocator: Arc<StandardMemoryAllocator>,
) -> Vec<Arc<Framebuffer>> {
    let depth_buffer = ImageView::new_default(
        Image::new(
            mamory_allocator.clone(),
            ImageCreateInfo {
                image_type: ImageType::Dim2d,
                format: Format::D32_SFLOAT,
                extent: images[0].extent(),
                usage: ImageUsage::DEPTH_STENCIL_ATTACHMENT | ImageUsage::TRANSIENT_ATTACHMENT,
                samples: SampleCount::Sample8,
                ..Default::default()
            },
            AllocationCreateInfo::default(),
        )
        .unwrap(),
    )
    .unwrap();


    images
        .iter()
        .map(|image| {
            let view = ImageView::new_default(
                image.clone()
            ).unwrap();

            let inter = ImageView::new_default(
                Image::new(
                    mamory_allocator.clone(), 
                    ImageCreateInfo {
                        image_type: ImageType::Dim2d,
                        format: image.format(),
                        extent: image.extent(),
                        usage: ImageUsage::COLOR_ATTACHMENT,
                        samples: SampleCount::Sample8,
                        ..Default::default()
                    },
                    AllocationCreateInfo::default()
                ).unwrap()
            ).unwrap();

            Framebuffer::new(
                render_pass.clone(),
                FramebufferCreateInfo {
                    attachments: vec![inter, view, depth_buffer.clone()],
                    ..Default::default()
                },
            )
            .unwrap()
        })
        .collect::<Vec<_>>()
}

fn get_pipeline(
    device: Arc<Device>,
    vs: Arc<ShaderModule>,
    fs: Arc<ShaderModule>,
    render_pass: Arc<RenderPass>,
    viewport: Viewport,
) -> Arc<GraphicsPipeline> {
    let vs = vs.entry_point("main").unwrap();
    let fs = fs.entry_point("main").unwrap();

    let vertex_input_state = VertexData::per_vertex()
        .definition(&vs.info().input_interface)
        .unwrap();

    let stages = [
        PipelineShaderStageCreateInfo::new(vs),
        PipelineShaderStageCreateInfo::new(fs),
    ];

    let layout = PipelineLayout::new(
        device.clone(),
        PipelineDescriptorSetLayoutCreateInfo::from_stages(&stages)
            .into_pipeline_layout_create_info(device.clone())
            .unwrap(),
    )
    .unwrap();

    let subpass = Subpass::from(render_pass.clone(), 0).unwrap();

    GraphicsPipeline::new(
        device.clone(),
        None,
        GraphicsPipelineCreateInfo {
            stages: stages.into_iter().collect(),
            vertex_input_state: Some(vertex_input_state),
            input_assembly_state: Some(InputAssemblyState::default()),
            viewport_state: Some(ViewportState {
                viewports: [viewport].into_iter().collect(),
                ..Default::default()
            }),
            rasterization_state: Some(RasterizationState::default()),
            depth_stencil_state: Some(DepthStencilState {
                depth: Some(DepthState::simple()),
                ..Default::default()
            }),
            multisample_state: Some(MultisampleState { rasterization_samples: SampleCount::Sample8, ..Default::default() }),
            color_blend_state: Some(ColorBlendState::with_attachment_states(
                subpass.num_color_attachments(),
                ColorBlendAttachmentState::default(),
            )),
            subpass: Some(subpass.into()),
            ..GraphicsPipelineCreateInfo::layout(layout)
        },
    )
    .unwrap()
}

fn get_command_buffers(
    command_buffer_allocator: &StandardCommandBufferAllocator,
    descriptor_set_allocator: &StandardDescriptorSetAllocator,
    queue: &Arc<Queue>,
    pipelines: &HashMap<(String, String), Arc<GraphicsPipeline>>,
    framebuffers: &[Arc<Framebuffer>],
    meshes: &Vec<Mesh>,
    vp_buffer: &UpdatableBuffer<VPData>,
    m_buffer: &Vec<UpdatableBuffer<ModelData>>,
) -> Vec<Arc<PrimaryAutoCommandBuffer>> {
    framebuffers
        .iter()
        .map(|framebuffer| {
            let mut builder = AutoCommandBufferBuilder::primary(
                command_buffer_allocator,
                queue.queue_family_index(),
                CommandBufferUsage::MultipleSubmit,
            )
            .unwrap();

            for buff in m_buffer.iter() {
                builder
                    .copy_buffer(
                        CopyBufferInfo::buffers(buff.staging_buffer.clone(),
                                                buff.main_buffer.clone())
                    )
                    .unwrap();
            }

            builder
                .copy_buffer(
                    CopyBufferInfo::buffers(vp_buffer.staging_buffer.clone(), 
                                            vp_buffer.main_buffer.clone())
                )   
                .unwrap()
                .begin_render_pass(
                    RenderPassBeginInfo {
                        clear_values: vec![Some([0.1, 0.1, 0.1, 1.0].into()), Some([0.1, 0.1, 0.1, 1.0].into()), Some(1f32.into())],
                        ..RenderPassBeginInfo::framebuffer(framebuffer.clone())
                    },
                    SubpassBeginInfo {
                        contents: SubpassContents::Inline,
                        ..Default::default()
                    },
                )
                .unwrap();

            for (i, mesh) in meshes.iter().enumerate() {

                let pipeline =  pipelines
                    .get(&(mesh.vertex.clone(), mesh.fragment.clone()))
                    .unwrap()
                    .clone();
                
                let vp_set = PersistentDescriptorSet::new(
                    descriptor_set_allocator, 
                    pipeline.layout().set_layouts().get(0).unwrap().clone(), 
                    [WriteDescriptorSet::buffer(0, vp_buffer.main_buffer.clone())], 
                    []).unwrap();

                let m_set = PersistentDescriptorSet::new(
                    descriptor_set_allocator, 
                    pipeline.layout().set_layouts().get(1).unwrap().clone(), 
                    [WriteDescriptorSet::buffer(0, m_buffer.get(i).unwrap().main_buffer.clone())], 
                    []).unwrap();


                builder
                    .bind_pipeline_graphics(pipeline.clone())
                    .unwrap()
                    .bind_descriptor_sets(PipelineBindPoint::Graphics, 
                                          pipeline.layout().clone(), 0, 
                                          (vp_set.clone(), m_set.clone()))
                    .unwrap()
                    .bind_vertex_buffers(0, mesh.buffer.clone().unwrap().clone())
                    .unwrap()
                    .draw(mesh.mesh.len() as u32, 1, 0, 0)
                    .unwrap();
            }

            builder.end_render_pass(Default::default()).unwrap();
            builder.build().unwrap()
        })
        .collect()
}

pub fn run(mut meshes: Vec<Mesh>, shaders: HashMap<String, ShaderData>) {
    let event_loop_ = EventLoop::new();
    let window_ = Window::new(&event_loop_); 
    let renderer: SimpleRenderer<_> = SimpleRenderer::<NowFuture>::new();
    renderer.init(&event_loop_, &window_);
    let shader_manager = ShaderManager::new();
    let mesh_manager = MeshManager::new();

    let standard_memory_allocator = Arc::new(StandardMemoryAllocator::new_default(renderer.device.clone().unwrap().clone()));

    for mesh in meshes.iter_mut() {
        let memory_allocator = Arc::new(StandardMemoryAllocator::new_default(renderer.device.clone().unwrap().clone()));
        mesh.buffer = Some(
            Buffer::from_iter(
                memory_allocator,
                BufferCreateInfo {
                    usage: BufferUsage::VERTEX_BUFFER,
                    ..Default::default()
                },
                AllocationCreateInfo {
                    memory_type_filter: MemoryTypeFilter::PREFER_DEVICE
                        | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                    ..Default::default()
                },
                mesh.mesh.clone(),
           )
            .unwrap(),
        );
        mesh_manager.list.push(*mesh);
    }

    let mut model_buffers: Vec<UpdatableBuffer<ModelData>> = Vec::new();
    for _ in (0..2).step_by(1) {
        model_buffers.push(
            UpdatableBuffer::new(&renderer.device.clone().unwrap().clone(), BufferUsage::UNIFORM_BUFFER)
        );
        model_buffers.last_mut().unwrap().write(ModelData { translation: Matrix4f::translation(Vec3f::new([0.0, 0.0, -1.0]))});
    }

    let mut vp_data = VPData {
        view: Matrix4f::look_at(
                  Vec3f::new([0.0, 0.0, 0.0]), 
                  Vec3f::new([0.0, 0.0, 1.0]), 
                  Vec3f::new([0.0, 1.0, 0.0])),
        projection: Matrix4f::perspective((60.0_f32).to_radians(), 1.0, 0.1, 10.0)
    };
    let mut vp_buffer: UpdatableBuffer<VPData> = 
        UpdatableBuffer::new(&renderer.device.clone().unwrap().clone(), BufferUsage::UNIFORM_BUFFER);
    vp_buffer.write(vp_data);

    let vertex_shaders: Vec<(&String, &ShaderData)> = shaders
        .iter()
        .filter(|shader_data| matches!(shader_data.1.shader_type, ShaderType::Vertex))
        .collect();

    let fragment_shaders: Vec<(&String, &ShaderData)> = shaders
        .iter()
        .filter(|shader_data| matches!(shader_data.1.shader_type, ShaderType::Fragment))
        .collect();

    for (shader, _) in shaders.iter() {
        shader_manager.library.insert(
            shader.to_string(),
            Shader { shader: load_shader_module(&shaders, &renderer.device.clone().unwrap(), shader) },
        );
    }

    for (name_vert, _) in vertex_shaders.iter() {
        for (name_frag, _) in fragment_shaders.iter() {
            shader_manager.pipelines.insert(
                (name_vert.to_string(), name_frag.to_string()),
                renderer.get_pipeline(
                    shader_manager.library.get(*name_vert).unwrap().clone(),
                    shader_manager.library.get(*name_frag).unwrap().clone(),
                ),
            );
        }
    }

    let descriptor_set_allocator = StandardDescriptorSetAllocator::new(renderer.device.clone().unwrap().clone(), Default::default());
    let command_buffer_allocator = StandardCommandBufferAllocator::new(renderer.device.clone().unwrap().clone(), Default::default());

    renderer.update_command_buffers(&mesh_manager, &shader_manager, &model_buffers, &vp_buffer);

    let mut dbg: f32 = 0.0;

    event_loop_.event_loop.run(move |event, _, control_flow| match event {
        Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } => {
            *control_flow = ControlFlow::Exit;
        }
        Event::WindowEvent {
            event: WindowEvent::Resized(_),
            ..
        } => {
            renderer.window_resized = true;
        }
        Event::MainEventsCleared => {
            if renderer.window_resized || renderer.recreate_swapchain {
                renderer.recreate_swapchain = false;

                let new_dimensions = window_.window_handle.inner_size();

                let (new_swapchain, new_images) = swapchain
                    .recreate(SwapchainCreateInfo {
                        image_extent: new_dimensions.into(),
                        ..swapchain.create_info()
                    })
                    .expect("failed to recreate swapchain");

                swapchain = new_swapchain;
                let new_framebuffers = get_framebuffers(
                    &new_images,
                    render_pass.clone(),
                    standard_memory_allocator.clone(),
                );

                if window_resized {
                    window_resized = false;
                    
                    vp_data.projection = Matrix4f::perspective((60.0_f32).to_radians(), (new_dimensions.width as f32) / (new_dimensions.height as f32), 0.1, 10.0);
                    viewport.extent = new_dimensions.into();
                    for (pipeline, val) in pipelines.iter_mut() {
                        *val = get_pipeline(
                            device.clone(),
                            loaded_shaders.get(&pipeline.0).unwrap().clone(),
                            loaded_shaders.get(&pipeline.1).unwrap().clone(),
                            render_pass.clone(),
                            viewport.clone(),
                        )
                    }
                    command_buffers = get_command_buffers(
                        &command_buffer_allocator,
                        &descriptor_set_allocator,
                        &queue,
                        &pipelines,
                        &new_framebuffers,
                        &meshes,
                        &vp_buffer,
                        &model_buffers,
                    );
                }
            }

            let (image_i, suboptimal, acquire_future) =
                match swapchain::acquire_next_image(swapchain.clone(), None)
                    .map_err(Validated::unwrap)
                {
                    Ok(r) => r,
                    Err(VulkanError::OutOfDate) => {
                        recreate_swapchain = true;
                        return;
                    }
                    Err(e) => panic!("failed to acquire next image: {e}"),
                };

            if suboptimal {
                recreate_swapchain = true;
            }

            // wait for the fence related to this image to finish (normally this would be the oldest fence)
            if let Some(image_fence) = &fences[image_i as usize] {
                image_fence.wait(None).unwrap();
            }

            let previous_future = match fences[previous_fence_i as usize].clone() {
                // Create a NowFuture
                None => {
                    let mut now = sync::now(device.clone());
                    now.cleanup_finished();

                    now.boxed()
                }
                // Use the existing FenceSignalFuture
                Some(fence) => fence.boxed(),
            };
            
            // Waiting for all fences to be able to write to buffers
            for fence in fences.iter_mut() {
                let _ = match fence.as_mut() {
                    Some(val) => val.wait(None).unwrap(),
                    _ => (), 
                };
            }

            vp_data.view = Matrix4f::look_at(Vec3f::new([0.0, 0.0, 0.0]), Vec3f::new([(dbg/5.0).sin(), 0.0, (dbg/5.0).cos()]), Vec3f::new([0.0, 1.0, 0.0]));
            model_buffers.get_mut(0).unwrap().write(
                ModelData { translation: Matrix4f::translation(Vec3f::new([0.0, 0.0, -5.0])) }
                );
            model_buffers.get_mut(1).unwrap().write(
                ModelData { translation: Matrix4f::translation(Vec3f::new([0.0, 0.0, 5.0])) }
                );
            vp_buffer.write(vp_data);
            dbg += 0.01;

            let future = previous_future
                .join(acquire_future)
                .then_execute(queue.clone(), command_buffers[image_i as usize].clone())
                .unwrap()
                .then_swapchain_present(
                    queue.clone(),
                    SwapchainPresentInfo::swapchain_image_index(swapchain.clone(), image_i),
                )
                .then_signal_fence_and_flush();

            fences[image_i as usize] = match future.map_err(Validated::unwrap) {
                Ok(value) => Some(Arc::new(value)),
                Err(VulkanError::OutOfDate) => {
                    recreate_swapchain = true;
                    None
                }
                Err(e) => {
                    println!("failed to flush future: {e}");
                    None
                }
            };
            previous_fence_i = image_i;
        }
        _ => (),
    });
}
