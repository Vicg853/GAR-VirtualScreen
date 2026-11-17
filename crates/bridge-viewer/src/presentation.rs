//! Viewer-side presentation maths. Pure and platform independent, so it is
//! checked on every host even though only the Linux renderer draws with it.
use bridge_core::sessions::Scaling;

/// Where the picture is drawn on the surface, as `(x, y, width, height)` in
/// pixels. The rectangle may extend past the surface (Fill, 1:1); the GPU clips
/// it. `rotation` is in degrees and swaps the picture's sides at 90 and 270.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub fn viewport(
    scaling: Scaling,
    rotation: u16,
    frame: (u32, u32),
    surface: (u32, u32),
    max_dimension: u32,
) -> (f32, f32, f32, f32) {
    let (surface_width, surface_height) = (surface.0.max(1) as f32, surface.1.max(1) as f32);
    let (frame_width, frame_height) = if rotation % 180 == 90 {
        (frame.1.max(1) as f32, frame.0.max(1) as f32)
    } else {
        (frame.0.max(1) as f32, frame.1.max(1) as f32)
    };
    let fit = (surface_width / frame_width).min(surface_height / frame_height);
    let fill = (surface_width / frame_width).max(surface_height / frame_height);
    let (width, height) = match scaling {
        Scaling::Fit => (frame_width * fit, frame_height * fit),
        Scaling::Fill => (frame_width * fill, frame_height * fill),
        Scaling::Stretch => (surface_width, surface_height),
        Scaling::Native => (frame_width, frame_height),
    };
    // A viewport larger than the device's texture limit is rejected outright.
    let limit = max_dimension.max(1) as f32;
    let shrink = (limit / width).min(limit / height).min(1.0);
    let (width, height) = (width * shrink, height * shrink);
    (
        (surface_width - width) / 2.0,
        (surface_height - height) / 2.0,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_modes_place_the_picture() {
        let frame = (1920, 1080);
        // Fit letterboxes inside a taller surface.
        assert_eq!(
            viewport(Scaling::Fit, 0, frame, (1920, 1200), 8192),
            (0.0, 60.0, 1920.0, 1080.0)
        );
        // Fill covers it and overflows sideways, centred.
        let (x, y, width, height) = viewport(Scaling::Fill, 0, frame, (1920, 1200), 8192);
        assert_eq!((y, height), (0.0, 1200.0));
        assert!(width > 1920.0 && (x + width / 2.0 - 960.0).abs() < 0.01);
        assert_eq!(
            viewport(Scaling::Stretch, 0, frame, (1000, 1000), 8192),
            (0.0, 0.0, 1000.0, 1000.0)
        );
        // 1:1 keeps source pixels, centred even when larger than the window.
        assert_eq!(
            viewport(Scaling::Native, 0, frame, (1280, 720), 8192),
            (-320.0, -180.0, 1920.0, 1080.0)
        );
    }

    #[test]
    fn quarter_turns_swap_sides_and_oversized_pictures_stay_valid() {
        // A landscape stream turned 90° fits a portrait monitor exactly.
        assert_eq!(
            viewport(Scaling::Fit, 90, (1920, 1080), (1080, 1920), 8192),
            (0.0, 0.0, 1080.0, 1920.0)
        );
        assert_eq!(
            viewport(Scaling::Fit, 180, (1920, 1080), (1920, 1080), 8192),
            (0.0, 0.0, 1920.0, 1080.0)
        );
        let (_, _, width, height) = viewport(Scaling::Native, 0, (7680, 4320), (800, 600), 4096);
        assert!(width <= 4096.0 && height <= 4096.0);
        assert!((width / height - 7680.0 / 4320.0).abs() < 0.001);
        // Degenerate sizes never produce NaN or negative extents.
        let (x, y, width, height) = viewport(Scaling::Fill, 270, (0, 0), (0, 0), 0);
        assert!([x, y, width, height].iter().all(|value| value.is_finite()));
        assert!(width >= 0.0 && height >= 0.0);
    }

    #[test]
    fn presentation_shader_is_valid_without_opening_a_window() {
        let module = naga::front::wgsl::parse_str(include_str!("video.wgsl")).unwrap();
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap();
    }
}
