// SPDX-License-Identifier: MIT OR Apache-2.0
// Prints the struct sizes and offsets openh264_sys.rs pins (its
// `openh264_layouts_match_the_c_headers` test). Build it against each
// release's headers; nothing here is built into the daemon.
//
//   for tag in v2.4.1 v2.6.0; do
//     mkdir -p "$tmp/$tag/wels"
//     for f in codec_api codec_app_def codec_def codec_ver; do
//       curl -sfL -o "$tmp/$tag/wels/$f.h" \
//         "https://raw.githubusercontent.com/cisco/openh264/$tag/codec/api/wels/$f.h"
//     done
//     gcc -I "$tmp/$tag" layout.c -o "$tmp/layout-$tag" && "$tmp/layout-$tag"
//   done
//
// (v2.5.0 has the v2.4.1 layout; v2.6.0 appended bPsnrY/U/V to SEncParamExt
// and SSourcePicture and float rPsnr[3] to SLayerBSInfo.)
#include <stddef.h>
#include <stdio.h>
#include <wels/codec_api.h>
#include <wels/codec_ver.h>
#define P(t, f) printf(#t "." #f " = %zu\n", offsetof(t, f))
int main(void) {
  printf("OpenH264 %d.%d\n", OPENH264_MAJOR, OPENH264_MINOR);
  printf("sizeof SEncParamExt=%zu SSpatialLayerConfig=%zu SSourcePicture=%zu SLayerBSInfo=%zu "
         "SFrameBSInfo=%zu SSliceArgument=%zu\n",
         sizeof(SEncParamExt), sizeof(SSpatialLayerConfig), sizeof(SSourcePicture),
         sizeof(SLayerBSInfo), sizeof(SFrameBSInfo), sizeof(SSliceArgument));
  P(SEncParamExt, iComplexityMode); P(SEncParamExt, bEnableFrameSkip);
  P(SEncParamExt, bEnableLongTermReference); P(SEncParamExt, iLtrMarkPeriod);
  P(SEncParamExt, iMultipleThreadIdc); P(SEncParamExt, bEnableSceneChangeDetect);
  P(SEncParamExt, iIdrBitrateRatio);
  P(SSpatialLayerConfig, sSliceArgument);
  P(SSourcePicture, pData); P(SSourcePicture, uiTimeStamp);
  P(SLayerBSInfo, pBsBuf);
  P(SFrameBSInfo, sLayerInfo); P(SFrameBSInfo, eFrameType); P(SFrameBSInfo, iFrameSizeInBytes);
  return 0;
}
