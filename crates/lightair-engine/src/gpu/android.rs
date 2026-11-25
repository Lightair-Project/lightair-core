//! Android's zero-copy camera import: the camera's
//! `AHardwareBuffer` imported into Vulkan as it is (its own, often driver-private, Y′CbCr format
//! through a `VkSamplerYcbcrConversion`) and drawn upright into an RGBA texture by a small raw
//! Vulkan pass recorded inside wgpu's own command encoder. Needs a Vulkan device with
//! `VK_ANDROID_external_memory_android_hardware_buffer`, `VK_EXT_queue_family_foreign` and
//! sampler Y′CbCr conversion, which the engine makes itself ([`make_device`]) and gives to
//! lightair-render; anywhere else (GLES, the emulator without those) the plane copy stays.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use ash::vk;
use wgpu::hal::api::Vulkan;

const AHB_EXT: &std::ffi::CStr = ash::android::external_memory_android_hardware_buffer::NAME;
const FOREIGN_EXT: &std::ffi::CStr = ash::ext::queue_family_foreign::NAME;

/// Makes the shared device with what the import needs, when the adapter has it (Vulkan, both
/// extensions, Y′CbCr sampling: wgpu turns that on with `TEXTURE_FORMAT_NV12`), and gives it to
/// lightair-render. Before anything else makes the device. Whether the import is on.
pub fn make_device() -> bool {
    let Ok((_, adapter)) = lightair_render::compose::shared_adapter() else {
        return false;
    };
    if adapter.get_info().backend != wgpu::Backend::Vulkan
        || !adapter.features().contains(wgpu::Features::TEXTURE_FORMAT_NV12)
    {
        return false;
    }
    let features = wgpu::Features::TEXTURE_FORMAT_NV12;
    let limits = adapter.limits();
    let open = {
        let Some(hal) = (unsafe { adapter.as_hal::<Vulkan>() }) else {
            return false;
        };
        let instance = hal.shared_instance().raw_instance();
        let Ok(exts) = (unsafe { instance.enumerate_device_extension_properties(hal.raw_physical_device()) }) else {
            return false;
        };
        let has = |name: &std::ffi::CStr| exts.iter().any(|e| e.extension_name_as_c_str() == Ok(name));
        if !has(AHB_EXT) || !has(FOREIGN_EXT) {
            return false;
        }
        let callback = Box::new(|args: wgpu::hal::vulkan::CreateDeviceCallbackArgs| {
            for e in [AHB_EXT, FOREIGN_EXT] {
                if !args.extensions.contains(&e) {
                    args.extensions.push(e);
                }
            }
        });
        match unsafe { hal.open_with_callback(features, &limits, &Default::default(), Some(callback)) } {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("no Vulkan device for the zero-copy camera import: {e}");
                return false;
            }
        }
    };
    let desc = wgpu::DeviceDescriptor { required_features: features, required_limits: limits, ..Default::default() };
    match unsafe { adapter.create_device_from_hal::<Vulkan>(open, &desc) } {
        Ok((device, queue)) => lightair_render::compose::set_device(device, queue),
        Err(e) => {
            tracing::warn!("the zero-copy camera import's device: {e}");
            false
        }
    }
}

/// A frame's buffer, held (acquired) while it's drawn; given back to the host (its token, so
/// it closes the `Image`) once nothing holds it.
pub struct Held {
    pub buffer: *mut ndk_sys::AHardwareBuffer,
    pub token: i64,
    pub released: Arc<parking_lot::Mutex<Vec<i64>>>,
}

unsafe impl Send for Held {}
unsafe impl Sync for Held {}

impl Drop for Held {
    fn drop(&mut self) {
        unsafe { ndk_sys::AHardwareBuffer_release(self.buffer) };
        self.released.lock().push(self.token);
    }
}

/// What one Y′CbCr format (external or not) is drawn with.
struct Conversion {
    conversion: vk::SamplerYcbcrConversion,
    sampler: vk::Sampler,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    pool: vk::DescriptorPool,
}

/// A frame imported, kept until the GPU is surely done with it.
struct InFlight {
    frame: u64,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    set: (vk::DescriptorPool, vk::DescriptorSet),
    _held: Arc<Held>,
}

/// A buffer imported: (image, memory, view, size, its conversion).
type Imported<'a> = (vk::Image, vk::DeviceMemory, vk::ImageView, (u32, u32), &'a Conversion);

/// Frames are freed this many draws later.
const KEEP: u64 = 6;

pub struct Importer {
    device: ash::Device,
    ahb: ash::android::external_memory_android_hardware_buffer::Device,
    family: u32,
    render_pass: vk::RenderPass,
    vertex: vk::ShaderModule,
    fragment: vk::ShaderModule,
    conversions: HashMap<(u64, i32), Conversion>,
    /// The RGBA targets' views and framebuffers, by their image.
    targets: HashMap<u64, (vk::ImageView, vk::Framebuffer, (u32, u32))>,
    in_flight: VecDeque<InFlight>,
    frame: u64,
}

unsafe impl Send for Importer {}

impl Importer {
    pub fn new(device: &wgpu::Device) -> Option<Importer> {
        let hal = unsafe { device.as_hal::<Vulkan>() }?;
        let raw = hal.raw_device().clone();
        let instance = hal.shared_instance().raw_instance();
        let ahb = ash::android::external_memory_android_hardware_buffer::Device::new(instance, &raw);
        let family = hal.queue_family_index();
        let attachment = vk::AttachmentDescription::default()
            .format(vk::Format::R8G8B8A8_UNORM)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::DONT_CARE)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .final_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
        let colour =
            [vk::AttachmentReference::default().attachment(0).layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let subpass = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&colour)];
        let attachments = [attachment];
        let render_pass = unsafe {
            raw.create_render_pass(
                &vk::RenderPassCreateInfo::default().attachments(&attachments).subpasses(&subpass),
                None,
            )
        }
        .ok()?;
        let module = |words: &[u32]| unsafe {
            raw.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(words), None)
        };
        let vertex = module(&super::spirv::vertex()).ok()?;
        let fragment = module(&super::spirv::fragment()).ok()?;
        Some(Importer {
            device: raw,
            ahb,
            family,
            render_pass,
            vertex,
            fragment,
            conversions: HashMap::new(),
            targets: HashMap::new(),
            in_flight: VecDeque::new(),
            frame: 0,
        })
    }

    fn conversion(&mut self, props: &vk::AndroidHardwareBufferFormatPropertiesANDROID) -> Option<&Conversion> {
        let key = (props.external_format, props.format.as_raw());
        if !self.conversions.contains_key(&key) {
            let c = unsafe { self.make_conversion(props) }?;
            self.conversions.insert(key, c);
        }
        self.conversions.get(&key)
    }

    unsafe fn make_conversion(&self, props: &vk::AndroidHardwareBufferFormatPropertiesANDROID) -> Option<Conversion> {
        let d = &self.device;
        // once per format: what the driver suggests (the colour check)
        tracing::info!(
            external_format = props.external_format,
            format = ?props.format,
            model = ?props.suggested_ycbcr_model,
            range = ?props.suggested_ycbcr_range,
            components = ?props.sampler_ycbcr_conversion_components,
            x_chroma = ?props.suggested_x_chroma_offset,
            y_chroma = ?props.suggested_y_chroma_offset,
            features = ?props.format_features,
            "zero-copy camera conversion"
        );
        let linear =
            props.format_features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE_YCBCR_CONVERSION_LINEAR_FILTER);
        let filter = if linear { vk::Filter::LINEAR } else { vk::Filter::NEAREST };
        let mut external = vk::ExternalFormatANDROID::default().external_format(props.external_format);
        let format = if props.external_format != 0 { vk::Format::UNDEFINED } else { props.format };
        let mut info = vk::SamplerYcbcrConversionCreateInfo::default()
            .format(format)
            .ycbcr_model(props.suggested_ycbcr_model)
            .ycbcr_range(props.suggested_ycbcr_range)
            .components(props.sampler_ycbcr_conversion_components)
            .x_chroma_offset(props.suggested_x_chroma_offset)
            .y_chroma_offset(props.suggested_y_chroma_offset)
            .chroma_filter(filter)
            .force_explicit_reconstruction(false);
        if props.external_format != 0 {
            info = info.push_next(&mut external);
        }
        let conversion = unsafe { d.create_sampler_ycbcr_conversion(&info, None) }.ok()?;
        let mut with = vk::SamplerYcbcrConversionInfo::default().conversion(conversion);
        let sampler = unsafe {
            d.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(filter)
                    .min_filter(filter)
                    .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .push_next(&mut with),
                None,
            )
        }
        .ok()?;
        let samplers = [sampler];
        let binding = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .immutable_samplers(&samplers)];
        let set_layout = unsafe {
            d.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&binding), None)
        }
        .ok()?;
        let layouts = [set_layout];
        let push = [vk::PushConstantRange { stage_flags: vk::ShaderStageFlags::VERTEX, offset: 0, size: 32 }];
        let layout = unsafe {
            d.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts).push_constant_ranges(&push),
                None,
            )
        }
        .ok()?;
        let name = c"main";
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(self.vertex)
                .name(name),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(self.fragment)
                .name(name),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let assembly =
            vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample =
            vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blend_attachment =
            [vk::PipelineColorBlendAttachmentState::default().color_write_mask(vk::ColorComponentFlags::RGBA)];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachment);
        let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic_state)
            .layout(layout)
            .render_pass(self.render_pass)
            .subpass(0);
        let pipeline = unsafe { d.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) }.ok()?.remove(0);
        // a Y′CbCr image can take several descriptors each: room for plenty
        let sizes = [vk::DescriptorPoolSize { ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER, descriptor_count: 64 }];
        let pool = unsafe {
            d.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
                    .max_sets(16)
                    .pool_sizes(&sizes),
                None,
            )
        }
        .ok()?;
        Some(Conversion { conversion, sampler, set_layout, layout, pipeline, pool })
    }

    /// The camera frame `held` drawn upright (turned `rotation` clockwise degrees from as
    /// captured, `mirrored` after) into `target` (RGBA, `RENDER_ATTACHMENT | TEXTURE_BINDING`, of
    /// the upright size). False if it couldn't be (the caller falls back for this frame).
    pub fn draw(
        &mut self,
        wgpu_device: &wgpu::Device,
        queue: &wgpu::Queue,
        held: Arc<Held>,
        rotation: u32,
        mirrored: bool,
        target: &wgpu::Texture,
    ) -> bool {
        self.frame += 1;
        self.free_done();
        let Some((image, memory, view, size, conv)) = (unsafe { self.import(held.buffer) }) else {
            return false;
        };
        let (pool, set_layout, layout, pipeline) = (conv.pool, conv.set_layout, conv.layout, conv.pipeline);
        let d = self.device.clone();
        let render_pass = self.render_pass;
        let layouts = [set_layout];
        let Ok(sets) = (unsafe {
            d.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default().descriptor_pool(pool).set_layouts(&layouts),
            )
        }) else {
            unsafe { self.destroy(image, memory, view) };
            return false;
        };
        let set = sets[0];
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        unsafe {
            d.update_descriptor_sets(
                &[vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&image_info)],
                &[],
            )
        };
        let Some(target_raw) = (unsafe { target.as_hal::<Vulkan>() }).map(|t| unsafe { t.raw_handle() }) else {
            return false;
        };
        let (tw, th) = (target.width(), target.height());
        let Some(framebuffer) = self.framebuffer(target_raw, (tw, th)) else {
            return false;
        };
        let _ = size;
        // a raw-only encoder (wgpu forbids mixing): the target goes from what wgpu left it as (a
        // sampled texture: `Gpu::prime` makes sure of that) to a colour attachment and back, so
        // wgpu's own tracking of it stays true
        let mut encoder = wgpu_device.create_command_encoder(&Default::default());
        let push = super::spirv::transform(rotation, mirrored);
        let push_bytes: Vec<u8> = push.iter().flat_map(|v| v.to_le_bytes()).collect();
        let family = self.family;
        let recorded = unsafe {
            encoder.as_hal_mut::<Vulkan, _, _>(|e| {
                let Some(e) = e else { return false };
                let cmd = e.raw_handle();
                if cmd == vk::CommandBuffer::null() {
                    return false;
                }
                let colour = vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                };
                let to_attachment = vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::SHADER_READ)
                    .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                    .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(target_raw)
                    .subresource_range(colour);
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_attachment],
                );
                // the camera's buffer, from the foreign producer, to be sampled
                let acquire = vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::empty())
                    .dst_access_mask(vk::AccessFlags::SHADER_READ)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                    .dst_queue_family_index(family)
                    .image(image)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    });
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[acquire],
                );
                let area =
                    vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: vk::Extent2D { width: tw, height: th } };
                d.cmd_begin_render_pass(
                    cmd,
                    &vk::RenderPassBeginInfo::default()
                        .render_pass(render_pass)
                        .framebuffer(framebuffer)
                        .render_area(area),
                    vk::SubpassContents::INLINE,
                );
                d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
                d.cmd_set_viewport(
                    cmd,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: tw as f32,
                        height: th as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                d.cmd_set_scissor(cmd, 0, &[area]);
                d.cmd_bind_descriptor_sets(cmd, vk::PipelineBindPoint::GRAPHICS, layout, 0, &[set], &[]);
                d.cmd_push_constants(cmd, layout, vk::ShaderStageFlags::VERTEX, 0, &push_bytes);
                d.cmd_draw(cmd, 3, 1, 0, 0);
                d.cmd_end_render_pass(cmd);
                // the target back to a sampled texture (as wgpu tracks it), the camera's buffer
                // back to its producer
                let to_sampled = vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ)
                    .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(target_raw)
                    .subresource_range(colour);
                let give_back = vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::SHADER_READ)
                    .dst_access_mask(vk::AccessFlags::empty())
                    .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .src_queue_family_index(family)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                    .image(image)
                    .subresource_range(colour);
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT | vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER | vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_sampled, give_back],
                );
                true
            })
        };
        queue.submit([encoder.finish()]);
        self.in_flight.push_back(InFlight { frame: self.frame, image, memory, view, set: (pool, set), _held: held });
        recorded
    }

    /// The buffer as a Vulkan image with its conversion: (image, memory, view, size, conversion).
    unsafe fn import(&mut self, buffer: *mut ndk_sys::AHardwareBuffer) -> Option<Imported<'_>> {
        let mut format = vk::AndroidHardwareBufferFormatPropertiesANDROID::default();
        let mut props = vk::AndroidHardwareBufferPropertiesANDROID::default().push_next(&mut format);
        unsafe { self.ahb.get_android_hardware_buffer_properties(buffer.cast(), &mut props) }.ok()?;
        let (allocation_size, memory_bits) = (props.allocation_size, props.memory_type_bits);
        let format = vk::AndroidHardwareBufferFormatPropertiesANDROID { p_next: std::ptr::null_mut(), ..format };
        let mut desc = ndk_sys::AHardwareBuffer_Desc {
            width: 0,
            height: 0,
            layers: 0,
            format: 0,
            usage: 0,
            stride: 0,
            rfu0: 0,
            rfu1: 0,
        };
        unsafe { ndk_sys::AHardwareBuffer_describe(buffer, &mut desc) };
        let (w, h) = (desc.width, desc.height);
        let mut external_memory = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::ANDROID_HARDWARE_BUFFER_ANDROID);
        let mut external_format = vk::ExternalFormatANDROID::default().external_format(format.external_format);
        let vk_format = if format.external_format != 0 { vk::Format::UNDEFINED } else { format.format };
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D { width: w, height: h, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_memory)
            .push_next(&mut external_format);
        let d = self.device.clone();
        let image = unsafe { d.create_image(&info, None) }.ok()?;
        let mut import = vk::ImportAndroidHardwareBufferInfoANDROID::default().buffer(buffer.cast());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let memory_type = memory_bits.trailing_zeros();
        let memory = match unsafe {
            d.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(allocation_size)
                    .memory_type_index(memory_type)
                    .push_next(&mut import)
                    .push_next(&mut dedicated),
                None,
            )
        } {
            Ok(m) => m,
            Err(_) => {
                unsafe { d.destroy_image(image, None) };
                return None;
            }
        };
        if unsafe { d.bind_image_memory(image, memory, 0) }.is_err() {
            unsafe {
                d.destroy_image(image, None);
                d.free_memory(memory, None);
            }
            return None;
        }
        let conv = match self.conversion(&format) {
            Some(c) => c.conversion,
            None => {
                unsafe {
                    d.destroy_image(image, None);
                    d.free_memory(memory, None);
                }
                return None;
            }
        };
        let mut with = vk::SamplerYcbcrConversionInfo::default().conversion(conv);
        let view = match unsafe {
            d.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk_format)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    })
                    .push_next(&mut with),
                None,
            )
        } {
            Ok(v) => v,
            Err(_) => {
                unsafe {
                    d.destroy_image(image, None);
                    d.free_memory(memory, None);
                }
                return None;
            }
        };
        let key = (format.external_format, format.format.as_raw());
        let conv = self.conversions.get(&key)?;
        Some((image, memory, view, (w, h), conv))
    }

    fn framebuffer(&mut self, image: vk::Image, size: (u32, u32)) -> Option<vk::Framebuffer> {
        use ash::vk::Handle;
        let key = image.as_raw();
        if let Some((_, fb, s)) = self.targets.get(&key)
            && *s == size
        {
            return Some(*fb);
        }
        let d = &self.device;
        let view = unsafe {
            d.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
        }
        .ok()?;
        let views = [view];
        let fb = unsafe {
            d.create_framebuffer(
                &vk::FramebufferCreateInfo::default()
                    .render_pass(self.render_pass)
                    .attachments(&views)
                    .width(size.0)
                    .height(size.1)
                    .layers(1),
                None,
            )
        }
        .ok()?;
        if let Some((v, f, _)) = self.targets.insert(key, (view, fb, size)) {
            // a target of the same image at another size: the old one is surely done with
            unsafe {
                d.destroy_framebuffer(f, None);
                d.destroy_image_view(v, None);
            }
        }
        Some(fb)
    }

    unsafe fn destroy(&self, image: vk::Image, memory: vk::DeviceMemory, view: vk::ImageView) {
        let d = &self.device;
        unsafe {
            d.destroy_image_view(view, None);
            d.destroy_image(image, None);
            d.free_memory(memory, None);
        }
    }

    /// Frames drawn long enough ago for the GPU to be done: freed, their buffers given back.
    fn free_done(&mut self) {
        while self.in_flight.front().is_some_and(|f| f.frame + KEEP <= self.frame) {
            let Some(f) = self.in_flight.pop_front() else {
                break;
            };
            unsafe {
                let _ = self.device.free_descriptor_sets(f.set.0, &[f.set.1]);
                self.destroy(f.image, f.memory, f.view);
            }
        }
    }
}

impl Drop for Importer {
    fn drop(&mut self) {
        let d = &self.device;
        unsafe {
            let _ = d.device_wait_idle();
            for f in std::mem::take(&mut self.in_flight) {
                let _ = d.free_descriptor_sets(f.set.0, &[f.set.1]);
                self.destroy(f.image, f.memory, f.view);
            }
            for (_, (v, f, _)) in self.targets.drain() {
                d.destroy_framebuffer(f, None);
                d.destroy_image_view(v, None);
            }
            for (_, c) in self.conversions.drain() {
                d.destroy_pipeline(c.pipeline, None);
                d.destroy_pipeline_layout(c.layout, None);
                d.destroy_descriptor_pool(c.pool, None);
                d.destroy_descriptor_set_layout(c.set_layout, None);
                d.destroy_sampler(c.sampler, None);
                d.destroy_sampler_ycbcr_conversion(c.conversion, None);
            }
            d.destroy_shader_module(self.vertex, None);
            d.destroy_shader_module(self.fragment, None);
            d.destroy_render_pass(self.render_pass, None);
        }
    }
}
