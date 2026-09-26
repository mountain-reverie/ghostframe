#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl31.h>
#include <stdio.h>

static void q(const char *n, GLenum e) {
    GLint v = -1; glGetError();
    glGetIntegerv(e, &v);
    if (glGetError() != GL_NO_ERROR) printf("%-46s UNSUPPORTED\n", n);
    else printf("%-46s %d\n", n, v);
}
static void qi(const char *n, GLenum e, GLuint i) {
    GLint v = -1; glGetError();
    glGetIntegeri_v(e, i, &v);
    if (glGetError() != GL_NO_ERROR) printf("%-46s UNSUPPORTED\n", n);
    else printf("%-46s %d\n", n, v);
}

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(d, NULL, NULL)) { printf("eglInitialize failed\n"); return 1; }
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint cfgattr[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                         EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_NONE };
    EGLConfig cfg; EGLint n;
    if (!eglChooseConfig(d, cfgattr, &cfg, 1, &n) || n < 1) { printf("no config\n"); return 1; }
    EGLint ctxattr[] = { EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 1, EGL_NONE };
    EGLContext c = eglCreateContext(d, cfg, EGL_NO_CONTEXT, ctxattr);
    if (c == EGL_NO_CONTEXT) { printf("no GLES 3.1 context\n"); return 1; }
    if (!eglMakeCurrent(d, EGL_NO_SURFACE, EGL_NO_SURFACE, c)) { printf("makeCurrent failed\n"); return 1; }

    printf("GL_VERSION   %s\n", glGetString(GL_VERSION));
    printf("GL_RENDERER  %s\n\n", glGetString(GL_RENDERER));

    puts("--- storage buffers (ghostframe needs 7 in one compute stage) ---");
    q("GL_MAX_COMPUTE_SHADER_STORAGE_BLOCKS",      GL_MAX_COMPUTE_SHADER_STORAGE_BLOCKS);
    q("GL_MAX_SHADER_STORAGE_BUFFER_BINDINGS",     GL_MAX_SHADER_STORAGE_BUFFER_BINDINGS);
    q("GL_MAX_FRAGMENT_SHADER_STORAGE_BLOCKS",     GL_MAX_FRAGMENT_SHADER_STORAGE_BLOCKS);
    q("GL_MAX_COMBINED_SHADER_STORAGE_BLOCKS",     GL_MAX_COMBINED_SHADER_STORAGE_BLOCKS);

    puts("\n--- compute dispatch (wgpu downlevel wants 256 invocations) ---");
    q("GL_MAX_COMPUTE_WORK_GROUP_INVOCATIONS",     GL_MAX_COMPUTE_WORK_GROUP_INVOCATIONS);
    qi("GL_MAX_COMPUTE_WORK_GROUP_SIZE[0]",        GL_MAX_COMPUTE_WORK_GROUP_SIZE, 0);
    qi("GL_MAX_COMPUTE_WORK_GROUP_SIZE[1]",        GL_MAX_COMPUTE_WORK_GROUP_SIZE, 1);
    qi("GL_MAX_COMPUTE_WORK_GROUP_COUNT[0]",       GL_MAX_COMPUTE_WORK_GROUP_COUNT, 0);
    q("GL_MAX_COMPUTE_SHARED_MEMORY_SIZE",         GL_MAX_COMPUTE_SHARED_MEMORY_SIZE);

    puts("\n--- storage images / textures ---");
    q("GL_MAX_COMPUTE_IMAGE_UNIFORMS",             GL_MAX_COMPUTE_IMAGE_UNIFORMS);
    q("GL_MAX_IMAGE_UNITS",                        GL_MAX_IMAGE_UNITS);
    q("GL_MAX_COMPUTE_UNIFORM_BLOCKS",             GL_MAX_COMPUTE_UNIFORM_BLOCKS);
    q("GL_MAX_COMPUTE_TEXTURE_IMAGE_UNITS",        GL_MAX_COMPUTE_TEXTURE_IMAGE_UNITS);
    q("GL_MAX_TEXTURE_SIZE",                       GL_MAX_TEXTURE_SIZE);
    return 0;
}
