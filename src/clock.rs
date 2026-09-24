//! Wall clock: set from SNTP, then extrapolated from the monotonic esp-hal timer,
//! so it keeps running from the last sync when Wi-Fi drops.

use esp_hal::time::Instant;

use crate::storage::{ClockConfig, DstRule};

const SECONDS_PER_DAY: i64 = 86_400;

#[derive(Default)]
pub struct WallClock {
    // Unix seconds at the moment of the last sync, and when that moment happened.
    synced: Option<(u64, Instant)>,
}

impl WallClock {
    pub fn set(&mut self, unix_seconds: u64, at: Instant) {
        self.synced = Some((unix_seconds, at));
    }

    pub fn is_synced(&self) -> bool {
        self.synced.is_some()
    }

    pub fn unix_now(&self) -> Option<u64> {
        let (unix_seconds, at) = self.synced?;
        Some(unix_seconds + (Instant::now() - at).as_secs())
    }
}

/// Local time as (hours, minutes) for the configured offset and DST rule.
pub fn local_hh_mm(unix_seconds: u64, config: &ClockConfig) -> (u8, u8) {
    let seconds_of_day = local_seconds(unix_seconds, config).rem_euclid(SECONDS_PER_DAY);
    (
        (seconds_of_day / 3600) as u8,
        ((seconds_of_day % 3600) / 60) as u8,
    )
}

/// Seconds since 1970-01-01 00:00 *local* time (offset and DST applied).
pub fn local_seconds(unix_seconds: u64, config: &ClockConfig) -> i64 {
    let utc = unix_seconds as i64;
    let mut local = utc + i64::from(config.utc_offset_minutes) * 60;
    if config.dst == DstRule::Eu && eu_summer_time(utc) {
        local += 3600;
    }
    local
}

/// Calendar fields of a [`local_seconds`] value.
#[derive(Clone, Copy)]
pub struct DateTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

pub fn date_time(seconds: i64) -> DateTime {
    let (year, month, day) = civil_from_days(seconds.div_euclid(SECONDS_PER_DAY));
    let seconds_of_day = seconds.rem_euclid(SECONDS_PER_DAY) as u32;
    DateTime {
        year,
        month,
        day,
        hour: seconds_of_day / 3600,
        minute: seconds_of_day % 3600 / 60,
        second: seconds_of_day % 60,
    }
}

/// EU summer time: from 01:00 UTC on the last Sunday of March
/// until 01:00 UTC on the last Sunday of October.
fn eu_summer_time(utc: i64) -> bool {
    let (year, _, _) = civil_from_days(utc.div_euclid(SECONDS_PER_DAY));
    let start = last_sunday(year, 3) * SECONDS_PER_DAY + 3600;
    let end = last_sunday(year, 10) * SECONDS_PER_DAY + 3600;
    (start..end).contains(&utc)
}

/// Day number (days since 1970-01-01) of the last Sunday of `month`.
fn last_sunday(year: i64, month: u32) -> i64 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let last_day = days_from_civil(next_year, next_month, 1) - 1;
    // 1970-01-01 was a Thursday; with Sunday = 0 that day is weekday 4.
    last_day - (last_day + 4).rem_euclid(7)
}

// Howard Hinnant's civil-date algorithms (proleptic Gregorian calendar).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month = i64::from(month);
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}
