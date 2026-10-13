//! The local time zone `Date` works in: LocalTime(t) and UTC(t) of
//! ECMA-262 21.4.1.25–26, over the tz database Temporal already ships.
//!
//! With no zone set (the default) local time IS UTC, which is how the engine
//! always behaved: `getTimezoneOffset()` is 0 and the local and UTC methods
//! agree. A host that wants a browser's behaviour names its zone
//! (`ScriptState::set_time_zone`).
#![allow(unused_imports)]
use super::*;
use crate::vm::temporal::tzdb;

impl<'p> Vm<'p> {
    /// Set the local time zone to the IANA zone `id` (`None`: UTC). An
    /// unknown identifier is an error, not a silent UTC.
    pub(crate) fn set_local_time_zone(&mut self, id: Option<&str>) -> Result<(), String> {
        self.local_tz = match id {
            None => None,
            Some(id) => Some(tzdb::lookup(id).ok_or_else(|| format!("unknown time zone: {id}"))?.zone),
        };
        Ok(())
    }

    /// The identifier of the local time zone ("UTC" when none is set), as
    /// Intl's DefaultTimeZone answers it.
    pub(crate) fn local_time_zone_id(&self) -> String {
        match self.local_tz {
            Some(zone) => tzdb::primary(zone).to_string(),
            None => "UTC".to_string(),
        }
    }

    /// The local offset, in ms, at the UTC time value `t` (LocalTime(t) =
    /// t + this).
    pub(crate) fn local_offset_ms(&self, t: f64) -> f64 {
        match self.local_tz {
            Some(zone) if t.is_finite() => {
                tzdb::offset_seconds(zone, (t / 1000.0).floor() as i64) as f64 * 1000.0
            }
            _ => 0.0,
        }
    }

    /// UTC(t): the time value whose local wall clock is `t`. In a fall-back
    /// repeat the earlier instant; in a spring-forward gap the offset in force
    /// before the transition, as the specification has it.
    pub(crate) fn local_to_utc_ms(&self, t: f64) -> f64 {
        let Some(zone) = self.local_tz else {
            return t;
        };
        if !t.is_finite() {
            return t;
        }
        let secs = (t / 1000.0).floor() as i64;
        let offset = match tzdb::possible_instants(zone, secs).first() {
            Some(&instant) => secs - instant,
            None => tzdb::offset_seconds(zone, secs - 86_400) as i64,
        };
        t - offset as f64 * 1000.0
    }
}

/// The `GMT±hhmm` a local time string carries for `offset_ms`.
pub(crate) fn gmt_offset(offset_ms: f64) -> String {
    let minutes = (offset_ms / 60_000.0).round() as i64;
    let sign = if minutes < 0 { '-' } else { '+' };
    let m = minutes.abs();
    format!("GMT{sign}{:02}{:02}", m / 60, m % 60)
}
