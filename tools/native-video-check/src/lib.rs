//! Nonvisual native-video tests and type checks. Uses the real source files.
//! This does not validate Linux WSI, Vulkan drivers or hardware decoding.
//!
//! Unsupported codec/API pairs must not instantiate:
//! ```compile_fail
//! use airbridge_engine_native_check::stream_engine::{DecodeApi, SoftwareH264};
//! struct UnsupportedCodec;
//! fn requires<A: DecodeApi<UnsupportedCodec>>() {}
//! requires::<SoftwareH264>();
//! ```
#![allow(dead_code)]
#[path = "../../../crates/bridge-viewer/src/linux.rs"]
mod linux;
#[path = "../../../crates/bridge-viewer/src/native_video.rs"]
mod native_video;
#[path = "../../../crates/bridge-viewer/src/resource_lifecycle.rs"]
mod resource_lifecycle;
#[path = "../../../crates/bridge-viewer/src/stream_engine.rs"]
pub mod stream_engine;
