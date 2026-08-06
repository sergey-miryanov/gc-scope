//! The format-independent trace event model: the contract between the monitor and its
//! output formats. Nothing here belongs to a trace format, so there are no `ph` letters, no
//! microseconds, no JSON.
//!
//! One model rather than one conversion per format, because what a Collection looks like in
//! a trace (slice names, categories, argument keys, which sub-phases a build has) is one
//! decision. gcmon's Chrome and Perfetto paths each converted from the raw Record, and
//! drifted until they wrote two disagreeing traces of one run (gcmon ADR-0007).
//!
//! Timestamps are nanoseconds, as CPython publishes them; a format converts on the way out
//! (see [`super::exporters::timing::ts_us`]).

use std::fmt;

/// The value side of an event argument. Two arms because that is what a Record carries:
/// counters and timestamps are `i64`, `duration` is an `f64` of seconds. [`fmt::Display`]
/// serves the formats that write bare numbers; anything else renders its own way.
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

/// One event argument. The key is `&'static str` because keys come from the conversion's
/// own tables, never from the target.
pub type Arg = (&'static str, ArgValue);

/// One event in a trace, independent of how a format writes it.
///
/// ## Ordering
///
/// An encoder writes events in the order it receives them, so a producer must emit them in
/// the order a trace needs:
///
/// 1. [`ProcessMeta`](Self::ProcessMeta) and [`ThreadMeta`](Self::ThreadMeta) before the
///    first event naming that pid or tid, so a format can write metadata inline instead of
///    buffering the whole trace.
/// 2. A `Begin` before its `End`, with sub-spans nested inside their enclosing pair.
/// 3. A [`Counter`](Self::Counter) after the span it describes.
///
/// Emission order is the contract; timestamp order is not. A build that publishes a phase's
/// stop but not its start chains that phase onto a field reading zero, so a sub-span's
/// `ts_ns` can precede its parent's. Do not reorder on timestamps to repair it: the raw
/// numbers are what the target published.
#[derive(Debug, Clone, PartialEq)]
pub enum TraceEvent {
    /// Names a process. The encoder deduplicates, since each output stream needs its own
    /// copy.
    ProcessMeta { pid: u32, name: String },
    /// Names a track. gcscope draws one per interpreter, so `tid` is an interpreter id
    /// rather than an OS thread id.
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
    /// Closes the innermost open span on `(pid, tid)`. It repeats the name and category so
    /// a format can catch a mismatched pair.
    End {
        pid: u32,
        tid: i64,
        ts_ns: i64,
        name: String,
        cat: String,
    },
    /// A named moment with no duration, scoped to the process: what the Observed injects to
    /// correlate GC activity with its own behaviour. No producer emits one yet, since the
    /// control plane is not built.
    Instant { pid: u32, ts_ns: i64, name: String },
    /// One sample of a numeric series on `(pid, tid)`: `name` is the series, `args` its
    /// components. A single-component series has an empty `name` and takes its label from
    /// the argument key.
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

    /// The Chrome encoder writes argument values through `Display`, so the awkward ones are
    /// pinned here rather than inside a JSON assertion. `Float` must print `0` for `0.0`:
    /// that is what the hand-built JSON did, and the trace bytes depend on it.
    ///
    /// The last two cases record a defect, not a requirement. `inf` and `NaN` are not JSON,
    /// so one non-finite `duration` (a torn read can supply one) makes a viewer reject the
    /// whole capture. See `.scratch/chrome-encoder-defects/issues/03`.
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

    /// The conversion builds arguments with `.into()`, so an `i64` must not arrive as a
    /// `Float` or the reverse. The two print differently.
    #[test]
    fn conversion_from_a_number_keeps_its_kind() {
        assert_eq!(ArgValue::from(7i64), ArgValue::Int(7));
        assert_eq!(ArgValue::from(7.0f64), ArgValue::Float(7.0));
    }
}
