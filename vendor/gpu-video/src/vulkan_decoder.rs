use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use ash::vk;

use h264_reader::nal::{pps::PicParameterSet, sps::SeqParameterSet};
use rustc_hash::FxHashMap;
use session_resources::VideoSessionResources;

use crate::{
    RawFrameData,
    codec::h264::parameters::SeqParameterSetExt as _,
    device::{ColorRange, ColorSpace, DecodingDevice},
    parser::{
        decoder_instructions::DecoderInstruction,
        reference_manager::{DecodeInformation, ReferenceId},
    },
};
use crate::{VulkanCommonError, wrappers::*};

mod frame_sorter;
mod session_resources;

pub(crate) use frame_sorter::FrameSorter;

pub struct VulkanDecoder<'a> {
    video_session_resources: Option<VideoSessionResources<'a>>,
    #[cfg(feature = "wgpu")]
    pending_outputs: VecDeque<PendingOutput>,
    #[cfg(feature = "wgpu")]
    output_pool: Arc<Mutex<Vec<Arc<Image>>>>,
    #[cfg(feature = "wgpu")]
    scratch_images: Vec<Arc<Image>>,
    #[cfg(feature = "wgpu")]
    submitted_images: VecDeque<(SemaphoreWaitValue, Arc<Image>)>,
    pub(crate) tracker: DecoderTracker,
    reference_id_to_dpb_slot_index: FxHashMap<ReferenceId, usize>,
    decoding_device: Arc<DecodingDevice>,
    usage_info: vk::VideoDecodeUsageInfoKHR<'a>,
    pub(crate) timing_origin: Option<std::time::Instant>,
    pub(crate) upload_timing: Option<(u64, u64)>,
    image_modifiers: ImageModifiers,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ImageModifiers {
    pub(crate) create_flags: vk::ImageCreateFlags,
    pub(crate) usage_flags: vk::ImageUsageFlags,
    pub(crate) additional_queue_index: usize,
}

pub(crate) enum DecoderTrackerWaitState {
    NewDecodingImagesLayoutTransition,
    Decode,
    DownloadImageToBuffer,
    #[cfg_attr(not(feature = "transcoder"), allow(dead_code))]
    ExternalProcessing,
}

pub(crate) struct DecoderTrackerKind {}

impl TrackerKind for DecoderTrackerKind {
    type WaitState = DecoderTrackerWaitState;

    type CommandBufferPools = DecoderCommandBufferPools;
}

pub(crate) struct DecoderCommandBufferPools {
    decode: CommandBufferPool,
    transfer: CommandBufferPool,
}

impl CommandBufferPoolStorage for DecoderCommandBufferPools {
    fn mark_submitted_as_free(&mut self, last_waited_for: SemaphoreWaitValue) {
        self.decode.mark_submitted_as_free(last_waited_for);
        self.transfer.mark_submitted_as_free(last_waited_for);
    }
}

pub(crate) type DecoderTracker = Tracker<DecoderTrackerKind>;

pub(crate) struct DecodeSubmissionImageInfo {
    pub(crate) image: Arc<Image>,
    pub(crate) layer: u32,
    pub(crate) cropped_extent: vk::Extent2D,
    pub(crate) cropped_origin: vk::Offset3D,
}

pub(crate) struct DecodeResultMetadata {
    pub(crate) pts: Option<u64>,
    pub(crate) pic_order_cnt: i32,
    pub(crate) max_num_reorder_frames: u64,
    pub(crate) is_idr: bool,
    pub(crate) color_space: ColorSpace,
    pub(crate) color_range: ColorRange,
}

pub(crate) struct DecodeResult<T> {
    pub(crate) frame: T,
    pub(crate) metadata: DecodeResultMetadata,
}

#[cfg(feature = "wgpu")]
struct PendingOutput {
    result: DecodeResult<wgpu::Texture>,
    ready: SemaphoreWaitValue,
    input: DecodeInputBuffer,
    _resources: InFlightDecodeResources,
    query: Option<Arc<DecodingQueryPool>>,
}

/// Vulkan resources that must be kept alive while a decode submission is in flight.
pub(crate) struct InFlightDecodeResources {
    _video_session: Arc<VideoSession>,
    _video_session_params: Arc<VideoSessionParameters>,
    _dpb_image_with_view: Arc<ImageWithView>,
    _dst_image_with_view: Option<Arc<ImageWithView>>,
}

// Field guard runs before the other submission fields are destroyed on an
// error or unwind. Native decode/copy resources may not outlive their commands.
struct SubmissionFence {
    timeline: Arc<TimelineSemaphore>,
    armed: bool,
}
impl Drop for SubmissionFence {
    fn drop(&mut self) {
        if self.armed {
            let last = self
                .timeline
                .highest_submitted
                .load(std::sync::atomic::Ordering::Acquire);
            let _ = self.timeline.wait(u64::MAX, SemaphoreWaitValue(last));
        }
    }
}

pub(crate) struct DecodeSubmission<'borrow, 'decoder> {
    _flight: SubmissionFence,
    pub(crate) decode_result: DecodeResult<DecodeSubmissionImageInfo>,
    pub(crate) decoder: &'borrow mut VulkanDecoder<'decoder>,
    pub(crate) input_buffer: DecodeInputBuffer,
    pub(crate) decode_query_pool: Option<Arc<DecodingQueryPool>>,
    #[cfg_attr(not(feature = "transcoder"), allow(dead_code))]
    pub(crate) semaphore_wait_value: SemaphoreWaitValue,
    #[cfg_attr(not(feature = "transcoder"), allow(dead_code))]
    pub(crate) in_flight_resources: InFlightDecodeResources,
}

impl<'a, 'b> DecodeSubmission<'a, 'b> {
    fn download_output(self) -> Result<DecodeResult<RawFrameData>, VulkanDecoderError> {
        let raw_frame_data = self.decoder.download_output(&self.decode_result.frame)?;
        let frame = RawFrameData {
            frame: raw_frame_data,
            width: self.decode_result.frame.cropped_extent.width,
            height: self.decode_result.frame.cropped_extent.height,
        };

        self.finish(frame)
    }

    #[cfg(feature = "wgpu")]
    fn output_to_wgpu_texture(self) -> Result<DecodeResult<wgpu::Texture>, VulkanDecoderError> {
        let (wgpu_texture, ready) = self
            .decoder
            .output_to_wgpu_texture(&self.decode_result.frame)?;

        self.decoder.tracker.wait_for(ready, u64::MAX)?;
        while self
            .decoder
            .submitted_images
            .front()
            .is_some_and(|(value, _)| *value <= ready)
        {
            self.decoder.submitted_images.pop_front();
        }
        self.finish(wgpu_texture)
    }

    fn finish<T>(self, output: T) -> Result<DecodeResult<T>, VulkanDecoderError> {
        self.input_buffer.release_to_pool();

        if let Some(query_pool) = self.decode_query_pool {
            query_pool.check_results_blocking()?;
        }

        Ok(DecodeResult {
            frame: output,
            metadata: self.decode_result.metadata,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VulkanDecoderError {
    #[error("Vulkan error: {0}")]
    VkError(#[from] vk::Result),

    #[error("The device does not support vulkan h264 decoding")]
    VulkanDecoderUnsupported,

    #[error(
        "A NALU requiring a session received before a session was created (probably before receiving first SPS)"
    )]
    NoSession,

    #[error(
        "A picture which is not in the decoded pictures buffer was requested as a reference picture"
    )]
    NonExistentReferenceRequested,

    #[error("A vulkan decode operation failed with code {0:?}")]
    DecodeOperationFailed(vk::QueryResultStatusKHR),

    #[error("Invalid input data for the decoder: {0}.")]
    InvalidInputData(String),

    #[error("Monochrome video is not supported")]
    MonochromeChromaFormatUnsupported,

    #[error(transparent)]
    VulkanCommonError(#[from] VulkanCommonError),
}

impl VulkanDecoder<'_> {
    pub fn new(
        decoding_device: Arc<DecodingDevice>,
        usage_flags: crate::parameters::DecoderUsageFlags,
        image_modifiers: ImageModifiers,
    ) -> Result<Self, VulkanDecoderError> {
        let command_buffer_pools = DecoderCommandBufferPools {
            transfer: CommandBufferPool::new(
                decoding_device.vulkan_device.clone(),
                decoding_device.vulkan_device.queues.transfer.family_index,
            )?,
            decode: CommandBufferPool::new(
                decoding_device.vulkan_device.clone(),
                decoding_device.h264_decode_queues.family_index,
            )?,
        };

        let tracker = Tracker::new(
            decoding_device.vulkan_device.device.clone(),
            command_buffer_pools,
            Some("decoder"),
        )?;

        let usage_info = vk::VideoDecodeUsageInfoKHR::default().video_usage_hints(usage_flags);

        Ok(Self {
            decoding_device,
            video_session_resources: None,
            #[cfg(feature = "wgpu")]
            pending_outputs: VecDeque::new(),
            #[cfg(feature = "wgpu")]
            output_pool: Arc::new(Mutex::new(Vec::new())),
            #[cfg(feature = "wgpu")]
            scratch_images: Vec::new(),
            #[cfg(feature = "wgpu")]
            submitted_images: VecDeque::new(),
            tracker,
            reference_id_to_dpb_slot_index: Default::default(),
            usage_info,
            timing_origin: None,
            upload_timing: None,
            image_modifiers,
        })
    }
}

impl<'a> VulkanDecoder<'a> {
    pub fn decode_to_bytes(
        &mut self,
        decoder_instructions: &[DecoderInstruction],
    ) -> Result<Vec<DecodeResult<RawFrameData>>, VulkanDecoderError> {
        let mut result = Vec::new();
        for instruction in decoder_instructions {
            if let Some(output) = self.decode(instruction)? {
                result.push(output.download_output()?);
            }
        }

        Ok(result)
    }

    #[cfg(feature = "wgpu")]
    pub fn decode_to_wgpu_textures(
        &mut self,
        decoder_instructions: &[DecoderInstruction],
    ) -> Result<Vec<DecodeResult<wgpu::Texture>>, VulkanDecoderError> {
        let mut result = Vec::new();
        for instruction in decoder_instructions {
            if let Some(output) = self.decode(instruction)? {
                result.push(output.output_to_wgpu_texture()?);
            }
        }

        Ok(result)
    }

    #[cfg(feature = "wgpu")]
    pub fn submit_to_wgpu_textures<const MEASURE: bool>(
        &mut self,
        instructions: &[DecoderInstruction],
    ) -> Result<(), VulkanDecoderError> {
        for instruction in instructions {
            if self.pending_outputs.len() >= 4 {
                return Err(VulkanDecoderError::InvalidInputData(
                    "GPU decode admission capacity exceeded".into(),
                ));
            }
            if let Some(mut submission) = self.decode_impl::<MEASURE>(instruction)? {
                let (texture, ready) = submission
                    .decoder
                    .output_to_wgpu_texture(&submission.decode_result.frame)?;
                let pending = PendingOutput {
                    result: DecodeResult {
                        frame: texture,
                        metadata: submission.decode_result.metadata,
                    },
                    ready,
                    input: submission.input_buffer,
                    _resources: submission.in_flight_resources,
                    query: submission.decode_query_pool,
                };
                submission.decoder.pending_outputs.push_back(pending);
                submission._flight.armed = false; // PendingOutput now retains all native resource leases.
            }
        }
        Ok(())
    }

    #[cfg(feature = "wgpu")]
    pub fn poll_wgpu_textures(
        &mut self,
    ) -> Result<Vec<DecodeResult<wgpu::Texture>>, VulkanDecoderError> {
        let mut ready_frames = Vec::new();
        while let Some(front) = self.pending_outputs.front() {
            let completed = unsafe {
                self.decoding_device
                    .vulkan_device
                    .device
                    .get_semaphore_counter_value(
                        self.tracker.semaphore_tracker.semaphore.semaphore,
                    )?
            };
            if completed < front.ready.0 {
                break;
            }
            let ready = self.pending_outputs.pop_front().unwrap();
            self.tracker.mark_waited(ready.ready);
            while self
                .submitted_images
                .front()
                .is_some_and(|(value, _)| value.0 <= completed)
            {
                self.submitted_images.pop_front();
            }
            if let Some(query) = &ready.query {
                query.check_results_blocking()?;
            }
            ready.input.release_to_pool();
            // Cross-queue memory visibility must be established on the graphics
            // queue; a CPU counter observation alone is not a GPU memory dependency.
            let queue = self.decoding_device.vulkan_device.wgpu_queue();
            let timeline = self.tracker.semaphore_tracker.semaphore.clone();
            {
                let hal = unsafe { queue.as_hal::<wgpu::hal::vulkan::Api>() }.unwrap();
                hal.add_wait_semaphore(
                    timeline.semaphore,
                    Some(ready.ready.0),
                    vk::PipelineStageFlags::ALL_COMMANDS,
                );
            }
            queue.submit([]);
            // Keep the external semaphore alive until the graphics wait is done.
            queue.on_submitted_work_done(move || drop(timeline));
            ready_frames.push(ready.result);
        }
        Ok(ready_frames)
    }

    #[cfg(feature = "wgpu")]
    pub fn pending_count(&self) -> usize {
        self.pending_outputs.len()
    }

    pub(crate) fn decode<'b>(
        &'b mut self,
        instruction: &DecoderInstruction,
    ) -> Result<Option<DecodeSubmission<'b, 'a>>, VulkanDecoderError> {
        self.decode_impl::<false>(instruction)
    }

    fn decode_impl<'b, const MEASURE: bool>(
        &'b mut self,
        instruction: &DecoderInstruction,
    ) -> Result<Option<DecodeSubmission<'b, 'a>>, VulkanDecoderError> {
        match instruction {
            DecoderInstruction::Decode {
                decode_info,
                reference_id,
            } => {
                return self
                    .process_reference_frame::<MEASURE>(decode_info, *reference_id)
                    .map(Option::Some);
            }

            DecoderInstruction::Idr {
                decode_info,
                reference_id,
            } => {
                return self
                    .process_idr::<MEASURE>(decode_info, *reference_id)
                    .map(Option::Some);
            }

            DecoderInstruction::Drop { reference_ids } => {
                for reference_id in reference_ids {
                    match self.reference_id_to_dpb_slot_index.remove(reference_id) {
                        Some(dpb_idx) => self
                            .video_session_resources
                            .as_mut()
                            .map(|s| s.free_reference_picture(dpb_idx)),
                        None => return Err(VulkanDecoderError::NonExistentReferenceRequested),
                    };
                }
            }

            DecoderInstruction::Sps(sps) => self.process_sps(sps)?,

            DecoderInstruction::Pps(pps) => self.process_pps(pps)?,
        }

        Ok(None)
    }

    fn process_sps(&mut self, sps: &SeqParameterSet) -> Result<(), VulkanDecoderError> {
        match self.video_session_resources.as_mut() {
            Some(session) => session.process_sps(sps.clone(), self.usage_info)?,
            None => {
                self.video_session_resources = Some(VideoSessionResources::new_from_sps(
                    &self.decoding_device,
                    self.tracker.command_buffer_pools.decode.begin_buffer()?,
                    sps.clone(),
                    self.usage_info,
                    &mut self.tracker,
                    self.image_modifiers,
                )?)
            }
        }

        Ok(())
    }

    fn process_pps(&mut self, pps: &PicParameterSet) -> Result<(), VulkanDecoderError> {
        self.video_session_resources
            .as_mut()
            .ok_or(VulkanDecoderError::NoSession)?
            .process_pps(pps.clone())?;

        Ok(())
    }

    fn process_idr<'b, const MEASURE: bool>(
        &'b mut self,
        decode_information: &DecodeInformation,
        reference_id: ReferenceId,
    ) -> Result<DecodeSubmission<'b, 'a>, VulkanDecoderError> {
        self.do_decode::<MEASURE>(decode_information, reference_id, true, true)
    }

    fn process_reference_frame<'b, const MEASURE: bool>(
        &'b mut self,
        decode_information: &DecodeInformation,
        reference_id: ReferenceId,
    ) -> Result<DecodeSubmission<'b, 'a>, VulkanDecoderError> {
        self.do_decode::<MEASURE>(decode_information, reference_id, false, true)
    }

    fn do_decode<'b, const MEASURE: bool>(
        &'b mut self,
        decode_information: &'_ DecodeInformation,
        reference_id: ReferenceId,
        is_idr: bool,
        is_reference: bool,
    ) -> Result<DecodeSubmission<'b, 'a>, VulkanDecoderError> {
        let video_session_resources = self
            .video_session_resources
            .as_mut()
            .ok_or(VulkanDecoderError::NoSession)?;

        let sps = video_session_resources
            .sps
            .get(&decode_information.sps_id)
            .ok_or(VulkanDecoderError::InvalidInputData(format!(
                "Unknown SPS id {}",
                decode_information.sps_id
            )))?;

        let cropped_extent = sps.size()?;
        let cropped_origin = sps
            .frame_cropping
            .as_ref()
            .map_or(vk::Offset3D::default(), |crop| vk::Offset3D {
                x: (crop.left_offset * 2) as i32,
                y: (crop.top_offset * 2) as i32,
                z: 0,
            });
        let color_space = ColorSpace::from(sps);
        let color_range = ColorRange::from(sps);

        if is_idr {
            video_session_resources.ensure_session(
                &self.decoding_device,
                self.tracker.command_buffer_pools.decode.begin_buffer()?,
                &mut self.tracker,
            )?;
        }

        // upload data to a buffer
        let size = (decode_information.rbsp_bytes.len() as u64).next_multiple_of(
            self.decoding_device
                .profile_capabilities
                .video_capabilities
                .min_bitstream_buffer_size_alignment,
        );

        let mut buffer = video_session_resources.decode_buffer_pool.buffer()?;
        let upload_start = if MEASURE {
            self.timing_origin
                .map(|start| start.elapsed().as_micros() as u64)
        } else {
            None
        };
        buffer.upload_data(
            &decode_information.rbsp_bytes,
            size,
            &video_session_resources.parameters.profile_info,
        )?;
        if MEASURE {
            if let (Some(start), Some(origin)) = (upload_start, self.timing_origin) {
                self.upload_timing = Some((
                    start,
                    (origin.elapsed().as_micros() as u64).saturating_sub(start),
                ));
            }
        }

        // decode
        // IDR - remove all reference picures
        if is_idr {
            video_session_resources
                .decoding_images
                .reset_all_allocations();

            self.reference_id_to_dpb_slot_index = Default::default();
        }

        // begin video coding
        let mut cmd_buffer = self.tracker.command_buffer_pools.decode.begin_buffer()?;

        video_session_resources
            .decoding_images
            .dpb
            .image
            .image_with_view
            .transition_layout(
                &mut cmd_buffer,
                vk::PipelineStageFlags2::VIDEO_DECODE_KHR
                    ..vk::PipelineStageFlags2::VIDEO_DECODE_KHR,
                vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR
                    ..vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR
                        | vk::AccessFlags2::VIDEO_DECODE_READ_KHR,
                vk::ImageLayout::VIDEO_DECODE_DPB_KHR,
                vk::ImageSubresourceRange {
                    base_array_layer: 0,
                    layer_count: vk::REMAINING_ARRAY_LAYERS,
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                },
            )?;

        if let Some(dst) = &video_session_resources.decoding_images.dst_image {
            dst.image_with_view.transition_layout(
                &mut cmd_buffer,
                vk::PipelineStageFlags2::VIDEO_DECODE_KHR
                    ..vk::PipelineStageFlags2::VIDEO_DECODE_KHR,
                vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR
                    ..vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR
                        | vk::AccessFlags2::VIDEO_DECODE_READ_KHR,
                vk::ImageLayout::VIDEO_DECODE_DST_KHR,
                vk::ImageSubresourceRange {
                    base_array_layer: 0,
                    layer_count: vk::REMAINING_ARRAY_LAYERS,
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                },
            )?;
        }

        let memory_barrier = vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
            .src_access_mask(vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR)
            .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
            .dst_access_mask(
                vk::AccessFlags2::VIDEO_DECODE_READ_KHR | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR,
            );

        unsafe {
            self.decoding_device
                .vulkan_device
                .device
                .cmd_pipeline_barrier2(
                    cmd_buffer.buffer(),
                    &vk::DependencyInfo::default().memory_barriers(&[memory_barrier]),
                )
        };

        if let Some(pool) = video_session_resources.decode_query_pool.as_ref() {
            pool.reset(cmd_buffer.buffer());
        }

        let reference_slots = video_session_resources
            .decoding_images
            .reference_slot_info();

        let begin_info = vk::VideoBeginCodingInfoKHR::default()
            .video_session(video_session_resources.video_session.session)
            .video_session_parameters(video_session_resources.parameters_manager.parameters())
            .reference_slots(&reference_slots);

        unsafe {
            self.decoding_device
                .vulkan_device
                .device
                .video_queue_ext
                .cmd_begin_video_coding_khr(cmd_buffer.buffer(), &begin_info)
        };

        // IDR - issue the reset command to the video session
        if is_idr {
            let control_info = vk::VideoCodingControlInfoKHR::default()
                .flags(vk::VideoCodingControlFlagsKHR::RESET);

            unsafe {
                self.decoding_device
                    .vulkan_device
                    .device
                    .video_queue_ext
                    .cmd_control_video_coding_khr(cmd_buffer.buffer(), &control_info)
            };
        }

        // allocate a new reference picture and fill out the forms to get it set up
        let new_reference_slot_index = video_session_resources
            .decoding_images
            .allocate_reference_picture()?;

        let new_reference_slot_std_reference_info = decode_information.picture_info.into();
        let mut new_reference_slot_dpb_slot_info = vk::VideoDecodeH264DpbSlotInfoKHR::default()
            .std_reference_info(&new_reference_slot_std_reference_info);

        let new_reference_slot_video_picture_resource_info = video_session_resources
            .decoding_images
            .video_resource_info(new_reference_slot_index)
            .unwrap();

        let setup_reference_slot = vk::VideoReferenceSlotInfoKHR::default()
            .picture_resource(new_reference_slot_video_picture_resource_info)
            .slot_index(new_reference_slot_index as i32)
            .push_next(&mut new_reference_slot_dpb_slot_info);

        // prepare the reference list
        let reference_slots = video_session_resources
            .decoding_images
            .reference_slot_info();

        let references_std_ref_info = Self::prepare_references_std_ref_info(decode_information);

        let mut references_dpb_slot_info =
            Self::prepare_references_dpb_slot_info(&references_std_ref_info);

        let pic_reference_slots = Self::prepare_reference_list_slot_info(
            &self.reference_id_to_dpb_slot_index,
            &reference_slots,
            &mut references_dpb_slot_info,
            decode_information,
        )?;

        // prepare the decode target picture
        let std_picture_info = vk::native::StdVideoDecodeH264PictureInfo {
            flags: vk::native::StdVideoDecodeH264PictureInfoFlags {
                _bitfield_align_1: [],
                __bindgen_padding_0: [0; 3],
                _bitfield_1: vk::native::StdVideoDecodeH264PictureInfoFlags::new_bitfield_1(
                    matches!(
                        decode_information.header.field_pic,
                        h264_reader::nal::slice::FieldPic::Field(..)
                    )
                    .into(),
                    is_idr.into(),
                    is_idr.into(),
                    0,
                    is_reference.into(),
                    0,
                ),
            },
            PicOrderCnt: decode_information.picture_info.PicOrderCnt_for_decoding,
            seq_parameter_set_id: decode_information.sps_id,
            pic_parameter_set_id: decode_information.pps_id,
            frame_num: decode_information.header.frame_num,
            idr_pic_id: decode_information
                .header
                .idr_pic_id
                .map(|a| a as u16)
                .unwrap_or(0),
            reserved1: 0,
            reserved2: 0,
        };

        let slice_offsets = decode_information
            .slice_indices
            .iter()
            .map(|&x| x as u32)
            .collect::<Vec<_>>();

        let mut decode_h264_picture_info = vk::VideoDecodeH264PictureInfoKHR::default()
            .std_picture_info(&std_picture_info)
            .slice_offsets(&slice_offsets);

        let dst_picture_resource_info = &video_session_resources
            .decoding_images
            .target_picture_resource_info(new_reference_slot_index)
            .unwrap();

        // these 3 variables are for copying the result later
        let (target_image, target_layer) = video_session_resources
            .decoding_images
            .target_info(new_reference_slot_index);

        // fill out the final struct and issue the command
        let decode_info = vk::VideoDecodeInfoKHR::default()
            .src_buffer(*buffer.buffer)
            .src_buffer_offset(0)
            .src_buffer_range(size)
            .dst_picture_resource(*dst_picture_resource_info)
            .setup_reference_slot(&setup_reference_slot)
            .reference_slots(&pic_reference_slots)
            .push_next(&mut decode_h264_picture_info);

        let submission_query = if video_session_resources.decode_query_pool.is_some() {
            Some(Arc::new(DecodingQueryPool::new(
                self.decoding_device.vulkan_device.device.clone(),
                video_session_resources
                    .parameters
                    .profile_info
                    .profile_info
                    .profile_info,
            )?))
        } else {
            None
        };
        if let Some(pool) = submission_query.as_ref() {
            pool.begin_query(cmd_buffer.buffer());
        }

        unsafe {
            self.decoding_device
                .vulkan_device
                .device
                .video_decode_queue_ext
                .cmd_decode_video_khr(cmd_buffer.buffer(), &decode_info)
        };

        if let Some(pool) = submission_query.as_ref() {
            pool.end_query(cmd_buffer.buffer());
        }

        unsafe {
            self.decoding_device
                .vulkan_device
                .device
                .video_queue_ext
                .cmd_end_video_coding_khr(
                    cmd_buffer.buffer(),
                    &vk::VideoEndCodingInfoKHR::default(),
                )
        };

        let semaphore_wait_value = self
            .decoding_device
            .h264_decode_queues
            .submit_chain_semaphore(
                cmd_buffer.end()?,
                &mut self.tracker,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                DecoderTrackerWaitState::Decode,
            )?;

        // after the decode save the new reference picture
        self.reference_id_to_dpb_slot_index
            .insert(reference_id, new_reference_slot_index);

        let in_flight_resources = InFlightDecodeResources {
            _video_session: video_session_resources.video_session.clone(),
            _video_session_params: video_session_resources
                .parameters_manager
                .parameters
                .clone(),
            _dpb_image_with_view: video_session_resources
                .decoding_images
                .dpb_image_with_view(),
            _dst_image_with_view: video_session_resources
                .decoding_images
                .dst_image_with_view(),
        };

        Ok(DecodeSubmission {
            _flight: SubmissionFence {
                timeline: self.tracker.semaphore_tracker.semaphore.clone(),
                armed: true,
            },
            decode_result: DecodeResult {
                frame: DecodeSubmissionImageInfo {
                    image: target_image,
                    layer: target_layer as u32,
                    cropped_extent,
                    cropped_origin,
                },
                metadata: DecodeResultMetadata {
                    pic_order_cnt: decode_information.picture_info.PicOrderCnt_for_decoding[0],
                    max_num_reorder_frames: video_session_resources
                        .parameters
                        .max_num_reorder_frames,
                    is_idr,
                    pts: decode_information.pts,
                    color_space,
                    color_range,
                },
            },
            semaphore_wait_value,
            decode_query_pool: submission_query,
            in_flight_resources,
            input_buffer: buffer,
            decoder: self,
        })
    }

    #[cfg(feature = "wgpu")]
    fn output_to_wgpu_texture(
        &mut self,
        decode_output: &DecodeSubmissionImageInfo,
    ) -> Result<(wgpu::Texture, SemaphoreWaitValue), VulkanDecoderError> {
        let wgpu_device = unsafe {
            self.decoding_device
                .wgpu_device()
                .as_hal::<wgpu::hal::vulkan::Api>()
                .unwrap()
        };
        let copy_extent = vk::Extent3D {
            width: decode_output.cropped_extent.width,
            height: decode_output.cropped_extent.height,
            depth: 1,
        };

        let mut queue_indices = vec![
            self.decoding_device.queues.transfer.family_index as u32,
            self.decoding_device.queues.wgpu.family_index as u32,
        ];
        queue_indices.sort();
        queue_indices.dedup();

        let create_info = vk::ImageCreateInfo::default()
            .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .extent(copy_extent)
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::TRANSFER_SRC,
            )
            .sharing_mode(if queue_indices.len() > 1 {
                vk::SharingMode::CONCURRENT
            } else {
                vk::SharingMode::EXCLUSIVE
            })
            .queue_family_indices(if queue_indices.len() > 1 {
                &queue_indices
            } else {
                &[]
            })
            .initial_layout(vk::ImageLayout::UNDEFINED);

        let cached = {
            let mut pool = self.output_pool.lock().unwrap();
            let index = pool.iter().position(|image| image.extent == copy_extent);
            index.map(|index| pool.swap_remove(index))
        };
        let image = match cached {
            Some(image) => image,
            None => Arc::new(Image::new(
                self.decoding_device.allocator.clone(),
                &create_info,
                self.tracker.image_layout_tracker.clone(),
            )?),
        };
        // Retain before submitting: even a panic while constructing wgpu handles
        // cannot release an image that the copy queue is still writing.
        self.scratch_images.push(image.clone());

        let mut cmd_buffer = self.tracker.command_buffer_pools.transfer.begin_buffer()?;

        decode_output.image.transition_layout_single_layer(
            &mut cmd_buffer,
            vk::PipelineStageFlags2::NONE..vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::NONE..vk::AccessFlags2::TRANSFER_READ,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            decode_output.layer,
        )?;

        image.transition_layout_single_layer(
            &mut cmd_buffer,
            vk::PipelineStageFlags2::NONE..vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::NONE..vk::AccessFlags2::TRANSFER_WRITE,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            0,
        )?;

        let copy_info = [
            vk::ImageCopy::default()
                .src_subresource(vk::ImageSubresourceLayers {
                    base_array_layer: decode_output.layer,
                    mip_level: 0,
                    layer_count: 1,
                    aspect_mask: vk::ImageAspectFlags::PLANE_0,
                })
                .src_offset(decode_output.cropped_origin)
                .dst_subresource(vk::ImageSubresourceLayers {
                    base_array_layer: 0,
                    mip_level: 0,
                    layer_count: 1,
                    aspect_mask: vk::ImageAspectFlags::PLANE_0,
                })
                .dst_offset(vk::Offset3D::default())
                .extent(copy_extent),
            vk::ImageCopy::default()
                .src_subresource(vk::ImageSubresourceLayers {
                    base_array_layer: decode_output.layer,
                    mip_level: 0,
                    layer_count: 1,
                    aspect_mask: vk::ImageAspectFlags::PLANE_1,
                })
                .src_offset(vk::Offset3D {
                    x: decode_output.cropped_origin.x / 2,
                    y: decode_output.cropped_origin.y / 2,
                    z: 0,
                })
                .dst_subresource(vk::ImageSubresourceLayers {
                    base_array_layer: 0,
                    mip_level: 0,
                    layer_count: 1,
                    aspect_mask: vk::ImageAspectFlags::PLANE_1,
                })
                .dst_offset(vk::Offset3D::default())
                .extent(vk::Extent3D {
                    width: copy_extent.width / 2,
                    height: copy_extent.height / 2,
                    ..copy_extent
                }),
        ];

        unsafe {
            self.decoding_device.vulkan_device.device.cmd_copy_image(
                cmd_buffer.buffer(),
                decode_output.image.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                **image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &copy_info,
            );
        }

        image.transition_layout_single_layer(
            &mut cmd_buffer,
            vk::PipelineStageFlags2::COPY..vk::PipelineStageFlags2::NONE,
            vk::AccessFlags2::TRANSFER_WRITE..vk::AccessFlags2::NONE,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            0,
        )?;

        let semaphore_wait_value = self
            .decoding_device
            .queues
            .transfer
            .submit_chain_semaphore(
                cmd_buffer.end()?,
                &mut self.tracker,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                DecoderTrackerWaitState::DownloadImageToBuffer,
            )?;

        let owned = self.scratch_images.pop().unwrap();
        self.submitted_images
            .push_back((semaphore_wait_value, owned));
        let image_clone = image.clone();
        let pool = self.output_pool.clone();

        let hal_texture = unsafe {
            wgpu_device.texture_from_raw(
                **image,
                &wgpu::hal::TextureDescriptor {
                    label: Some("vulkan video output texture"),
                    usage: wgpu::TextureUses::RESOURCE
                        | wgpu::TextureUses::COPY_DST
                        | wgpu::TextureUses::COPY_SRC,
                    memory_flags: wgpu::hal::MemoryFlags::empty(),
                    size: wgpu::Extent3d {
                        width: copy_extent.width,
                        height: copy_extent.height,
                        depth_or_array_layers: copy_extent.depth,
                    },
                    dimension: wgpu::TextureDimension::D2,
                    sample_count: 1,
                    view_formats: Vec::new(),
                    format: wgpu::TextureFormat::NV12,
                    mip_level_count: 1,
                },
                Some(Box::new(move || {
                    let mut pool = pool.lock().unwrap();
                    if pool.len() < 8 {
                        pool.push(image_clone);
                    }
                })),
                wgpu::hal::vulkan::TextureMemory::External,
            )
        };

        let wgpu_texture = unsafe {
            self.decoding_device
                .wgpu_device()
                .create_texture_from_hal::<wgpu::hal::vulkan::Api>(
                    hal_texture,
                    &wgpu::TextureDescriptor {
                        label: Some("vulkan video output texture"),
                        usage: wgpu::TextureUsages::COPY_DST
                            | wgpu::TextureUsages::TEXTURE_BINDING
                            | wgpu::TextureUsages::COPY_SRC,
                        size: wgpu::Extent3d {
                            width: copy_extent.width,
                            height: copy_extent.height,
                            depth_or_array_layers: copy_extent.depth,
                        },
                        dimension: wgpu::TextureDimension::D2,
                        sample_count: 1,
                        view_formats: &[],
                        format: wgpu::TextureFormat::NV12,
                        mip_level_count: 1,
                    },
                    wgpu::TextureUses::RESOURCE,
                )
        };

        Ok((wgpu_texture, semaphore_wait_value))
    }

    fn download_output(
        &mut self,
        decode_output: &DecodeSubmissionImageInfo,
    ) -> Result<Vec<u8>, VulkanDecoderError> {
        let extent = vk::Extent3D {
            width: decode_output.cropped_extent.width,
            height: decode_output.cropped_extent.height,
            depth: 1,
        };
        let (mut dst_buffer, wait_value) =
            self.copy_image_to_buffer(&decode_output.image, extent, decode_output.layer)?;

        self.tracker.wait_for(wait_value, u64::MAX)?;

        let output = unsafe {
            dst_buffer
                .download_data_from_buffer(extent.width as usize * extent.height as usize * 3 / 2)?
        };

        Ok(output)
    }

    fn prepare_references_std_ref_info(
        decode_information: &DecodeInformation,
    ) -> Vec<vk::native::StdVideoDecodeH264ReferenceInfo> {
        decode_information
            .reference_list_l0
            .iter()
            .flatten()
            .chain(decode_information.reference_list_l1.iter().flatten())
            .map(|&ref_info| ref_info.into())
            .collect::<Vec<_>>()
    }

    fn prepare_references_dpb_slot_info(
        references_std_ref_info: &[vk::native::StdVideoDecodeH264ReferenceInfo],
    ) -> Vec<vk::VideoDecodeH264DpbSlotInfoKHR<'_>> {
        references_std_ref_info
            .iter()
            .map(|info| vk::VideoDecodeH264DpbSlotInfoKHR::default().std_reference_info(info))
            .collect::<Vec<_>>()
    }

    fn prepare_reference_list_slot_info<'b>(
        reference_id_to_dpb_slot_index: &FxHashMap<ReferenceId, usize>,
        reference_slots: &'b [vk::VideoReferenceSlotInfoKHR<'b>],
        references_dpb_slot_info: &'b mut [vk::VideoDecodeH264DpbSlotInfoKHR<'b>],
        decode_information: &'b DecodeInformation,
    ) -> Result<Vec<vk::VideoReferenceSlotInfoKHR<'b>>, VulkanDecoderError> {
        let mut pic_reference_slots: Vec<vk::VideoReferenceSlotInfoKHR<'b>> = Vec::new();
        for (ref_info, dpb_slot_info) in decode_information
            .reference_list_l0
            .iter()
            .flatten()
            .chain(decode_information.reference_list_l1.iter().flatten())
            .zip(references_dpb_slot_info.iter_mut())
        {
            let i = *reference_id_to_dpb_slot_index
                .get(&ref_info.id)
                .ok_or(VulkanDecoderError::NonExistentReferenceRequested)?;

            let reference = *reference_slots
                .get(i)
                .ok_or(VulkanDecoderError::NonExistentReferenceRequested)?;

            if reference.slot_index < 0 || reference.p_picture_resource.is_null() {
                return Err(VulkanDecoderError::NonExistentReferenceRequested);
            }

            let reference = reference.push_next(dpb_slot_info);

            if pic_reference_slots
                .iter()
                .all(|r| r.slot_index != reference.slot_index)
            {
                pic_reference_slots.push(reference);
            }
        }

        Ok(pic_reference_slots)
    }

    fn copy_image_to_buffer(
        &mut self,
        image: &Image,
        dimensions: vk::Extent3D,
        layer: u32,
    ) -> Result<(Buffer, SemaphoreWaitValue), VulkanDecoderError> {
        let mut cmd_buffer = self.tracker.command_buffer_pools.transfer.begin_buffer()?;

        image.transition_layout_single_layer(
            &mut cmd_buffer,
            vk::PipelineStageFlags2::NONE..vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::NONE..vk::AccessFlags2::TRANSFER_READ,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            layer,
        )?;

        let y_plane_size = dimensions.width as u64 * dimensions.height as u64;

        let dst_buffer = Buffer::new_transfer(
            self.decoding_device.allocator.clone(),
            y_plane_size * 3 / 2,
            TransferDirection::GpuToMem,
        )?;

        let copy_info = [
            vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers {
                    mip_level: 0,
                    layer_count: 1,
                    base_array_layer: layer,
                    aspect_mask: vk::ImageAspectFlags::PLANE_0,
                })
                .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .image_extent(vk::Extent3D {
                    width: dimensions.width,
                    height: dimensions.height,
                    depth: 1,
                })
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0),
            vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers {
                    mip_level: 0,
                    layer_count: 1,
                    base_array_layer: layer,
                    aspect_mask: vk::ImageAspectFlags::PLANE_1,
                })
                .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .image_extent(vk::Extent3D {
                    width: dimensions.width / 2,
                    height: dimensions.height / 2,
                    depth: 1,
                })
                .buffer_offset(y_plane_size)
                .buffer_row_length(0)
                .buffer_image_height(0),
        ];

        unsafe {
            self.decoding_device
                .vulkan_device
                .device
                .cmd_copy_image_to_buffer(
                    cmd_buffer.buffer(),
                    **image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    *dst_buffer,
                    &copy_info,
                )
        };

        let wait_value = self
            .decoding_device
            .queues
            .transfer
            .submit_chain_semaphore(
                cmd_buffer.end()?,
                &mut self.tracker,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                DecoderTrackerWaitState::DownloadImageToBuffer,
            )?;

        Ok((dst_buffer, wait_value))
    }
}

impl Drop for VulkanDecoder<'_> {
    fn drop(&mut self) {
        // Native resource safety on normal close, device error and Rust unwinding.
        // Worker supervision is the outer timeout for an unresponsive driver.
        let _ = self.tracker.wait_for_all(u64::MAX);
    }
}
