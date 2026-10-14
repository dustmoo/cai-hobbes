use chrono::NaiveDate;

pub(super) fn d(s: &str) -> NaiveDate {
    s.parse().unwrap()
}
