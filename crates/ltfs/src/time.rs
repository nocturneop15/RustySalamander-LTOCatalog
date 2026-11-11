//! LTFS times: `2026-11-11T10:20:30.123456789Z`, always UTC.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A moment, seconds and nanoseconds since 1970 (UTC); before 1970 negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct Time {
    pub secs: i64,
    pub nanos: u32,
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl Time {
    pub fn now() -> Time {
        Time::from(SystemTime::now())
    }

    /// LTFS text.
    pub fn format(&self) -> String {
        let days = self.secs.div_euclid(86400);
        let rem = self.secs.rem_euclid(86400);
        let (y, m, d) = civil_from_days(days);
        format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:09}Z", rem / 3600, rem / 60 % 60, rem % 60, self.nanos)
    }

    /// Parse LTFS text; also accepts fewer fraction digits, none, and a
    /// numeric offset instead of `Z`.
    pub fn parse(s: &str) -> Option<Time> {
        let s = s.trim();
        let b = s.as_bytes();
        if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b' ') || b[13] != b':' || b[16] != b':' {
            return None;
        }
        let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
        let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
            return None;
        }
        let mut rest = &s[19..];
        let mut nanos = 0u32;
        if let Some(r) = rest.strip_prefix('.') {
            let digits = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
            let frac = &r[..digits];
            let mut v: u64 = 0;
            for (i, c) in frac.bytes().enumerate() {
                if i < 9 {
                    v = v * 10 + (c - b'0') as u64;
                }
            }
            for _ in frac.len()..9 {
                v *= 10;
            }
            nanos = v as u32;
            rest = &r[digits..];
        }
        let offset = match rest {
            "" | "Z" | "z" => 0,
            r if r.len() >= 6 && (r.starts_with('+') || r.starts_with('-')) && &r[3..4] == ":" => {
                let sign = if r.starts_with('-') { -1 } else { 1 };
                sign * (r[1..3].parse::<i64>().ok()? * 3600 + r[4..6].parse::<i64>().ok()? * 60)
            }
            _ => return None,
        };
        let secs = days_from_civil(y, mo as u32, d as u32) * 86400 + h * 3600 + mi * 60 + se - offset;
        Some(Time { secs, nanos })
    }

    pub fn to_system(&self) -> SystemTime {
        if self.secs >= 0 {
            UNIX_EPOCH + Duration::new(self.secs as u64, self.nanos)
        } else {
            UNIX_EPOCH - Duration::from_secs(self.secs.unsigned_abs()) + Duration::from_nanos(self.nanos as u64)
        }
    }
}

impl From<SystemTime> for Time {
    fn from(t: SystemTime) -> Time {
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => Time { secs: d.as_secs() as i64, nanos: d.subsec_nanos() },
            Err(e) => {
                let d = e.duration();
                let (s, n) = (d.as_secs() as i64, d.subsec_nanos());
                if n == 0 {
                    Time { secs: -s, nanos: 0 }
                } else {
                    Time { secs: -s - 1, nanos: 1_000_000_000 - n }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for t in [Time { secs: 0, nanos: 0 }, Time { secs: 1_794_399_630, nanos: 123_456_789 }, Time { secs: -86_401, nanos: 5 }, Time { secs: 951_782_400, nanos: 0 }] {
            assert_eq!(Time::parse(&t.format()), Some(t), "{}", t.format());
            // Windows keeps times in 100 ns steps
            let t100 = Time { nanos: t.nanos / 100 * 100, ..t };
            assert_eq!(Time::from(t100.to_system()), t100);
        }
        assert_eq!(Time { secs: 951_782_400, nanos: 0 }.format(), "2000-02-29T00:00:00.000000000Z");
        assert_eq!(Time::parse("2000-02-29T01:00:00.5+01:00"), Some(Time { secs: 951_782_400, nanos: 500_000_000 }));
        assert_eq!(Time::parse("garbage"), None);
    }
}
