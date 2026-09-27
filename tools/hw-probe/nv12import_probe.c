// Can this EGL implementation import the two planes of a decoded NV12 dmabuf?
//
// ghostframe's GLES H.264 path imports rkvdec's output as two textures --
// DRM_FORMAT_R8 luma at full size, DRM_FORMAT_GR88 chroma at half -- both
// carved out of ONE dmabuf at different offsets. That is the last hardware
// unknown for the import side: GBM already refuses to *allocate* NV12 on
// panfrost (gbmprobe.c), which says nothing about whether EGL will *import*
// R8/GR88.
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl31.h>
#include <GLES2/gl2ext.h>
#include <stdio.h>
#include <string.h>

#ifndef DRM_FORMAT_R8
#define DRM_FORMAT_R8   0x20203852
#define DRM_FORMAT_GR88 0x38385247
#define DRM_FORMAT_NV12 0x3231564e
#endif

static const char *fourcc_str(EGLint f, char *b) {
    b[0] = f & 0xff; b[1] = (f >> 8) & 0xff; b[2] = (f >> 16) & 0xff; b[3] = (f >> 24) & 0xff;
    b[4] = 0; return b;
}

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(d, NULL, NULL)) { printf("eglInitialize failed\n"); return 1; }
    eglBindAPI(EGL_OPENGL_ES_API);

    const char *ext = eglQueryString(d, EGL_EXTENSIONS);
    printf("EGL_EXT_image_dma_buf_import            %s\n",
           strstr(ext, "EGL_EXT_image_dma_buf_import") ? "yes" : "NO");
    printf("EGL_EXT_image_dma_buf_import_modifiers  %s\n",
           strstr(ext, "EGL_EXT_image_dma_buf_import_modifiers") ? "yes" : "NO");
    printf("EGL_MESA_image_dma_buf_export           %s\n\n",
           strstr(ext, "EGL_MESA_image_dma_buf_export") ? "yes" : "NO");

    PFNEGLQUERYDMABUFFORMATSEXTPROC qf =
        (PFNEGLQUERYDMABUFFORMATSEXTPROC)eglGetProcAddress("eglQueryDmaBufFormatsEXT");
    if (!qf) { printf("eglQueryDmaBufFormatsEXT missing -- cannot enumerate\n"); return 1; }

    EGLint n = 0;
    if (!qf(d, 0, NULL, &n)) { printf("format count query failed\n"); return 1; }
    EGLint fmts[256];
    if (n > 256) n = 256;
    if (!qf(d, n, fmts, &n)) { printf("format query failed\n"); return 1; }

    printf("%d importable dmabuf formats. The three that matter:\n", n);
    struct { EGLint f; const char *why; } want[] = {
        { DRM_FORMAT_R8,   "NV12 luma plane" },
        { DRM_FORMAT_GR88, "NV12 chroma plane" },
        { DRM_FORMAT_NV12, "NV12 as a single 2-plane image" },
    };
    char b[5];
    for (unsigned i = 0; i < sizeof(want) / sizeof(want[0]); i++) {
        int found = 0;
        for (EGLint j = 0; j < n; j++) if (fmts[j] == want[i].f) found = 1;
        printf("  %-6s (0x%08x) %-34s %s\n", fourcc_str(want[i].f, b), want[i].f,
               want[i].why, found ? "IMPORTABLE" : "not listed");
    }

    printf("\nall listed formats:");
    for (EGLint j = 0; j < n; j++) printf(" %s", fourcc_str(fmts[j], b));
    printf("\n");
    return 0;
}
