// SPDX-License-Identifier: MIT

#ifndef UGSP_USB_PERSONALITY_FFI_H
#define UGSP_USB_PERSONALITY_FFI_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define UGSP_USB_SPEED_LOW 0
#define UGSP_USB_SPEED_FULL 1
#define UGSP_USB_SPEED_HIGH 2

typedef bool (*ugsp_control_transfer_fn)(
    void *context, const uint8_t setup[8], const uint8_t *out_data,
    size_t out_length, const uint8_t **response, size_t *response_length);

bool ugsp_discover_usb_personality(uint8_t speed,
                                   ugsp_control_transfer_fn transfer,
                                   void *context, uint8_t **output,
                                   size_t *output_length);

void ugsp_personality_cbor_free(uint8_t *bytes, size_t length);

#endif
