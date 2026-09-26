#include <linux/videodev2.h>
#include <sys/ioctl.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>

#ifndef V4L2_PIX_FMT_H264_SLICE
#define V4L2_PIX_FMT_H264_SLICE v4l2_fourcc('S','2','6','4')
#endif

int main(void) {
    int fd = open("/dev/video3", O_RDWR | O_CLOEXEC);
    if (fd < 0) { perror("open /dev/video3"); return 1; }

    // 1. OUTPUT queue: H.264 parsed slices in.
    struct v4l2_format of; memset(&of, 0, sizeof of);
    of.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
    of.fmt.pix_mp.pixelformat = V4L2_PIX_FMT_H264_SLICE;
    of.fmt.pix_mp.width = 640; of.fmt.pix_mp.height = 480;
    of.fmt.pix_mp.num_planes = 1;
    of.fmt.pix_mp.plane_fmt[0].sizeimage = 1024 * 1024;
    if (ioctl(fd, VIDIOC_S_FMT, &of) < 0) { perror("S_FMT output S264"); return 1; }
    printf("OUTPUT  set: S264 %ux%u\n", of.fmt.pix_mp.width, of.fmt.pix_mp.height);

    // 2. CAPTURE queue: NV12 out. Driver picks strides.
    struct v4l2_format cf; memset(&cf, 0, sizeof cf);
    cf.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    cf.fmt.pix_mp.pixelformat = V4L2_PIX_FMT_NV12;
    cf.fmt.pix_mp.width = 640; cf.fmt.pix_mp.height = 480;
    if (ioctl(fd, VIDIOC_S_FMT, &cf) < 0) { perror("S_FMT capture NV12"); return 1; }
    printf("CAPTURE set: NV12 %ux%u planes=%u\n", cf.fmt.pix_mp.width,
           cf.fmt.pix_mp.height, cf.fmt.pix_mp.num_planes);
    for (unsigned i = 0; i < cf.fmt.pix_mp.num_planes; i++)
        printf("   plane %u: bytesperline=%u sizeimage=%u\n", i,
               cf.fmt.pix_mp.plane_fmt[i].bytesperline,
               cf.fmt.pix_mp.plane_fmt[i].sizeimage);

    // 3. Driver-allocated MMAP buffers (vb2 dma_contig, what rkvdec needs).
    struct v4l2_requestbuffers rb; memset(&rb, 0, sizeof rb);
    rb.count = 4; rb.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE; rb.memory = V4L2_MEMORY_MMAP;
    if (ioctl(fd, VIDIOC_REQBUFS, &rb) < 0) { perror("REQBUFS MMAP"); return 1; }
    printf("REQBUFS MMAP: got %u buffers\n", rb.count);

    // 4. THE POINT: export each as a dmabuf fd.
    int ok = 0;
    for (unsigned i = 0; i < rb.count; i++) {
        for (unsigned p = 0; p < cf.fmt.pix_mp.num_planes; p++) {
            struct v4l2_exportbuffer eb; memset(&eb, 0, sizeof eb);
            eb.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
            eb.index = i; eb.plane = p; eb.flags = O_CLOEXEC | O_RDWR;
            if (ioctl(fd, VIDIOC_EXPBUF, &eb) < 0) {
                printf("  EXPBUF buf %u plane %u: FAILED (%s)\n", i, p, strerror(errno));
            } else {
                printf("  EXPBUF buf %u plane %u -> dmabuf fd %d\n", i, p, eb.fd);
                close(eb.fd); ok++;
            }
        }
    }
    printf("\n%d dmabuf fds exported\n", ok);
    close(fd);
    return ok ? 0 : 1;
}
