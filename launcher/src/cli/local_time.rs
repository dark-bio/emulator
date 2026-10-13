// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The local time the reading view prints, taken from each instant's own
//! offset from UTC, so a date across a daylight saving change stays right.

use time::{OffsetDateTime, UtcOffset};

use super::style::{Role, Theme};

/// An instant at `offset`, as a muted date for a table, or in `full` for a
/// block, where a muted clock and offset follow the plain date.
pub(super) fn render(
    theme: &Theme,
    instant: OffsetDateTime,
    offset: UtcOffset,
    full: bool,
) -> String {
    // An instant at the calendar's limits cannot move to the offset
    let Some(local) = instant.checked_to_offset(offset) else {
        return theme.paint(Role::Muted, "-");
    };
    let date = local.date().to_string();
    if !full {
        return theme.paint(Role::Muted, date);
    }

    // The date stays plain in a block, followed by a muted clock and offset
    let seconds = offset.whole_seconds();
    let minutes = seconds.unsigned_abs() / 60;
    let clock = format!(
        "{:02}:{:02} {}{:02}:{:02}",
        local.hour(),
        local.minute(),
        if seconds < 0 { '-' } else { '+' },
        minutes / 60,
        minutes % 60,
    );
    format!("{date} {}", theme.paint(Role::Muted, clock))
}

/// The local offset at `instant`, with the daylight saving rule in force then.
#[cfg(unix)]
pub(super) fn offset(instant: OffsetDateTime) -> Option<UtcOffset> {
    let timestamp = libc::time_t::try_from(instant.unix_timestamp()).ok()?;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: Both pointers refer to live, aligned storage for this call
    if unsafe { libc::localtime_r(&timestamp, local.as_mut_ptr()) }.is_null() {
        return None;
    }
    // SAFETY: A successful localtime_r initializes the output structure
    let local = unsafe { local.assume_init() };
    UtcOffset::from_whole_seconds(local.tm_gmtoff.try_into().ok()?).ok()
}

/// The local offset at `instant`, from Windows' dynamic time zone rules.
#[cfg(windows)]
pub(super) fn offset(instant: OffsetDateTime) -> Option<UtcOffset> {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::Time::{
        DYNAMIC_TIME_ZONE_INFORMATION, GetDynamicTimeZoneInformation,
        SystemTimeToTzSpecificLocalTimeEx,
    };

    // Convert the UTC instant using the system's time zone and historical rules
    let utc = instant.to_offset(UtcOffset::UTC);
    let universal = SYSTEMTIME {
        wYear: utc.year().try_into().ok()?,
        wMonth: utc.month() as u16,
        wDayOfWeek: 0,
        wDay: u16::from(utc.day()),
        wHour: u16::from(utc.hour()),
        wMinute: u16::from(utc.minute()),
        wSecond: u16::from(utc.second()),
        wMilliseconds: 0,
    };
    let mut zone = std::mem::MaybeUninit::<DYNAMIC_TIME_ZONE_INFORMATION>::uninit();
    // SAFETY: The pointer is valid writable storage for the output structure
    if unsafe { GetDynamicTimeZoneInformation(zone.as_mut_ptr()) } == u32::MAX {
        return None;
    }
    let mut local = std::mem::MaybeUninit::<SYSTEMTIME>::uninit();
    // SAFETY: The zone is initialized, and all pointers remain valid for the call
    if unsafe { SystemTimeToTzSpecificLocalTimeEx(zone.as_ptr(), &universal, local.as_mut_ptr()) }
        == 0
    {
        return None;
    }
    // SAFETY: A successful conversion initializes the output structure
    let local = unsafe { local.assume_init() };

    // Interpret the local fields as UTC to measure their distance from the input
    let date = time::Date::from_calendar_date(
        i32::from(local.wYear),
        u8::try_from(local.wMonth).ok()?.try_into().ok()?,
        local.wDay.try_into().ok()?,
    )
    .ok()?;
    let local = date
        .with_hms(
            local.wHour.try_into().ok()?,
            local.wMinute.try_into().ok()?,
            local.wSecond.try_into().ok()?,
        )
        .ok()?
        .assume_utc();
    let seconds = local.unix_timestamp() - utc.unix_timestamp();
    UtcOffset::from_whole_seconds(seconds.try_into().ok()?).ok()
}

/// Tests of local times, at fixed offsets so the host's zone plays no part.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::style::Color;
    use time::format_description::well_known::Rfc3339;

    /// Dates cross midnight locally, and blocks mute only the clock and offset.
    #[test]
    fn test_local_dates_and_times_at_fixed_offsets() {
        let theme = Theme::fixed(80, Color::True, true);
        for (stamp, seconds, date, clock) in [
            ("2026-11-12T13:03:00Z", 3600, "2026-11-12", "14:03 +01:00"),
            ("2026-11-12T23:30:00Z", 3600, "2026-11-13", "00:30 +01:00"),
            ("2026-01-01T02:03:00Z", -19800, "2025-12-31", "20:33 -05:30"),
            ("2026-11-12T13:03:00Z", 0, "2026-11-12", "13:03 +00:00"),
        ] {
            let instant = OffsetDateTime::parse(stamp, &Rfc3339).unwrap();
            let offset = UtcOffset::from_whole_seconds(seconds).unwrap();
            assert_eq!(
                render(&theme, instant, offset, false),
                format!("\x1b[38;2;124;128;152m{date}\x1b[0m"),
                "table: {stamp}, {seconds}"
            );
            assert_eq!(
                render(&theme, instant, offset, true),
                format!("{date} \x1b[38;2;124;128;152m{clock}\x1b[0m"),
                "block: {stamp}, {seconds}"
            );
            assert_eq!(
                render(&Theme::fixed(80, Color::Off, true), instant, offset, true),
                format!("{date} {clock}"),
                "pipe: {stamp}, {seconds}"
            );
        }
    }
}
