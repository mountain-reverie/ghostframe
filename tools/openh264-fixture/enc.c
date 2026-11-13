// SPDX-License-Identifier: MIT OR Apache-2.0
// Produces the openh264 LTR test stream (see make.sh here).
//
// 128x128, camera usage (screen content silently disables LTR), scene-change
// detection off (it makes unrequested IDRs), a short LTR mark period, the
// marking feedback a real client would send (carrying the MARKED picture's
// frame_num), and scripted recovery requests.
//
// usage: enc OUT.h264 LIB.so FRAMES MARK_PERIOD FB_DELAY RECOVERY_AT[,RECOVERY_AT...]
//   The marking feedback names the MARKED picture's frame_num (MMCO 6: the
//   carrier itself; MMCO 3: the carrier's predecessor). The recovery request's
//   iLastCorrectFrameNum must name the CARRIER's frame_num instead: with the
//   marked picture's number openh264 treats an MMCO-3 LT as invalid and
//   answers with an IDR.
//   A recovery at frame R means: the previous frames are treated as lost back
//   to the newest confirmed LT; the request names that LT as the last correct
//   picture and the newest encoded frame_num as the current one, and frame R is
//   the recovery P.
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wels/codec_api.h>

typedef int (*cre_t)(ISVCEncoder **);
typedef long (*crd_t)(ISVCDecoder **);

#define W 128
#define H 128

static void content(unsigned char *y, unsigned char *u, unsigned char *v, int i) {
  for (int r = 0; r < H; r++)
    for (int c = 0; c < W; c++) y[r * W + c] = (unsigned char)(30 + (r + c) / 3 + i * 3 / 2);
  // The whole picture brightens every frame, so a wrong reference leaves an
  // error no later frame repairs (a decoder that is off stays off), and the
  // text rows below are static.
  for (int r = 0; r < 16; r++)
    for (int c = 0; c < W; c++)
      if ((r / 4) % 2 == 0 && (c / 3) % 2 == 0) y[r * W + c] = 235;
  // a moving textured block, so every picture differs from every other
  int bx = (i * 5) % (W - 24), by = 24 + (i * 3) % (H - 24 - 16);
  for (int r = 0; r < 16; r++)
    for (int c = 0; c < 24; c++)
      y[(by + r) * W + bx + c] = (unsigned char)(90 + (((c ^ r) + i * 7) & 63));
  memset(u, 128, W * H / 4);
  memset(v, 128, W * H / 4);
  for (int r = 0; r < 8; r++)
    for (int c = 0; c < 12; c++) u[((by + r * 2) / 2) * (W / 2) + bx / 2 + c] = (unsigned char)(100 + (i & 31));
}

int main(int argc, char **argv) {
  if (argc < 7) { fprintf(stderr, "usage: enc OUT LIB FRAMES MARK FB_DELAY R1[,R2...]\n"); return 1; }
  const char *out = argv[1];
  int n = atoi(argv[3]), period = atoi(argv[4]), fbdelay = atoi(argv[5]);
  int recov[16], nrec = 0;
  for (char *s = strtok(argv[6], ","); s && nrec < 16; s = strtok(NULL, ",")) recov[nrec++] = atoi(s);

  void *h = dlopen(argv[2], RTLD_NOW);
  if (!h) { fprintf(stderr, "dlopen %s failed\n", argv[2]); return 1; }
  cre_t cre = dlsym(h, "WelsCreateSVCEncoder");
  crd_t crd = dlsym(h, "WelsCreateDecoder");
  OpenH264Version (*gv)(void) = dlsym(h, "WelsGetCodecVersion");
  OpenH264Version v = gv();
  fprintf(stderr, "openh264 %u.%u.%u\n", v.uMajor, v.uMinor, v.uRevision);

  ISVCEncoder *e; cre(&e);
  SEncParamExt p; (*e)->GetDefaultParams(e, &p);
  p.iUsageType = CAMERA_VIDEO_REAL_TIME;
  p.iPicWidth = W; p.iPicHeight = H;
  p.iTargetBitrate = 100000; p.iMaxBitrate = 100000;
  p.iRCMode = RC_BITRATE_MODE; p.fMaxFrameRate = 30;
  p.iTemporalLayerNum = 1; p.iSpatialLayerNum = 1;
  p.bEnableFrameSkip = false; p.uiIntraPeriod = 0; p.iMultipleThreadIdc = 1;
  p.bEnableLongTermReference = 1; p.iLTRRefNum = 2; p.iLtrMarkPeriod = period;
  p.bEnableSceneChangeDetect = false;
  p.eSpsPpsIdStrategy = CONSTANT_ID;
  p.iEntropyCodingModeFlag = 1;
  p.sSpatialLayers[0].iVideoWidth = W; p.sSpatialLayers[0].iVideoHeight = H;
  p.sSpatialLayers[0].fFrameRate = 30;
  p.sSpatialLayers[0].iSpatialBitrate = 100000; p.sSpatialLayers[0].iMaxSpatialBitrate = 100000;
  p.sSpatialLayers[0].sSliceArgument.uiSliceMode = SM_SINGLE_SLICE;
  if ((*e)->InitializeExt(e, &p)) { fprintf(stderr, "InitializeExt failed\n"); return 2; }
  int fmt = videoFormatI420; (*e)->SetOption(e, ENCODER_OPTION_DATAFORMAT, &fmt);

  // openh264's own decoder tells us when a picture carried a mark and which
  // frame_num it marked: the feedback a client would send.
  ISVCDecoder *d; crd(&d);
  SDecodingParam dp; memset(&dp, 0, sizeof dp);
  dp.sVideoProperty.eVideoBsType = VIDEO_BITSTREAM_AVC;
  (*d)->Initialize(d, &dp);

  static unsigned char y[W * H], u[W * H / 4], vv[W * H / 4];
  FILE *f = fopen(out, "wb");
  int fb_at[256], fb_num[256], fb_car[256], fb_idr[256], fb_n = 0, lastok = 0, lastcar = 0, next_rec = 0;
  unsigned idrid = 0;
  int newest_fn = 0;
  for (int i = 0; i < n; i++) {
    content(y, u, vv, i);
    SSourcePicture pic; memset(&pic, 0, sizeof pic);
    pic.iPicWidth = W; pic.iPicHeight = H; pic.uiTimeStamp = i * 33;
    pic.iColorFormat = videoFormatI420;
    pic.iStride[0] = W; pic.iStride[1] = W / 2; pic.iStride[2] = W / 2;
    pic.pData[0] = y; pic.pData[1] = u; pic.pData[2] = vv;

    // A recovery request goes in before the frame it should shape.
    if (next_rec < nrec && recov[next_rec] == i) {
      SLTRRecoverRequest rq; memset(&rq, 0, sizeof rq);
      rq.uiFeedbackType = LTR_RECOVERY_REQUEST;
      rq.uiIDRPicId = idrid;
      rq.iLastCorrectFrameNum = lastcar;  // the CARRIER's frame_num,
      rq.iCurrentFrameNum = newest_fn;  // the NEWEST frame_num encoded
      int rr = (*e)->SetOption(e, ENCODER_LTR_RECOVERY_REQUEST, &rq);
      fprintf(stderr, "frame %d: recovery last_correct=%d (marked %d) current=%d -> %d\n", i, lastcar, lastok, newest_fn, rr);
      next_rec++;
    }

    SFrameBSInfo fi; memset(&fi, 0, sizeof fi);
    int r = (*e)->EncodeFrame(e, &pic, &fi);
    size_t sz = 0;
    for (int l = 0; l < fi.iLayerNum; l++)
      for (int k = 0; k < fi.sLayerInfo[l].iNalCount; k++) sz += fi.sLayerInfo[l].pNalLengthInByte[k];
    if (r || !sz || fi.eFrameType == videoFrameTypeSkip || fi.eFrameType == videoFrameTypeInvalid) {
      fprintf(stderr, "frame %d: encode rc=%d size=%zu type=%d\n", i, r, sz, fi.eFrameType);
      return 3;
    }
    // One layer holds the whole AU for a single slice, camera, one spatial layer.
    fwrite(fi.sLayerInfo[0].pBsBuf, 1, sz, f);

    unsigned char *dst[3] = {0};
    SBufferInfo bi; memset(&bi, 0, sizeof bi);
    (*d)->DecodeFrameNoDelay(d, fi.sLayerInfo[0].pBsBuf, sz, dst, &bi);
    int mark = 0, mnum = -1; unsigned idr = 0;
    (*d)->GetOption(d, DECODER_OPTION_LTR_MARKING_FLAG, &mark);
    (*d)->GetOption(d, DECODER_OPTION_LTR_MARKED_FRAME_NUM, &mnum);
    (*d)->GetOption(d, DECODER_OPTION_IDR_PIC_ID, &idr);
    if (fi.eFrameType == videoFrameTypeIDR) idrid = idr;
    (*d)->GetOption(d, DECODER_OPTION_FRAME_NUM, &newest_fn);
    fprintf(stderr, "f%02d type=%d size=%zu mark=%d marked_fn=%d fn=%d\n", i, fi.eFrameType, sz, mark, mnum, newest_fn);
    if (mark) { fb_at[fb_n] = i + fbdelay; fb_num[fb_n] = mnum; fb_car[fb_n] = newest_fn; fb_idr[fb_n] = idr; fb_n++; }
    for (int k = 0; k < fb_n; k++)
      if (fb_at[k] == i) {
        SLTRMarkingFeedback fb; memset(&fb, 0, sizeof fb);
        fb.uiFeedbackType = LTR_MARKING_SUCCESS; fb.uiIDRPicId = fb_idr[k]; fb.iLTRFrameNum = fb_num[k];
        int rr = (*e)->SetOption(e, ENCODER_LTR_MARKING_FEEDBACK, &fb);
        fprintf(stderr, "   feedback marked_fn=%d -> %d\n", fb_num[k], rr);
        lastok = fb_num[k]; lastcar = fb_car[k];
      }
  }
  fclose(f);
  return 0;
}
