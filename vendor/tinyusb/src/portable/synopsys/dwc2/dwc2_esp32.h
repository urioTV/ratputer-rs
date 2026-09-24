/*
 * Minimal bare-metal ESP32-S3 port for TinyUSB's Synopsys DWC2 driver.
 *
 * Derived from TinyUSB 0.21.0's dwc2_esp32.h (MIT).  The upstream
 * Espressif port delegates interrupt allocation and delays to ESP-IDF/
 * FreeRTOS.  RATPUTER polls dcd_int_handler() from its Rust main loop, so
 * those hooks are deliberately no-ops and no ESP-IDF headers are needed.
 */
#ifndef TUSB_DWC2_ESP32_H_
#define TUSB_DWC2_ESP32_H_

#ifdef __cplusplus
extern "C" {
#endif

#define DWC2_FS_REG_BASE 0x60080000UL
#define DWC2_EP_MAX 7

static const dwc2_controller_t _dwc2_controller[] = {
    {
        .reg_base = DWC2_FS_REG_BASE,
        .irqnum = 0,
        .ep_count = 7,
        .ep_in_count = 5,
        .otg_dfifo_depth = 256,
    },
};

TU_ATTR_ALWAYS_INLINE static inline void dwc2_clock_init(uint8_t rhport, tusb_role_t role) {
    (void)rhport;
    (void)role;
    // esp-hal's PeripheralGuard enables the USB_FS clock before TinyUSB starts.
}

#define dwc2_dcd_int_enable(_rhport)  ((void)(_rhport))
#define dwc2_dcd_int_disable(_rhport) ((void)(_rhport))

TU_ATTR_ALWAYS_INLINE static inline void dwc2_remote_wakeup_delay(void) {
    // Remote wake-up is not advertised by RATPUTER. Keep a short bounded delay
    // in case a host still reaches this path.
    for (volatile uint32_t i = 0; i < 240000; ++i) {
        __asm__ volatile ("nop");
    }
}

TU_ATTR_ALWAYS_INLINE static inline void dwc2_phy_init(dwc2_regs_t* dwc2, uint8_t hs_phy_type) {
    (void)dwc2;
    (void)hs_phy_type;
}

TU_ATTR_ALWAYS_INLINE static inline void dwc2_phy_deinit(dwc2_regs_t* dwc2, uint8_t hs_phy_type) {
    (void)dwc2;
    (void)hs_phy_type;
}

TU_ATTR_ALWAYS_INLINE static inline void dwc2_phy_update(dwc2_regs_t* dwc2, uint8_t hs_phy_type) {
    (void)dwc2;
    (void)hs_phy_type;
}

#ifdef __cplusplus
}
#endif
#endif
