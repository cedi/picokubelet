//! Wall clock without an RTC: anchor a unix timestamp from the HTTP Date
//! header on the first /version response, then advance via
//! embassy_time::Instant. Good to ~1s of accuracy, well within the 40s
//! lease tolerance.

use core::fmt::Write as FmtWrite;
use core::sync::atomic::Ordering;

use embassy_time::Instant;
use heapless::String as HString;
use portable_atomic::AtomicU64;

static WALL_CLOCK_ANCHOR_UNIX: AtomicU64 = AtomicU64::new(0);
static WALL_CLOCK_ANCHOR_BOOT_MS: AtomicU64 = AtomicU64::new(0);

pub fn set_wall_clock(unix_secs: u64) {
    let now_ms = Instant::now().as_millis();
    WALL_CLOCK_ANCHOR_UNIX.store(unix_secs, Ordering::Release);
    WALL_CLOCK_ANCHOR_BOOT_MS.store(now_ms, Ordering::Release);
}

pub fn unix_now_secs() -> u64 {
    let anchor = WALL_CLOCK_ANCHOR_UNIX.load(Ordering::Acquire);
    let anchor_at = WALL_CLOCK_ANCHOR_BOOT_MS.load(Ordering::Acquire);
    if anchor == 0 {
        return 0;
    }
    let now_ms = Instant::now().as_millis();
    anchor + (now_ms.saturating_sub(anchor_at)) / 1000
}

// Convert (year, month, day, h, m, s) → unix epoch seconds. Howard Hinnant's
// civil_from_days algorithm in reverse, simplified for AD years.
pub fn unix_from_ymdhms(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> u64 {
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // 0..399
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) as u64 + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as i64 * 146097 + doe as i64 - 719468;
    (days as u64) * 86400 + (h as u64) * 3600 + (mi as u64) * 60 + s as u64
}

/// Parse RFC1123 HTTP Date like "Sat, 02 May 2026 15:47:17 GMT" → unix secs.
/// Tolerant of the surrounding spaces but not other formats.
pub fn parse_http_date(s: &str) -> Option<u64> {
    // skip past day-of-week + comma + space (5–6 chars)
    let comma = s.find(',')?;
    let rest = s[comma + 1..].trim_start();
    let mut it = rest.split_ascii_whitespace();
    let day: u32 = it.next()?.parse().ok()?;
    let month = match it.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i32 = it.next()?.parse().ok()?;
    let mut hms = it.next()?.split(':');
    let h: u32 = hms.next()?.parse().ok()?;
    let m: u32 = hms.next()?.parse().ok()?;
    let s2: u32 = hms.next()?.parse().ok()?;
    Some(unix_from_ymdhms(year, month, day, h, m, s2))
}

/// Format unix seconds as RFC3339 with microsecond precision (which k8s
/// expects in `renewTime`): "2026-05-02T15:47:17.000000Z".
pub fn fmt_rfc3339(unix: u64, out: &mut HString<40>) -> Result<(), core::fmt::Error> {
    // civil_from_days, forward
    let days = (unix / 86400) as i64;
    let secs_today = (unix % 86400) as u32;
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + (era as i32) * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    let h = secs_today / 3600;
    let mi = (secs_today % 3600) / 60;
    let s = secs_today % 60;
    write!(
        out,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000000Z",
        y, m, d, h, mi, s
    )
}
