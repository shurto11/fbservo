// vbl <pipe> <回数>: その CRTC の垂直帰線を待ち、カーネルが記録した帰線の時刻 (CLOCK_MONOTONIC) を出す
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <xf86drm.h>
int main(int argc, char **argv) {
    int pipe = atoi(argv[1]), n = atoi(argv[2]);
    int fd = open("/dev/dri/card1", O_RDWR);
    setvbuf(stdout, NULL, _IOLBF, 0); // 止められても、それまでの分は残す
    for (int i = 0; i < n; i++) {
        drmVBlank v = {0};
        v.request.type = DRM_VBLANK_RELATIVE | ((pipe << DRM_VBLANK_HIGH_CRTC_SHIFT) & DRM_VBLANK_HIGH_CRTC_MASK);
        v.request.sequence = 1;
        if (drmWaitVBlank(fd, &v)) { perror("drmWaitVBlank"); return 1; }
        printf("%u %ld.%06ld\n", v.reply.sequence, v.reply.tval_sec, v.reply.tval_usec);
    }
    return 0;
}
