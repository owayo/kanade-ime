//! Date-only arithmetic. OS clock/time-zone access is restricted to maintenance.
use std::time::{Duration, SystemTime};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Date {
    year: i32,
    month: u32,
    day: u32,
}
impl Date {
    pub fn new(year: i32, month: u32, day: u32) -> Option<Self> {
        ((1..=9999).contains(&year)
            && (1..=12).contains(&month)
            && (1..=last_day(year, month)).contains(&day))
        .then_some(Self { year, month, day })
    }
    fn month_end(self, delta: i32) -> Option<Self> {
        let index = self.year * 12 + self.month as i32 - 1 + delta;
        let year = index.div_euclid(12);
        let month = (index.rem_euclid(12) + 1) as u32;
        Self::new(year, month, last_day(year, month))
    }
    fn offset(mut self, days: i8) -> Option<Self> {
        for _ in 0..days.unsigned_abs() {
            if days > 0 {
                if self.day < last_day(self.year, self.month) {
                    self.day += 1;
                } else {
                    self.day = 1;
                    if self.month == 12 {
                        self.year += 1;
                        self.month = 1;
                    } else {
                        self.month += 1;
                    }
                }
            } else if self.day > 1 {
                self.day -= 1;
            } else {
                self = self.month_end(-1)?;
            }
        }
        Self::new(self.year, self.month, self.day)
    }
    pub(crate) fn candidates(self, reading: &str) -> Vec<String> {
        let date = match reading {
            "いっしゅうかんご" => self.offset(7),
            "いっしゅうかんまえ" => self.offset(-7),
            "こんげつまつ" => self.month_end(0),
            "らいげつまつ" => self.month_end(1),
            "せんげつまつ" => self.month_end(-1),
            _ => None,
        };
        date.map(|d| {
            vec![
                format!("{:04}-{:02}-{:02}", d.year, d.month, d.day),
                format!("{}年{}月{}日", d.year, d.month, d.day),
            ]
        })
        .unwrap_or_default()
    }
}
fn last_day(year: i32, month: u32) -> u32 {
    match month {
        2 if year % 400 == 0 || (year % 4 == 0 && year % 100 != 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => 0,
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Calendar {
    date: Date,
    bounds: Option<(SystemTime, SystemTime)>,
}
impl Calendar {
    pub(crate) fn fixed(date: Date) -> Self {
        Self { date, bounds: None }
    }
    pub(crate) fn current(&self) -> Option<Date> {
        self.at(SystemTime::now())
    }
    fn at(&self, now: SystemTime) -> Option<Date> {
        self.bounds
            .is_none_or(|(observed, expires)| now >= observed && now < expires)
            .then_some(self.date)
    }
    /// Call only during startup or background maintenance, never for a key.
    pub(crate) fn local() -> Option<Self> {
        unsafe {
            #[cfg(unix)]
            {
                unsafe extern "C" {
                    fn tzset();
                }
                tzset();
            }
            #[cfg(windows)]
            libc::tzset();
            let stamp = libc::time(std::ptr::null_mut());
            if stamp == -1 {
                return None;
            }
            let mut result = std::mem::MaybeUninit::<libc::tm>::uninit();
            #[cfg(unix)]
            if libc::localtime_r(&stamp, result.as_mut_ptr()).is_null() {
                return None;
            }
            #[cfg(windows)]
            if libc::localtime_s(result.as_mut_ptr(), &stamp) != 0 {
                return None;
            }
            let mut result = result.assume_init();
            let date = Date::new(
                result.tm_year.checked_add(1900)?,
                (result.tm_mon + 1).try_into().ok()?,
                result.tm_mday.try_into().ok()?,
            )?;
            result.tm_mday += 1;
            result.tm_hour = 0;
            result.tm_min = 0;
            result.tm_sec = 0;
            result.tm_isdst = -1;
            #[cfg(unix)]
            let midnight = libc::mktime(&mut result);
            #[cfg(windows)]
            let midnight = {
                unsafe extern "C" {
                    #[link_name = "_mktime64"]
                    fn mktime64(value: *mut libc::tm) -> i64;
                }
                mktime64(&mut result)
            };
            if midnight <= stamp {
                return None;
            }
            let observed =
                SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(stamp.try_into().ok()?))?;
            let midnight = SystemTime::UNIX_EPOCH
                .checked_add(Duration::from_secs(midnight.try_into().ok()?))?;
            let expires = midnight.min(observed.checked_add(Duration::from_secs(90))?);
            Some(Self {
                date,
                bounds: Some((observed, expires)),
            })
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expired_and_backwards_clock_observations_are_unavailable() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let date = Date::new(2024, 1, 31).unwrap();
        let calendar = Calendar {
            date,
            bounds: Some((start, start + Duration::from_secs(1))),
        };
        assert_eq!(calendar.at(start), Some(date));
        assert_eq!(calendar.at(start - Duration::from_secs(1)), None);
        assert_eq!(calendar.at(start + Duration::from_secs(1)), None);
    }
    #[test]
    fn boundaries_leap_centuries_and_relative_days() {
        for (date, reading, expected) in [
            ((2024, 2, 22), "いっしゅうかんご", "2024-02-29"),
            ((2024, 2, 23), "いっしゅうかんご", "2024-03-01"),
            ((1900, 2, 22), "いっしゅうかんご", "1900-03-01"),
            ((2000, 2, 22), "いっしゅうかんご", "2000-02-29"),
            ((2024, 1, 31), "らいげつまつ", "2024-02-29"),
            ((2025, 1, 31), "らいげつまつ", "2025-02-28"),
            ((2026, 1, 1), "せんげつまつ", "2025-12-31"),
            ((2026, 12, 1), "らいげつまつ", "2027-01-31"),
            ((2026, 1, 7), "いっしゅうかんまえ", "2025-12-31"),
            ((2026, 1, 6), "いっしゅうかんまえ", "2025-12-30"),
            ((2026, 12, 28), "いっしゅうかんご", "2027-01-04"),
            ((2026, 1, 4), "いっしゅうかんまえ", "2025-12-28"),
        ] {
            assert_eq!(
                Date::new(date.0, date.1, date.2)
                    .unwrap()
                    .candidates(reading)[0],
                expected
            );
        }
        for date in [
            (0, 1, 1),
            (10000, 1, 1),
            (2026, 0, 1),
            (2026, 13, 1),
            (2026, 2, 29),
            (2026, 4, 31),
            (2026, 1, 0),
        ] {
            assert!(Date::new(date.0, date.1, date.2).is_none());
        }
        assert!(
            Date::new(1, 1, 1)
                .unwrap()
                .candidates("いっしゅうかんまえ")
                .is_empty()
        );
        assert!(
            Date::new(9999, 12, 31)
                .unwrap()
                .candidates("いっしゅうかんご")
                .is_empty()
        );
        assert!(
            Date::new(2026, 1, 1)
                .unwrap()
                .candidates("unknown")
                .is_empty()
        );
    }
}
