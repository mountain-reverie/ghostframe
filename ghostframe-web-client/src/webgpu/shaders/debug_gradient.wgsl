// Debug-only compute pipeline: writes a known gradient
// (r = x & 255, g = y & 255, b = 0, a = 255) into the framebuffer
// texture so __readPixel can be independently verified.  NOT used in
// production rendering — only window.__readGradientGolden dispatches
// this pipeline.  Lives alongside production codec shaders so the
// build system picks it up via the same wgsl?raw import pattern.

@group(0) @binding(0) var framebuffer: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(8, 8, 1)
fn debug_gradient(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(framebuffer);
    if (gid.x >= dims.x || gid.y >= dims.y) { return; }
    let r = f32(gid.x & 0xFFu) / 255.0;
    let g = f32(gid.y & 0xFFu) / 255.0;
    textureStore(
        framebuffer,
        vec2<i32>(i32(gid.x), i32(gid.y)),
        vec4<f32>(r, g, 0.0, 1.0),
    );
}
