//! Noubin Lazy Timestamp (§5) parse/format.

use plippa_noubin::{NoubinTimestamp, TimestampError};

fn ms(s: &str) -> u64 {
    NoubinTimestamp::parse(s)
        .unwrap_or_else(|e| panic!("{s:?}: {e}"))
        .as_millis()
}

#[test]
fn spec_examples() {
    assert_eq!(ms("45"), 45_000);
    assert_eq!(ms("45.25"), 45_250);
    assert_eq!(ms("12:55"), 12 * 60_000 + 55_000);
    assert_eq!(ms("1:02:03"), 3_723_000);
    assert_eq!(ms("1:02:03.125"), 3_723_125);
    assert_eq!(ms("12:55.5"), 12 * 60_000 + 55_500);
    assert_eq!(ms("12.5"), 12_500);
    assert_eq!(ms("1:23.75"), 83_750);
    assert_eq!(ms("2:01:03.12"), 7_263_120);
}

#[test]
fn components_may_exceed_clock_maximums() {
    assert_eq!(ms("120"), 120_000); // 2:00
    assert_eq!(ms("90:75"), (90 * 60 + 75) * 1000);
    assert_eq!(ms("100:00:00"), 360_000_000);
    assert_eq!(ms("0001:0002:0003"), 3_723_000);
}

#[test]
fn trailing_zero_fractions_are_equal() {
    assert_eq!(ms("1:23.1"), ms("1:23.10"));
    assert_eq!(ms("1:23.1"), ms("1:23.100"));
    assert_eq!(ms("1:23.1"), 83_100);
}

#[test]
fn extra_fraction_digits_round_half_up() {
    assert_eq!(ms("1.0004"), 1_000);
    assert_eq!(ms("1.0005"), 1_001);
    assert_eq!(ms("1.9996"), 2_000);
}

#[test]
fn whitespace_is_trimmed() {
    assert_eq!(ms("  1:00 \n"), 60_000);
}

#[test]
fn rejects_bad_input() {
    assert_eq!(NoubinTimestamp::parse(""), Err(TimestampError::Empty));
    assert_eq!(NoubinTimestamp::parse("   "), Err(TimestampError::Empty));
    assert_eq!(NoubinTimestamp::parse("-5"), Err(TimestampError::Negative));
    assert_eq!(
        NoubinTimestamp::parse(" -0:05"),
        Err(TimestampError::Negative)
    );
    assert_eq!(
        NoubinTimestamp::parse("1:2:3:4"),
        Err(TimestampError::TooManyComponents)
    );
    for bad in [
        "abc",
        "1:",
        ":30",
        "1::2",
        "1.",
        ".5",
        "1.2.3",
        "1:02.5:03",
        "1e3",
        "+5",
        "１２",
    ] {
        assert!(
            NoubinTimestamp::parse(bad).is_err(),
            "{bad:?} should be rejected"
        );
    }
    assert_eq!(
        NoubinTimestamp::parse("99999999999999999999"),
        Err(TimestampError::Overflow)
    );
}

#[test]
fn formats_full_form_without_trailing_fraction_zeros() {
    let f = |ms| NoubinTimestamp::from_millis(ms).to_string();
    assert_eq!(f(0), "00:00:00");
    assert_eq!(f(45_000), "00:00:45");
    assert_eq!(f(45_250), "00:00:45.25");
    assert_eq!(f(83_100), "00:01:23.1");
    assert_eq!(f(3_723_125), "01:02:03.125");
    assert_eq!(f(3_723_005), "01:02:03.005");
    assert_eq!(f(360_000_000), "100:00:00");
}

#[test]
fn format_parse_round_trip() {
    for v in [
        0u64,
        1,
        999,
        1_000,
        59_999,
        3_599_999,
        3_600_000,
        987_654_321,
    ] {
        let t = NoubinTimestamp::from_millis(v);
        assert_eq!(NoubinTimestamp::parse(&t.to_string()).unwrap(), t);
    }
}

#[test]
fn serialises_as_string() {
    assert_eq!(
        serde_json::to_string(&NoubinTimestamp::from_millis(3_723_125)).unwrap(),
        "\"01:02:03.125\""
    );
}
