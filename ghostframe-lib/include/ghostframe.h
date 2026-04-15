#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

extern int gbridge_new(const char *hostname,
                       const char *authkey,
                       const char *state_dir,
                       const char *control_url,
                       int *sd_out);

extern int gbridge_up(int sd);

extern int gbridge_listen_udp(int sd, const char *addr, int *fd_out);

extern int gbridge_dial_udp(int sd, const char *remote_addr, int *fd_out);

extern int gbridge_close(int sd);

extern int gbridge_getips(int sd, char *buf, uintptr_t buf_len);
