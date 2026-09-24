/*
 * TinyUSB MSC glue for RATPUTER.
 *
 * The TinyUSB device stack and DWC2 controller are polled from Rust.  Sector
 * I/O crosses the FFI boundary back into embedded-sdmmc, keeping all ownership
 * of the physical SD card on the Rust side.
 */
#include <stdbool.h>
#include <stdint.h>
#include <string.h>
#include "tusb.h"

extern uint32_t ratputer_sd_block_count(void);
extern int32_t ratputer_sd_read(uint32_t lba, uint32_t offset, void *buffer, uint32_t length);
extern int32_t ratputer_sd_write(uint32_t lba, uint32_t offset, const void *buffer, uint32_t length);

static bool disk_active;
static bool disk_ejected;

bool ratputer_tinyusb_init(void) {
    const tusb_rhport_init_t init = {
        .role = TUSB_ROLE_DEVICE,
        .speed = TUSB_SPEED_FULL,
    };
    if (!tusb_init(0, &init)) {
        return false;
    }
    tud_disconnect();
    disk_active = false;
    disk_ejected = false;
    return true;
}

void ratputer_tinyusb_poll(void) {
    if (!disk_active) {
        return;
    }
    // No RTOS/interrupt allocator is linked. Service pending DWC2 status bits
    // and then drain TinyUSB's device-event queue cooperatively.
    dcd_int_handler(0);
    tud_task_ext(0, false);
}

void ratputer_tinyusb_connect(void) {
    disk_ejected = false;
    disk_active = true;
    tud_connect();
}

void ratputer_tinyusb_disconnect(void) {
    tud_disconnect();
    disk_active = false;
}

// 0 inactive, 1 waiting for host, 2 configured/mounted, 3 safely ejected.
uint8_t ratputer_tinyusb_state(void) {
    if (!disk_active) return 0;
    if (disk_ejected) return 3;
    return tud_mounted() ? 2 : 1;
}

bool ratputer_tinyusb_can_disconnect(void) {
    return !tud_mounted() || disk_ejected;
}

// --------------------------------------------------------------------------
// USB descriptors
// --------------------------------------------------------------------------
#define USB_VID 0xCAFE
#define USB_PID 0x4002
#define EPNUM_MSC_OUT 0x01
#define EPNUM_MSC_IN  0x81

enum { ITF_NUM_MSC, ITF_NUM_TOTAL };
#define CONFIG_TOTAL_LEN (TUD_CONFIG_DESC_LEN + TUD_MSC_DESC_LEN)

static const tusb_desc_device_t device_descriptor = {
    .bLength = sizeof(tusb_desc_device_t),
    .bDescriptorType = TUSB_DESC_DEVICE,
    .bcdUSB = 0x0200,
    .bDeviceClass = 0,
    .bDeviceSubClass = 0,
    .bDeviceProtocol = 0,
    .bMaxPacketSize0 = CFG_TUD_ENDPOINT0_SIZE,
    .idVendor = USB_VID,
    .idProduct = USB_PID,
    .bcdDevice = 0x0100,
    .iManufacturer = 1,
    .iProduct = 2,
    .iSerialNumber = 3,
    .bNumConfigurations = 1,
};

static const uint8_t configuration_descriptor[] = {
    TUD_CONFIG_DESCRIPTOR(1, ITF_NUM_TOTAL, 0, CONFIG_TOTAL_LEN, 0, 100),
    TUD_MSC_DESCRIPTOR(ITF_NUM_MSC, 0, EPNUM_MSC_OUT, EPNUM_MSC_IN, 64),
};

const uint8_t *tud_descriptor_device_cb(void) {
    return (const uint8_t *)&device_descriptor;
}

const uint8_t *tud_descriptor_configuration_cb(uint8_t index) {
    (void)index;
    return configuration_descriptor;
}

static const char *const string_descriptors[] = {
    (const char[]){0x09, 0x04},
    "RATPUTER",
    "RATPUTER SD",
    "RATPUTER-ADV",
};
static uint16_t string_descriptor[32 + 1];

const uint16_t *tud_descriptor_string_cb(uint8_t index, uint16_t langid) {
    (void)langid;
    uint8_t count;
    if (index == 0) {
        memcpy(&string_descriptor[1], string_descriptors[0], 2);
        count = 1;
    } else {
        if (index >= (sizeof string_descriptors / sizeof string_descriptors[0])) return NULL;
        const char *text = string_descriptors[index];
        size_t length = strlen(text);
        if (length > 32) length = 32;
        count = (uint8_t)length;
        for (uint8_t i = 0; i < count; ++i) string_descriptor[1 + i] = (uint8_t)text[i];
    }
    string_descriptor[0] = (uint16_t)((TUSB_DESC_STRING << 8) | (2 * count + 2));
    return string_descriptor;
}

// --------------------------------------------------------------------------
// MSC / SCSI callbacks
// --------------------------------------------------------------------------
uint8_t tud_msc_get_maxlun_cb(void) { return 0; }

uint32_t tud_msc_inquiry2_cb(uint8_t lun, scsi_inquiry_resp_t *response, uint32_t size) {
    (void)lun;
    (void)size;
    memcpy(response->vendor_id, "RATPUTER", 8);
    memcpy(response->product_id, "SD CARD         ", 16);
    memcpy(response->product_rev, "1.0 ", 4);
    return sizeof(scsi_inquiry_resp_t);
}

bool tud_msc_test_unit_ready_cb(uint8_t lun) {
    (void)lun;
    if (!disk_active || disk_ejected || ratputer_sd_block_count() == 0) {
        tud_msc_set_sense(lun, SCSI_SENSE_NOT_READY, 0x3a, 0x00);
        return false;
    }
    return true;
}

void tud_msc_capacity_cb(uint8_t lun, uint32_t *block_count, uint16_t *block_size) {
    (void)lun;
    *block_count = ratputer_sd_block_count();
    *block_size = 512;
}

bool tud_msc_start_stop_cb(uint8_t lun, uint8_t power_condition, bool start, bool load_eject) {
    (void)lun;
    (void)power_condition;
    if (load_eject && !start) disk_ejected = true;
    if (load_eject && start) disk_ejected = false;
    return true;
}

bool tud_msc_is_writable_cb(uint8_t lun) {
    (void)lun;
    return disk_active && !disk_ejected;
}

int32_t tud_msc_read10_cb(uint8_t lun, uint32_t lba, uint32_t offset,
                          void *buffer, uint32_t size) {
    (void)lun;
    return ratputer_sd_read(lba, offset, buffer, size);
}

int32_t tud_msc_write10_cb(uint8_t lun, uint32_t lba, uint32_t offset,
                           uint8_t *buffer, uint32_t size) {
    (void)lun;
    return ratputer_sd_write(lba, offset, buffer, size);
}

int32_t tud_msc_scsi_cb(uint8_t lun, const uint8_t command[16],
                        void *buffer, uint16_t size) {
    (void)buffer;
    (void)size;
    // SYNCHRONIZE CACHE (10): embedded-sdmmc writes sectors synchronously.
    if (command[0] == 0x35) return 0;
    tud_msc_set_sense(lun, SCSI_SENSE_ILLEGAL_REQUEST, 0x20, 0x00);
    return -1;
}
