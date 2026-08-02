//! Deterministic, allocation-light presentation helpers.

use crate::i18n::Locale;

const IEC_UNITS: [&str; 9] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB", "YiB"];

/// Formats an exact byte count with IEC binary units.
///
/// Values below ten units retain one truncated decimal place; larger values
/// use an integer. Truncation avoids suggesting precision the scan did not
/// measure, and all arithmetic remains integer-only for the full `u128` range.
#[must_use]
pub fn format_iec_bytes(bytes: u128, locale: Locale) -> String {
    let mut unit_index = 0_usize;
    let mut divisor = 1_u128;
    while unit_index + 1 < IEC_UNITS.len() {
        let Some(next_divisor) = divisor.checked_mul(1_024) else {
            break;
        };
        if bytes < next_divisor {
            break;
        }
        divisor = next_divisor;
        unit_index += 1;
    }

    if unit_index == 0 {
        return format!("{bytes} {}", IEC_UNITS[unit_index]);
    }

    let whole = bytes / divisor;
    let tenth = ((bytes % divisor) * 10) / divisor;
    if whole < 10 && tenth != 0 {
        let decimal = match locale {
            Locale::EnglishUnitedStates => '.',
            Locale::SpanishMexico => ',',
        };
        format!("{whole}{decimal}{tenth} {}", IEC_UNITS[unit_index])
    } else {
        format!("{whole} {}", IEC_UNITS[unit_index])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_boundaries_without_rounding_up() {
        assert_eq!(format_iec_bytes(1_023, Locale::EnglishUnitedStates), "1023 B");
        assert_eq!(format_iec_bytes(1_024, Locale::EnglishUnitedStates), "1 KiB");
        assert_eq!(format_iec_bytes(1_535, Locale::EnglishUnitedStates), "1.4 KiB");
        assert_eq!(format_iec_bytes(10 * 1_024, Locale::EnglishUnitedStates), "10 KiB");
    }

    #[test]
    fn uses_the_selected_locale_decimal_separator() {
        assert_eq!(format_iec_bytes(1_536, Locale::EnglishUnitedStates), "1.5 KiB");
        assert_eq!(format_iec_bytes(1_536, Locale::SpanishMexico), "1,5 KiB");
    }

    #[test]
    fn supports_the_full_aggregate_range_without_overflow() {
        let formatted = format_iec_bytes(u128::MAX, Locale::EnglishUnitedStates);
        assert_eq!(formatted, "281474976710655 YiB");
    }
}
