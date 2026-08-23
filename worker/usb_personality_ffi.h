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

struct ugsp_bytes {
  const uint8_t *data;
  size_t length;
};

struct ugsp_usb_device {
  uint16_t usb_version;
  uint16_t vendor_id;
  uint16_t product_id;
  uint16_t device_version;
  uint8_t device_class;
  uint8_t device_subclass;
  uint8_t device_protocol;
  uint8_t max_packet_size_0;
  const char *manufacturer;
  const char *product;
  const char *serial_number;
  const char *interface_name;
};

struct ugsp_usb_interface {
  uint8_t number;
  uint8_t class_code;
  uint8_t subclass;
  uint8_t protocol;
  uint8_t string_index;
  uint8_t endpoint_in;
  uint8_t endpoint_out;
  uint8_t transfer_type;
  uint16_t max_packet_size;
  uint8_t interval;
  struct ugsp_bytes class_descriptors;
};

struct ugsp_personality_builder;

struct ugsp_personality_builder *ugsp_personality_builder_new(
    uint8_t speed, const struct ugsp_usb_device *device);
bool ugsp_personality_builder_add_interface(
    struct ugsp_personality_builder *builder,
    const struct ugsp_usb_interface *interface);
bool ugsp_personality_builder_set_serial_number(
    struct ugsp_personality_builder *builder, const char *serial_number);
bool ugsp_personality_builder_add_microsoft_compatible_id(
    struct ugsp_personality_builder *builder, uint8_t vendor_code,
    uint8_t interface, const char *compatible_id,
    const char *sub_compatible_id);
bool ugsp_personality_builder_set_webusb(
    struct ugsp_personality_builder *builder, uint8_t enabled,
    uint16_t version, uint8_t vendor_code, const char *landing_page);
bool ugsp_personality_builder_finish(
    const struct ugsp_personality_builder *builder, uint8_t **output,
    size_t *output_length);
void ugsp_personality_builder_free(struct ugsp_personality_builder *builder);

bool ugsp_discover_usb_personality(uint8_t speed,
                                   ugsp_control_transfer_fn transfer,
                                   void *context, uint8_t **output,
                                   size_t *output_length);

void ugsp_personality_cbor_free(uint8_t *bytes, size_t length);

#endif
