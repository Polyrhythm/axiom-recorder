use crate::pipeline_processing::{
    node::InputProcessingNode,
    parametrizable::{Parameterizable, Parameters, ParametersDescriptor},
    payload::Payload,
};
use anyhow::{Context, Result};


use crate::pipeline_processing::{
    frame::{Frame, FrameInterpretation, SampleInterpretation},
    node::{Caps, NodeID, ProcessingNode, Request},
    parametrizable::prelude::*,
    processing_context::ProcessingContext,
};
use async_trait::async_trait;

pub struct BitDepthConverter {
    input: InputProcessingNode,
    context: ProcessingContext,
}
impl Parameterizable for BitDepthConverter {
    fn describe_parameters() -> ParametersDescriptor {
        ParametersDescriptor::new().with("input", Mandatory(NodeInputParameter))
    }

    fn from_parameters(
        mut parameters: Parameters,
        _is_input_to: &[NodeID],
        context: &ProcessingContext,
    ) -> Result<Self> {
        Ok(Self { input: parameters.take("input")?, context: context.clone() })
    }
}

#[async_trait]
impl ProcessingNode for BitDepthConverter {
    async fn pull(&self, request: Request) -> Result<Payload> {
        let input = self.input.pull(request).await?;
        let frame = self
            .context
            .ensure_cpu_buffer_frame(&input)
            .context("Wrong input format for BitDepthConverter")?;
        let interpretation = FrameInterpretation {
            sample_interpretation: SampleInterpretation::UInt(8),
            ..frame.interpretation.clone()
        };
        let mut new_buffer =
            unsafe { self.context.get_uninit_cpu_buffer(interpretation.required_bytes()) };

        if let SampleInterpretation::UInt(8) = frame.interpretation.sample_interpretation {
            return Ok(input);
        } else if let SampleInterpretation::UInt(12) = frame.interpretation.sample_interpretation {
            new_buffer.as_mut_slice(|new_buffer| {
                frame.storage.as_slice(|frame_storage| {
                    for (input, output) in
                        frame_storage.chunks_exact(3).zip(new_buffer.chunks_exact_mut(2))
                    {
                        output[0] = input[0];
                        output[1] = (input[1] << 4) | (input[2] >> 4);
                    }
                })
            });
        } else if let SampleInterpretation::UInt(bits) = frame.interpretation.sample_interpretation
        {
            let mut rest_value: u32 = 0;
            let mut rest_bits: u32 = 0;
            let mut pos = 0;
            new_buffer.as_mut_slice(|new_buffer| {
                frame.storage.as_slice(|frame_storage| {
                    for value in frame_storage.iter() {
                        let bits_more_than_bit_depth = (rest_bits as i32 + 8) - bits as i32;
                        if bits_more_than_bit_depth >= 0 {
                            let new_n_bit_value: u32 = rest_value
                                .wrapping_shl(bits as u32 - rest_bits)
                                | value.wrapping_shr(8 - bits_more_than_bit_depth as u32) as u32;
                            new_buffer[pos] = (if bits > 8 {
                                new_n_bit_value.wrapping_shr(bits as u32 - 8)
                            } else {
                                new_n_bit_value
                            } as u8);
                            pos += 1;

                            rest_bits = bits_more_than_bit_depth as u32;
                            rest_value = (value & (2u32.pow(rest_bits as u32) - 1) as u8) as u32
                        } else {
                            rest_bits += 8;
                            rest_value = (rest_value << 8) | *value as u32;
                        };
                    }
                })
            });
        }

        let new_frame = Frame { storage: new_buffer, interpretation };

        Ok(Payload::from(new_frame))
    }

    fn get_caps(&self) -> Caps { self.input.get_caps() }
}
