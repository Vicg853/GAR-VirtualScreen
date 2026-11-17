struct ColorUniform {
    values: vec4<f32>,
    coefficients: vec4<f32>,
    output: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(0) var plane_y_or_rgba: texture_2d<f32>;
@group(0) @binding(1) var plane_uv: texture_2d<f32>;
@group(0) @binding(4) var plane_v: texture_2d<f32>;
@group(0) @binding(2) var frame_sampler: sampler;
@group(0) @binding(3) var<uniform> color: ColorUniform;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0)
    );
    let position = positions[index];
    var output: VertexOutput;
    output.position = vec4<f32>(position, 0.0, 1.0);
    var uv = vec2<f32>(position.x * 0.5 + 0.5, 0.5 - position.y * 0.5);
    // Viewer-side rotation in clockwise quarter turns: where each output
    // position samples the upright picture.
    let turns = color.output.y;
    if turns > 2.5 {
        uv = vec2<f32>(1.0 - uv.y, uv.x);
    } else if turns > 1.5 {
        uv = vec2<f32>(1.0 - uv.x, 1.0 - uv.y);
    } else if turns > 0.5 {
        uv = vec2<f32>(uv.y, 1.0 - uv.x);
    }
    output.uv = uv;
    return output;
}

fn srgb_to_linear(value: vec3<f32>) -> vec3<f32> {
    let low = value / 12.92;
    let high = pow((value + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(low, high, value > vec3<f32>(0.04045));
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    var rgb: vec3<f32>;
    if color.values.w > 0.5 && color.values.w < 1.5 {
        rgb = textureSample(plane_y_or_rgba, frame_sampler, input.uv).rgb;
    } else {
        let luma = textureSample(plane_y_or_rgba, frame_sampler, input.uv).r;
        var chroma = textureSample(plane_uv, frame_sampler, input.uv).rg;
        if color.values.w > 1.5 { chroma.y = textureSample(plane_v, frame_sampler, input.uv).r; }
        chroma -= vec2<f32>(128.0/255.0, 128.0/255.0);
        let y = (luma + color.values.x) * color.values.y;
        let u = chroma.x * color.values.z;
        let v = chroma.y * color.values.z;
        rgb = vec3<f32>(
            y + color.coefficients.x * v,
            y + color.coefficients.y * u + color.coefficients.z * v,
            y + color.coefficients.w * u
        );
    }
    rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    if color.output.x > 0.5 {
        rgb = srgb_to_linear(rgb);
    }
    return vec4<f32>(rgb, 1.0);
}
