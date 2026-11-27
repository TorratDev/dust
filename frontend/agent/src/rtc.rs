//! RTC backend with an optional fixed base time, so runs are reproducible.

use chrono::{Datelike, Duration, Local, NaiveDateTime, Timelike};
use core::any::Any;
use dust_core::rtc::{self, Date, Time};

pub struct Backend {
    /// `None` follows the host clock; `Some` freezes the base time.
    fixed_base: Option<NaiveDateTime>,
    offset: Duration,
}

impl Backend {
    pub fn new(fixed_base: Option<NaiveDateTime>) -> Self {
        Backend {
            fixed_base,
            offset: Duration::zero(),
        }
    }

    fn now(&self) -> NaiveDateTime {
        let base = self
            .fixed_base
            .unwrap_or_else(|| Local::now().naive_local());
        base + self.offset
    }
}

impl rtc::Backend for Backend {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn get_time(&mut self) -> Time {
        let now = self.now();
        Time {
            hour: now.hour() as u8,
            minute: now.minute() as u8,
            second: now.second() as u8,
        }
    }

    fn get_date_time(&mut self) -> (Date, Time) {
        let now = self.now();
        (
            Date {
                years_since_2000: (now.year() - 2000) as u8,
                month: now.month() as u8,
                day: now.day() as u8,
                days_from_sunday: now.weekday().num_days_from_sunday() as u8,
            },
            Time {
                hour: now.hour() as u8,
                minute: now.minute() as u8,
                second: now.second() as u8,
            },
        )
    }

    fn set_date_time(&mut self, (date, time): (Date, Time)) {
        let Some(date) = chrono::NaiveDate::from_ymd_opt(
            date.years_since_2000 as i32 + 2000,
            date.month as u32,
            date.day as u32,
        ) else {
            return;
        };
        let Some(time) = chrono::NaiveTime::from_hms_opt(
            time.hour as u32,
            time.minute as u32,
            time.second as u32,
        ) else {
            return;
        };
        let base = self
            .fixed_base
            .unwrap_or_else(|| Local::now().naive_local());
        self.offset = date.and_time(time) - base;
    }
}
