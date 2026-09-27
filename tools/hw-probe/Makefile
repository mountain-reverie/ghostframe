# Hardware probes for the GLES / V4L2-stateless client port.
# See README.md, and docs/superpowers/specs/2026-09-25-native-client-gles-v4l2-design.md
#
# Not part of the workspace build: these are diagnostics you run by hand on a
# target machine, not something ghostframe links.

CFLAGS ?= -O1 -Wall -Wextra

PROBES = glprobe gbmprobe expbuf_probe nv12import_probe

.PHONY: all clean run
all: $(PROBES)

glprobe: glprobe.c
	$(CC) $(CFLAGS) $< -o $@ -lEGL -lGLESv2

nv12import_probe: nv12import_probe.c
	$(CC) $(CFLAGS) $< -o $@ -lEGL -lGLESv2

gbmprobe: gbmprobe.c
	$(CC) $(CFLAGS) $< -o $@ $(shell pkg-config --cflags --libs gbm)

# No libraries: raw V4L2 ioctls against uapi headers only.
expbuf_probe: expbuf_probe.c
	$(CC) $(CFLAGS) $< -o $@

# Print everything the design doc's "Measured baseline" table records.
run: $(PROBES)
	@echo "=========== glprobe (GLES limits, EGL dmabuf) ==========="; ./glprobe || true
	@echo; echo "=========== gbmprobe (can GBM allocate NV12?) ==========="; ./gbmprobe || true
	@echo; echo "=========== expbuf_probe (driver dmabuf export) ==========="; ./expbuf_probe || true
	@echo; echo "=========== nv12import_probe (can EGL import NV12 planes?) ==========="; ./nv12import_probe || true

clean:
	rm -f $(PROBES)
