//! Battery gauge: GPIO10 (ADC1_CH9) behind a 100k/100k divider (BAT+ / 2),
//! read with eFuse curve calibration and mapped to a 1S Li-ion charge estimate.
//! ADC1 is safe to use while Wi-Fi is active (ADC2 is not).

use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcConfig, AdcPin, Attenuation};
use esp_hal::peripherals::{ADC1, GPIO10};
use esp_hal::Blocking;

const SAMPLES: u32 = 8;
// A oneshot conversion takes microseconds; bail out rather than hang the UI loop.
const MAX_POLLS_PER_SAMPLE: u32 = 100_000;
const DIVIDER_RATIO: u32 = 2;

// Resting-voltage discharge curve of a 1S Li-ion cell: (mV, %), descending.
const DISCHARGE_CURVE: [(u32, u32); 11] = [
    (4150, 100),
    (4060, 90),
    (3980, 80),
    (3920, 70),
    (3870, 60),
    (3820, 50),
    (3790, 40),
    (3770, 30),
    (3740, 20),
    (3680, 10),
    (3400, 0),
];

pub struct Battery<'d> {
    adc: Adc<'d, ADC1<'d>, Blocking>,
    pin: AdcPin<GPIO10<'d>, ADC1<'d>, AdcCalCurve<ADC1<'d>>>,
    filtered_mv: Option<u32>,
}

impl<'d> Battery<'d> {
    pub fn new(adc: ADC1<'d>, pin: GPIO10<'d>) -> Self {
        let mut config = AdcConfig::new();
        // 11 dB covers ~0-3.1 V at the pin; a full cell gives ~2.1 V after the divider.
        let pin = config.enable_pin_with_cal::<_, AdcCalCurve<ADC1<'d>>>(pin, Attenuation::_11dB);
        Self {
            adc: Adc::new(adc, config),
            pin,
            filtered_mv: None,
        }
    }

    /// Take an averaged reading, smooth it, and return the charge estimate in percent.
    /// `None` if the ADC did not deliver a conversion.
    pub fn sample_percent(&mut self) -> Option<u8> {
        let mut sum = 0;
        for _ in 0..SAMPLES {
            sum += u32::from(self.read_pin_mv()?);
        }
        let battery_mv = sum / SAMPLES * DIVIDER_RATIO;
        // Exponential smoothing (1/4 weight) hides load-dependent sag from the radio.
        let filtered = match self.filtered_mv {
            Some(previous) => (previous * 3 + battery_mv) / 4,
            None => battery_mv,
        };
        self.filtered_mv = Some(filtered);
        Some(percent_from_mv(filtered))
    }

    fn read_pin_mv(&mut self) -> Option<u16> {
        for _ in 0..MAX_POLLS_PER_SAMPLE {
            match self.adc.read_oneshot(&mut self.pin) {
                Ok(millivolts) => return Some(millivolts),
                Err(nb::Error::WouldBlock) => {}
                Err(nb::Error::Other(())) => return None,
            }
        }
        log::warn!("Battery ADC conversion timed out");
        None
    }
}

fn percent_from_mv(millivolts: u32) -> u8 {
    let (top_mv, _) = DISCHARGE_CURVE[0];
    if millivolts >= top_mv {
        return 100;
    }
    for pair in DISCHARGE_CURVE.windows(2) {
        let (high_mv, high_pct) = pair[0];
        let (low_mv, low_pct) = pair[1];
        if millivolts >= low_mv {
            let span = high_mv - low_mv;
            return (low_pct + (millivolts - low_mv) * (high_pct - low_pct) / span) as u8;
        }
    }
    0
}
