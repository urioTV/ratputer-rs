#ifndef RATPUTER_TUSB_CONFIG_H_
#define RATPUTER_TUSB_CONFIG_H_

#define CFG_TUSB_MCU OPT_MCU_ESP32S3
#define CFG_TUSB_OS OPT_OS_NONE
#define CFG_TUSB_DEBUG 0

#define CFG_TUD_ENABLED 1
#define CFG_TUH_ENABLED 0
#define CFG_TUD_MAX_SPEED OPT_MODE_FULL_SPEED
#define CFG_TUD_ENDPOINT0_SIZE 64

#define CFG_TUD_CDC 0
#define CFG_TUD_MSC 1
#define CFG_TUD_HID 0
#define CFG_TUD_MIDI 0
#define CFG_TUD_VENDOR 0
#define CFG_TUD_MSC_EP_BUFSIZE 512

// Use the DWC2's internal buffer DMA. Espressif makes this the default: slave
// mode is IRQ-driven and cannot reliably drain bulk FIFOs from our cooperative
// polling loop. All USB buffers live in directly accessible internal SRAM; the
// ADV has no cached PSRAM, so ESP-IDF cache-maintenance hooks are unnecessary.
#define CFG_TUD_DWC2_DMA_ENABLE 1
#define CFG_TUSB_MEM_ALIGN __attribute__((aligned(4)))

#endif
