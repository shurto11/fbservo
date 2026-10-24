// 外部コネクタのモード一覧 (リフレッシュレート付き) と、いま各 CRTC に設定されているモード
#include <fcntl.h>
#include <stdio.h>
#include <xf86drm.h>
#include <xf86drmMode.h>
int main(void) {
    int fd = open("/dev/dri/card1", O_RDWR);
    drmModeRes *r = drmModeGetResources(fd);
    for (int i = 0; i < r->count_crtcs; i++) {
        drmModeCrtc *c = drmModeGetCrtc(fd, r->crtcs[i]);
        if (c && c->mode_valid)
            printf("crtc[%d] id %u: %s %dHz clock %d htotal %d vtotal %d fb %u\n", i, c->crtc_id, c->mode.name,
                   c->mode.vrefresh, c->mode.clock, c->mode.htotal, c->mode.vtotal, c->buffer_id);
        else
            printf("crtc[%d] id %u: off\n", i, r->crtcs[i]);
    }
    for (int i = 0; i < r->count_connectors; i++) {
        drmModeConnector *k = drmModeGetConnector(fd, r->connectors[i]);
        if (!k || k->connection != DRM_MODE_CONNECTED) continue;
        drmModeEncoder *e = k->encoder_id ? drmModeGetEncoder(fd, k->encoder_id) : NULL;
        printf("connector %u type %u-%u crtc %u\n", k->connector_id, k->connector_type, k->connector_type_id, e ? e->crtc_id : 0);
        for (int m = 0; m < k->count_modes; m++) {
            drmModeModeInfo *mi = &k->modes[m];
            printf("  %s %dHz exact %.3fHz%s\n", mi->name, mi->vrefresh,
                   mi->clock * 1000.0 / (mi->htotal * mi->vtotal), (mi->type & DRM_MODE_TYPE_PREFERRED) ? " preferred" : "");
        }
    }
    return 0;
}
