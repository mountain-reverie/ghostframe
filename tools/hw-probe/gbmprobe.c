#include <gbm.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

static void t(struct gbm_device *d, const char *label, uint32_t fmt, uint32_t flags) {
    struct gbm_bo *bo = gbm_bo_create(d, 640, 480, fmt, flags);
    if (bo) {
        printf("%-52s OK   planes=%d modifier=0x%llx stride=%u\n", label,
               gbm_bo_get_plane_count(bo),
               (unsigned long long)gbm_bo_get_modifier(bo),
               gbm_bo_get_stride(bo));
        gbm_bo_destroy(bo);
    } else {
        printf("%-52s FAIL\n", label);
    }
}

int main(void) {
    int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    if (fd < 0) { perror("open renderD128"); return 1; }
    struct gbm_device *d = gbm_create_device(fd);
    if (!d) { printf("gbm_create_device failed\n"); return 1; }
    printf("gbm backend: %s\n\n", gbm_device_get_backend_name(d));

    const uint32_t NV12 = GBM_FORMAT_NV12;
    printf("NV12 supported (render): %d\n", gbm_device_is_format_supported(d, NV12, GBM_BO_USE_RENDERING));
    printf("NV12 supported (linear): %d\n", gbm_device_is_format_supported(d, NV12, GBM_BO_USE_LINEAR));
    printf("XRGB supported (render): %d\n\n", gbm_device_is_format_supported(d, GBM_FORMAT_XRGB8888, GBM_BO_USE_RENDERING));

    t(d, "NV12 flags=GBM_BO_USE_HW_VIDEO_DECODER (1<<13)", NV12, 1u << 13);
    t(d, "NV12 flags=GBM_BO_USE_LINEAR", NV12, GBM_BO_USE_LINEAR);
    t(d, "NV12 flags=GBM_BO_USE_RENDERING", NV12, GBM_BO_USE_RENDERING);
    t(d, "NV12 flags=LINEAR|RENDERING", NV12, GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING);
    t(d, "NV12 flags=0", NV12, 0);
    t(d, "XRGB8888 flags=LINEAR|RENDERING", GBM_FORMAT_XRGB8888, GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING);

    // wgpu's Rgba8Unorm is DRM_FORMAT_ABGR8888. The export path needs THIS
    // format linear, not XRGB -- a linear buffer in the wrong channel order
    // would render as swapped colours rather than failing.
    printf("\nABGR8888 supported (render): %d\n", gbm_device_is_format_supported(d, GBM_FORMAT_ABGR8888, GBM_BO_USE_RENDERING));
    t(d, "ABGR8888 flags=LINEAR|RENDERING", GBM_FORMAT_ABGR8888, GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING);
    t(d, "ABGR8888 flags=LINEAR|RENDERING|SCANOUT", GBM_FORMAT_ABGR8888, GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING | GBM_BO_USE_SCANOUT);
    t(d, "ARGB8888 flags=LINEAR|RENDERING", GBM_FORMAT_ARGB8888, GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING);

    gbm_device_destroy(d); close(fd);
    return 0;
}
