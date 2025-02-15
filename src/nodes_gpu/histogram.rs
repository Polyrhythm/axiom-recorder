use crate::pipeline_processing::{
    buffers::GpuBuffer,
    frame::{Frame, FrameInterpretation, SampleInterpretation},
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
    command_buffer::{AutoCommandBufferBuilder, CommandBufferUsage::OneTimeSubmit, FillBufferInfo},
    descriptor_set::{persistent::PersistentDescriptorSet, WriteDescriptorSet},
    device::{Device, Queue},
    pipeline::{ComputePipeline, Pipeline, PipelineBindPoint},
    sync::GpuFuture,
};

// generated by the macro
#[allow(clippy::needless_question_mark)]
mod compute_shader {
    vulkano_shaders::shader! {
        ty: "compute",
        path: "src/nodes_gpu/histogram.glsl"
    }
}

pub struct Histogram {
    device: Arc<Device>,
    pipeline: Arc<ComputePipeline>,
    queue: Arc<Queue>,
    input: InputProcessingNode,
}

impl Parameterizable for Histogram {
    fn describe_parameters() -> ParametersDescriptor {
        ParametersDescriptor::new().with("input", Mandatory(NodeInputParameter))
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

        Ok(Histogram { device, pipeline, queue, input: parameters.take("input")? })
    }
}

#[async_trait]
impl ProcessingNode for Histogram {
    async fn pull(&self, request: Request) -> Result<Payload> {
        let input = self.input.pull(request).await?;

        let (frame, fut) = ensure_gpu_buffer_frame(&input, self.queue.clone())
            .context("Wrong input format for Histogram")?;


        let sink_buffer = DeviceLocalBuffer::<[u8]>::array(
            self.device.clone(),
            (1 << 8) * 4, // actually uint
            BufferUsage {
                storage_buffer: true,
                storage_texel_buffer: true,
                transfer_src: true,
                transfer_dst: true,
                vertex_buffer: true,
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
            .fill_buffer(FillBufferInfo {
                data: 0,
                ..FillBufferInfo::dst_buffer(sink_buffer.clone())
            })?
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
            interpretation: FrameInterpretation {
                width: 4096,
                height: 1,
                sample_interpretation: SampleInterpretation::FP32,
                ..frame.interpretation.clone()
            },
            storage: GpuBuffer::from(sink_buffer),
        }))
    }

    fn get_caps(&self) -> Caps { self.input.get_caps() }
}
