use crate::pipeline_processing::{
    buffers::{CpuBuffer, GpuBuffer},
    frame::Frame,
    payload::Payload,
    prioritized_executor::PrioritizedReactor,
};
use anyhow::{anyhow, Result};
use parking_lot::lock_api::RwLock;
use std::{future::Future, sync::Arc};
use vulkano::{
    buffer::{BufferAccess, BufferUsage, CpuAccessibleBuffer},
    command_buffer::{
        AutoCommandBufferBuilder,
        CommandBufferUsage,
        CopyBufferInfo,
        PrimaryCommandBuffer,
    },
    device::{
        physical::{PhysicalDevice, PhysicalDeviceType},
        Device,
        DeviceCreateInfo,
        DeviceExtensions,
        Queue,
        QueueCreateInfo,
    },
    instance::{
        debug::{
            DebugUtilsMessageSeverity,
            DebugUtilsMessageType,
            DebugUtilsMessenger,
            DebugUtilsMessengerCreateInfo,
        },
        Instance,
        InstanceCreateInfo,
    },
};
use vulkano_maybe_molten::NewMaybeMolten;


#[derive(Clone)]
struct VulkanContext {
    device: Arc<Device>,
    queues: Vec<Arc<Queue>>,
}

// [u8 output priority, u56 frame number]
#[derive(Default, Copy, Clone, Ord, Eq, PartialEq, PartialOrd, Debug)]
pub struct Priority(u64);

impl Priority {
    const MASK: u64 = 0x0fff_ffff_ffff_ffff;

    pub fn new(output_priority: u8, frame_number: u64) -> Self {
        Self(((output_priority as u64) << 56) | (frame_number & Self::MASK))
    }

    pub fn for_frame(self, frame_number: u64) -> Self {
        Self((self.0 & !Self::MASK) | (frame_number & Self::MASK))
    }

    pub fn get_frame(&self) -> u64 { self.0 & Self::MASK }
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let output_priority = self.0 >> 56;
        let frame_number = self.0 & Self::MASK;
        write!(f, "Priority(output = {}, frame = {})", output_priority, frame_number)
    }
}

#[derive(Clone)]
pub struct ProcessingContext {
    vulkan_device: Option<VulkanContext>,
    prioritized_reactor: PrioritizedReactor<Priority>,
    tokio_rt_handle: Arc<tokio::runtime::Runtime>,
}
impl Default for ProcessingContext {
    fn default() -> Self {
        let vk_device = Instance::new_maybe_molten(InstanceCreateInfo {
            enabled_extensions: vulkano_win::required_extensions(),
            ..Default::default()
        })
        .map_err(|e| eprintln!("error creating vulkan instance: {e}"))
        .ok()
        .and_then(|instance| {
            // Safety: callback must not make any calls to the Vulkan API
            unsafe {
                std::mem::forget(DebugUtilsMessenger::new(
                    instance.clone(),
                    DebugUtilsMessengerCreateInfo {
                        message_severity: DebugUtilsMessageSeverity::all(),
                        message_type: DebugUtilsMessageType::all(),

                        ..DebugUtilsMessengerCreateInfo::user_callback(Arc::new(|msg| {
                            println!(
                                "{}: {}",
                                msg.layer_prefix.unwrap_or("unknown"),
                                msg.description
                            )
                        }))
                    },
                ));
            }
            PhysicalDevice::enumerate(&instance).find_map(|physical| {
                if physical.properties().device_type == PhysicalDeviceType::Cpu {
                    return None;
                }

                let queue_family = physical.queue_families().map(QueueCreateInfo::family).collect();
                let khr_shader_non_semantic_info =
                    physical.supported_extensions().khr_shader_non_semantic_info;
                let device_ext = DeviceExtensions {
                    khr_swapchain: true,
                    khr_storage_buffer_storage_class: true,
                    khr_8bit_storage: true,
                    khr_shader_non_semantic_info,
                    ..DeviceExtensions::none()
                };
                Device::new(
                    physical,
                    DeviceCreateInfo {
                        enabled_extensions: device_ext,
                        enabled_features: physical.supported_features().clone(),
                        queue_create_infos: queue_family,
                        ..Default::default()
                    },
                )
                .ok()
            })
        });
        match vk_device {
            None => ProcessingContext::new(None),
            Some((device, queues)) => {
                ProcessingContext::new(Some(VulkanContext { device, queues: queues.collect() }))
            }
        }
    }
}
impl ProcessingContext {
    pub fn from_vk_device_queues(device: Arc<Device>, queues: Vec<Arc<Queue>>) -> Self {
        Self::new(Some(VulkanContext { device, queues }))
    }
    fn new(vulkan_context: Option<VulkanContext>) -> Self {
        let threads = std::env::var("RECORDER_NUM_THREADS")
            .map_err(|_| ())
            .and_then(|v| v.parse::<usize>().map_err(|_| ()))
            .unwrap_or_else(|_| num_cpus::get());
        println!("using {threads} threads");


        if let Some(vulkan_context) = &vulkan_context {
            println!(
                "using gpu: {}",
                vulkan_context.device.physical_device().properties().device_name
            );
        } else {
            println!("using cpu only processing");
        }


        Self {
            vulkan_device: vulkan_context,
            prioritized_reactor: PrioritizedReactor::start(threads),
            tokio_rt_handle: Arc::new(tokio::runtime::Runtime::new().unwrap()),
        }
    }

    /// # Safety
    /// Only safe if you initialize the memory
    pub unsafe fn get_uninit_cpu_buffer(&self, len: usize) -> CpuBuffer {
        if let Some(vulkan_context) = &self.vulkan_device {
            CpuAccessibleBuffer::uninitialized_array(
                vulkan_context.device.clone(),
                len as _,
                BufferUsage {
                    storage_buffer: true,
                    storage_texel_buffer: true,
                    transfer_src: true,
                    transfer_dst: true,
                    ..BufferUsage::none()
                },
                true,
            )
            .unwrap()
            .into()
        } else {
            let mut vec: Vec<u8> = Vec::with_capacity(len);
            unsafe {
                vec.set_len(len);
            }
            CpuBuffer::Vec(Arc::new(RwLock::new(vec)))
        }
    }
    fn to_cpu_buffer_frame(&self, frame: Arc<Frame<GpuBuffer>>) -> Result<Frame<CpuBuffer>> {
        let (device, queues) = self.require_vulkan()?;
        let queue =
            queues.iter().find(|&q| q.family().explicitly_supports_transfers()).unwrap().clone();

        let buffer = unsafe { self.get_uninit_cpu_buffer(frame.storage.untyped().size() as usize) };
        let mut cbb = AutoCommandBufferBuilder::primary(
            device,
            queue.family(),
            CommandBufferUsage::MultipleSubmit,
        )?;
        cbb.copy_buffer(CopyBufferInfo::buffers(
            frame.storage.typed(),
            buffer.cpu_accessible_buffer(),
        ))
        .unwrap();
        let cb = cbb.build().unwrap();
        let future = match cb.execute(queue) {
            Ok(f) => f,
            Err(_) => unreachable!(),
        };

        // dropping this future blocks this thread until the gpu finished the work
        drop(future);

        Ok(Frame { interpretation: frame.interpretation.clone(), storage: buffer })
    }
    pub fn ensure_cpu_buffer_frame(&self, payload: &Payload) -> Result<Arc<Frame<CpuBuffer>>> {
        if let Ok(frame) = payload.downcast::<Frame<CpuBuffer>>() {
            Ok(frame)
        } else if let Ok(frame) = payload.downcast::<Frame<GpuBuffer>>() {
            Ok(Arc::new(self.to_cpu_buffer_frame(frame)?))
        } else {
            Err(anyhow!(
                "wanted a frame with type {}, but the payload was of type {}",
                std::any::type_name::<Frame<CpuBuffer>>(),
                payload.type_name
            ))
        }
    }
    pub fn require_vulkan(&self) -> Result<(Arc<Device>, Vec<Arc<Queue>>)> {
        if let Some(vulkan_context) = &self.vulkan_device {
            Ok((vulkan_context.device.clone(), vulkan_context.queues.clone()))
        } else {
            Err(anyhow!("gpu required but not present"))
        }
    }

    pub fn spawn<O: Send + 'static>(
        &self,
        priority: Priority,
        fut: impl Future<Output = O> + Send + 'static,
    ) -> impl Future<Output = O> {
        self.prioritized_reactor.spawn_with_priority(fut, priority)
    }

    pub fn block_on<O>(&self, fut: impl Future<Output = O>) -> O {
        self.tokio_rt_handle.block_on(fut)
    }

    pub fn num_threads(&self) -> usize { self.prioritized_reactor.num_threads }
}
