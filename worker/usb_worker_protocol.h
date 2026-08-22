// SPDX-License-Identifier: MIT

#ifndef UGSP_USB_WORKER_PROTOCOL_H
#define UGSP_USB_WORKER_PROTOCOL_H

#include <stdint.h>

#define UGSP_USB_BUS_EVENT_BODY_LENGTH 9U

static inline uint64_t ugsp_read_be64(const uint8_t bytes[8]) {
  return ((uint64_t)bytes[0] << 56) | ((uint64_t)bytes[1] << 48) |
         ((uint64_t)bytes[2] << 40) | ((uint64_t)bytes[3] << 32) |
         ((uint64_t)bytes[4] << 24) | ((uint64_t)bytes[5] << 16) |
         ((uint64_t)bytes[6] << 8) | bytes[7];
}

#endif
