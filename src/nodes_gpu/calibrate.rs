use crate::pipeline_processing::{
    buffers::GpuBuffer,
    frame::Frame,
    gpu_util::ensure_gpu_buffer_frame,
    node::{Caps, InputProcessingNode, NodeID, ProcessingNode, Request},
    parametrizable::prelude::*,
    payload::Payload,
    processing_context::ProcessingContext,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::sync::Arc;
use vulkano::{
    buffer::{BufferUsage, DeviceLocalBuffer},
    command_buffer::{AutoCommandBufferBuilder, CommandBufferUsage::OneTimeSubmit},
    descriptor_set::{persistent::PersistentDescriptorSet, WriteDescriptorSet},
    device::{Device, Queue},
    image::{view::ImageView, ImageViewAbstract, ImmutableImage},
    pipeline::{ComputePipeline, Pipeline, PipelineBindPoint},
    sampler::{Filter, Sampler, SamplerAddressMode, SamplerCreateInfo},
    sync::GpuFuture,
    DeviceSize,
};

// generated by the macro
#[allow(clippy::needless_question_mark)]
mod compute_shader {
    vulkano_shaders::shader! {
        ty: "compute",
        path: "src/nodes_gpu/calibrate.glsl"
    }
}

pub struct Calibrate {
    device: Arc<Device>,
    pipeline: Arc<ComputePipeline>,
    queue: Arc<Queue>,
    input: InputProcessingNode,
    darkframe_view: Arc<dyn ImageViewAbstract>,
    darkframe_sampler: Arc<Sampler>,
}

impl Parameterizable for Calibrate {
    fn describe_parameters() -> ParametersDescriptor {
        ParametersDescriptor::new()
            .with("input", Mandatory(NodeInputParameter))
            .with("darkframe", Mandatory(StringParameter))
            .with("width", Mandatory(IntRange(0, i64::MAX)))
            .with("height", Mandatory(IntRange(0, i64::MAX)))
    }

    fn from_parameters(
        mut parameters: Parameters,
        _is_input_to: &[NodeID],
        context: &ProcessingContext,
    ) -> Result<Self>
    where
        Self: Sized,
    {
        let (device, queues) = context.require_vulkan()?;
        let queue = queues.iter().find(|&q| q.family().supports_compute()).unwrap().clone();

        let shader = compute_shader::load(device.clone()).unwrap();
        let pipeline = ComputePipeline::new(
            device.clone(),
            shader.entry_point("main").unwrap(),
            &(),
            None,
            |_| {},
        )
        .unwrap();

        let darkframe = std::fs::read(parameters.take::<String>("darkframe")?)?;

        let (darkframe_image, _image_fut) = ImmutableImage::from_iter(
            darkframe.into_iter(),
            vulkano::image::ImageDimensions::Dim2d {
                width: parameters.take::<i64>("width")? as u32,
                height: parameters.take::<i64>("height")? as u32,
                array_layers: 1,
            },
            vulkano::image::MipmapsCount::One,
            vulkano::format::Format::R32_SFLOAT,
            queue.clone(),
        )?;

        let darkframe_sampler = Sampler::new(
            device.clone(),
            SamplerCreateInfo {
                mag_filter: Filter::Linear,
                min_filter: Filter::Linear,
                address_mode: [
                    SamplerAddressMode::Repeat,
                    SamplerAddressMode::Repeat,
                    SamplerAddressMode::Repeat,
                ],
                ..Default::default()
            },
        )
        .unwrap();

        Ok(Calibrate {
            device,
            pipeline,
            queue,
            input: parameters.take("input")?,
            darkframe_view: ImageView::new_default(darkframe_image).unwrap(),
            darkframe_sampler,
        })
    }
}

#[async_trait]
impl ProcessingNode for Calibrate {
    async fn pull(&self, request: Request) -> Result<Payload> {
        let input = self.input.pull(request).await?;

        let (frame, fut) = ensure_gpu_buffer_frame(&input, self.queue.clone())
            .context("Wrong input format for Calibrate")?;

        let sink_buffer = DeviceLocalBuffer::<[u8]>::array(
            self.device.clone(),
            frame.interpretation.required_bytes() as DeviceSize,
            BufferUsage {
                storage_buffer: true,
                storage_texel_buffer: true,
                transfer_src: true,
                ..BufferUsage::none()
            },
            std::iter::once(self.queue.family()),
        )?;

        let push_constants = compute_shader::ty::PushConstantData {
            width: frame.interpretation.width as _,
            height: frame.interpretation.height as _,
        };

        let layout = self.pipeline.layout().set_layouts()[0].clone();
        let set = PersistentDescriptorSet::new(
            layout,
            [
                WriteDescriptorSet::buffer(0, frame.storage.untyped()),
                WriteDescriptorSet::buffer(1, sink_buffer.clone()),
                WriteDescriptorSet::image_view_sampler(
                    2,
                    self.darkframe_view.clone(),
                    self.darkframe_sampler.clone(),
                ),
            ],
        )
        .unwrap();

        let mut builder = AutoCommandBufferBuilder::primary(
            self.device.clone(),
            self.queue.family(),
            OneTimeSubmit,
        )
        .unwrap();
        builder
            .bind_descriptor_sets(
                PipelineBindPoint::Compute,
                self.pipeline.layout().clone(),
                0,
                set,
            )
            .push_constants(self.pipeline.layout().clone(), 0, push_constants)
            .bind_pipeline_compute(self.pipeline.clone())
            .dispatch([
                (frame.interpretation.width as u32 + 15) / 16,
                (frame.interpretation.height as u32 + 31) / 32,
                1,
            ])?;
        let command_buffer = builder.build()?;

        let future =
            fut.then_execute(self.queue.clone(), command_buffer)?.then_signal_fence_and_flush()?;

        future.wait(None).unwrap();
        Ok(Payload::from(Frame {
            interpretation: frame.interpretation.clone(),
            storage: GpuBuffer::from(sink_buffer),
        }))
    }

    fn get_caps(&self) -> Caps { self.input.get_caps() }
}
