//! The format-independent trace event model.
//!
//! Everything upstream of an output format produces [`TraceEvent`]s; every output format
//! does nothing but encode them. The model is the contract between the two, and it is
//! deliberately narrow: a slice with a name and a category, a counter sample, a named
//! moment, and the metadata that gives a process and a thread a readable name. Nothing in
//! it is specific to a trace format — no `ph` letters, no microseconds, no JSON.
//!
//! **Why a model rather than one conversion per format.** The policy that decides what a
//! Collection *is* in a trace — the slice names, the categories, the argument keys, which
//! sub-phases a build has — is one decision, not one per format. gcmon reached this the
//! expensive way: its Chrome and Perfetto paths each converted from the raw Record
//! independently, reimplemented the same sub-phase discovery and naming, and drifted until
//! they produced two disagreeing traces of the same run (gcmon ADR-0007).
//!
//! Timestamps are **nanoseconds** here, as CPython publishes them. A format that wants
//! another unit converts on the way out; see [`super::exporters::timing::ts_us`] for the
//! Chrome encoder's microseconds.

use std::fmt;

/// The value side of an event argument.
///
/// Two arms because that is what a Record carries: counters and timestamps are `i64`, and
/// `duration` is an `f64` of seconds. A format renders each in its own way — JSON writes
/// them as bare numbers, a binary format as typed fields — so no textual formatting is
/// baked in here beyond [`fmt::Display`], which exists for the formats that want it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ArgValue {
    Int(i64),
    Float(f64),
}

impl fmt::Display for ArgValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArgValue::Int(v) => write!(f, "{}", v),
            ArgValue::Float(v) => write!(f, "{}", v),
        }
    }
}

impl From<i64> for ArgValue {
    fn from(v: i64) -> Self {
        ArgValue::Int(v)
    }
}

impl From<f64> for ArgValue {
    fn from(v: f64) -> Self {
        ArgValue::Float(v)
    }
}

/// One event argument. The key is `&'static str` because argument keys are policy — they
/// come from the conversion's own tables, never from the target.
pub type Arg = (&'static str, ArgValue);

/// One event in a trace, independent of how any format writes it.
///
/// ## Ordering
///
/// A producer hands an encoder events in the order they must be written, and an encoder
/// writes them in the order it receives them. Three guarantees ride on that, and a
/// producer that breaks any of them produces a trace no format can repair:
///
/// 1. **Metadata precedes reference.** The [`ProcessMeta`](Self::ProcessMeta) for a pid and
///    the [`ThreadMeta`](Self::ThreadMeta) for a tid come before the first event that names
///    them, so a format may write metadata inline rather than buffering the whole trace.
/// 2. **A `Begin` precedes its `End`.** Spans are properly nested in emission order: a
///    sub-span's `Begin` and `End` both fall between its enclosing span's pair.
/// 3. **A [`Counter`](Self::Counter) follows the span it describes.**
///
/// Emission order is the contract; **timestamp order is not**. A build that publishes a
/// phase's stop but not its start leaves the phase chained onto a field reading zero, so a
/// sub-span's `ts_ns` can legitimately precede its parent's. Encoders must not reorder on
/// timestamps to "fix" that — the raw numbers are what the target published.
#[derive(Debug, Clone, PartialEq)]
pub enum TraceEvent {
    /// Names a process. Emitted once per pid per stream — deduplication belongs to the
    /// encoder, since each output stream needs its own copy.
    ProcessMeta { pid: u32, name: String },
    /// Names a thread track. gcscope draws one track per interpreter, so `tid` is an
    /// interpreter id rather than an OS thread id.
    ThreadMeta { pid: u32, tid: i64, name: String },
    /// Opens a span on `(pid, tid)`.
    Begin {
        pid: u32,
        tid: i64,
        ts_ns: i64,
        name: String,
        cat: String,
        args: Vec<Arg>,
    },
    /// Closes the innermost open span on `(pid, tid)`. Carries the name and category too,
    /// because the formats that check them are the ones that catch a mismatched pair.
    End {
        pid: u32,
        tid: i64,
        ts_ns: i64,
        name: String,
        cat: String,
    },
    /// A named moment with no duration, scoped to the process rather than a track — what
    /// the Observed injects to correlate GC activity with its own behaviour. No producer
    /// emits one yet; the control plane that will is not built.
    Instant { pid: u32, ts_ns: i64, name: String },
    /// One sample of a named numeric series on `(pid, tid)`. The series is `name` and its
    /// components are `args`; a single-component series conventionally has an empty `name`
    /// and takes its label from the argument key.
    Counter {
        pid: u32,
        tid: i64,
        ts_ns: i64,
        name: String,
        args: Vec<Arg>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Chrome encoder writes argument values with `Display`, so what it prints for the
    /// awkward values is fixed here rather than inside a JSON assertion. `Float` in
    /// particular must print `0` for `0.0` (not `0.0`) — that is what `format!("{}", …)`
    /// did when the encoder built the JSON by hand, and the trace bytes depend on it.
    #[test]
    fn arg_values_display_as_bare_numbers() {
        assert_eq!(ArgValue::Int(0).to_string(), "0");
        assert_eq!(ArgValue::Int(-1).to_string(), "-1");
        assert_eq!(ArgValue::Int(i64::MIN).to_string(), "-9223372036854775808");
        assert_eq!(ArgValue::Float(0.0).to_string(), "0");
        assert_eq!(ArgValue::Float(0.00125).to_string(), "0.00125");
        assert_eq!(ArgValue::Float(f64::INFINITY).to_string(), "inf");
        assert_eq!(ArgValue::Float(f64::NAN).to_string(), "NaN");
    }

    /// The conversion builds arguments with `.into()`, so an `i64` must never silently
    /// become a `Float` (or the other way): the two print differently, and `duration` is
    /// the only float a Record carries.
    #[test]
    fn conversion_from_a_number_keeps_its_kind() {
        assert_eq!(ArgValue::from(7i64), ArgValue::Int(7));
        assert_eq!(ArgValue::from(7.0f64), ArgValue::Float(7.0));
    }
}
