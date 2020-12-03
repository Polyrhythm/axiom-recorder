use crate::{
    pipeline_processing::{
        parametrizable::{
            ParameterType::{IntRange, StringParameter},
            ParameterTypeDescriptor::Mandatory,
            Parameterizable,
            Parameters,
            ParametersDescriptor,
        },
        processing_node::{Payload, ProcessingNode},
    },
};
use anyhow::Result;
use std::{io::Read, net::TcpStream, sync::Mutex};
use crate::frame::raw_frame::RawFrame;

pub struct TcpReader {
    pub tcp_connection: Mutex<TcpStream>,
    pub width: u64,
    pub height: u64,
    pub bit_depth: u64,
}
impl Parameterizable for TcpReader {
    fn describe_parameters() -> ParametersDescriptor {
        ParametersDescriptor::new()
            .with("address", Mandatory(StringParameter))
            .with("width", Mandatory(IntRange(0, i64::max_value())))
            .with("height", Mandatory(IntRange(0, i64::max_value())))
            .with("bit-depth", Mandatory(IntRange(8, 16)))
    }

    fn from_parameters(parameters: &Parameters) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(Self {
            tcp_connection: Mutex::new(TcpStream::connect(parameters.get::<String>("address")?)?),
            width: parameters.get::<u64>("width")?,
            height: parameters.get::<u64>("height")?,
            bit_depth: parameters.get::<u64>("bit-depth")?,
        })
    }
}
impl ProcessingNode for TcpReader {
    fn process(&self, _input: &mut Payload) -> Result<Option<Payload>> {
        let mut bytes = vec![0u8; (self.width * self.height * self.bit_depth / 8) as usize];
        self.tcp_connection.lock().unwrap().read_exact(&mut bytes)?;
        Ok(Some(Payload::from(RawFrame::from_byte_vec(bytes, self.width, self.height, self.bit_depth)?)))
    }
}
