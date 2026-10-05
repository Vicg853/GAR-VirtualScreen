use std::collections::BinaryHeap;

use crate::{FrameMetadata, OutputFrame};

use super::DecodeResult;

impl<T> PartialEq for DecodeResult<T> {
    fn eq(&self, other: &Self) -> bool {
        self.metadata
            .pic_order_cnt
            .eq(&other.metadata.pic_order_cnt)
    }
}

impl<T> Eq for DecodeResult<T> {}

impl<T> PartialOrd for DecodeResult<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for DecodeResult<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.metadata
            .pic_order_cnt
            .cmp(&other.metadata.pic_order_cnt)
            .reverse()
    }
}

impl<T> From<DecodeResult<T>> for OutputFrame<T> {
    fn from(result: DecodeResult<T>) -> Self {
        Self {
            data: result.frame,
            metadata: FrameMetadata {
                pts: result.metadata.pts,
                color_space: result.metadata.color_space,
                color_range: result.metadata.color_range,
            },
        }
    }
}

pub(crate) struct FrameSorter<T> {
    frames: BinaryHeap<DecodeResult<T>>,
    no_reordering: bool,
}

impl<T> FrameSorter<T> {
    pub(crate) fn new() -> Self {
        Self {
            frames: BinaryHeap::new(),
            no_reordering: false,
        }
    }

    pub(crate) fn set_no_reordering(&mut self) {
        self.no_reordering = true;
    }

    pub(crate) fn put(&mut self, frame: DecodeResult<T>) -> Vec<OutputFrame<T>> {
        let max_num_reorder_frames = if self.no_reordering {
            0
        } else {
            frame.metadata.max_num_reorder_frames as usize
        };
        let is_idr = frame.metadata.is_idr;
        let mut result = Vec::new();

        if is_idr {
            while !self.frames.is_empty() {
                let frame = self.frames.pop().unwrap();
                result.push(frame.into());
            }

            result.push(frame.into());
        } else {
            self.frames.push(frame);

            while self.frames.len() > max_num_reorder_frames {
                let frame = self.frames.pop().unwrap();
                result.push(frame.into());
            }
        }

        result
    }

    pub(crate) fn put_frames(&mut self, frames: Vec<DecodeResult<T>>) -> Vec<OutputFrame<T>> {
        let mut result = Vec::new();
        for unsorted_frame in frames {
            let mut sorted_frames = self.put(unsorted_frame);
            result.append(&mut sorted_frames);
        }

        result
    }

    pub(crate) fn flush(&mut self) -> Vec<OutputFrame<T>> {
        let mut result = Vec::with_capacity(self.frames.len());

        while !self.frames.is_empty() {
            let frame = self.frames.pop().unwrap();
            result.push(frame.into());
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::super::DecodeResultMetadata;
    use super::*;
    use crate::device::{ColorRange, ColorSpace};
    #[test]
    fn explicit_no_reordering_releases_every_completed_picture() {
        let mut sorter = FrameSorter::new();
        sorter.set_no_reordering();
        for index in 0..1000 {
            let output = sorter.put(DecodeResult {
                frame: index,
                metadata: DecodeResultMetadata {
                    pts: Some(index as u64),
                    pic_order_cnt: index,
                    max_num_reorder_frames: 16,
                    is_idr: index % 30 == 0,
                    color_space: ColorSpace::BT709,
                    color_range: ColorRange::Limited,
                },
            });
            assert_eq!(output.len(), 1);
            assert_eq!(output[0].metadata.pts, Some(index as u64));
            assert!(sorter.frames.is_empty());
        }
    }
}
