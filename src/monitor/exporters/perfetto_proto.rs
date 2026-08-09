//! Perfetto's field numbers and enum values, and nothing else.
//!
//! Transcribed by hand from Perfetto's `.proto` files, which is only safe because the test
//! travels with the table: `tests/perfetto_encoder.rs` hands [`FIELDS`] and [`ENUM_VALUES`] to
//! `tests/harness/proto_constants.py`, which reads each number back out of the official
//! generated descriptors. A wrong number here writes a *different* message rather than an
//! invalid one, so nothing in gcscope's own output can catch it — see
//! [ADR 0021](../../../docs/adr/0021-hand-rolled-perfetto-encoder.md).
//!
//! **Both tables are built by the macros below from the same declarations that define the
//! constants.** That is deliberate: a constant added without an assertion line is the failure
//! this file guards against, so there is no way to declare one that stays out of the list the
//! checker is given. Adding a field means adding one line, and the line carries its own proof.
//!
//! Numbers are not in ascending order within a message. They are grouped as Perfetto's
//! `.proto` groups them, and the gaps are real — a later field number does not mean a later
//! addition.

macro_rules! proto_messages {
    ($(
        $(#[$message_doc:meta])*
        $module:ident = $message:literal {
            $(
                $(#[$field_doc:meta])*
                $konst:ident = $number:literal as $field:literal,
            )*
        }
    )*) => {
        $(
            $(#[$message_doc])*
            pub mod $module {
                $(
                    $(#[$field_doc])*
                    pub const $konst: u32 = $number;
                )*
            }
        )*

        /// Every field number above, as `(message, field, number)`, for the descriptor check.
        pub const FIELDS: &[(&str, &str, u32)] = &[
            $($( ($message, $field, $module::$konst), )*)*
        ];
    };
}

macro_rules! proto_enums {
    ($(
        $(#[$enum_doc:meta])*
        $module:ident = $message:literal . $enum_name:literal {
            $(
                $(#[$value_doc:meta])*
                $konst:ident = $number:literal as $value:literal,
            )*
        }
    )*) => {
        $(
            $(#[$enum_doc])*
            pub mod $module {
                $(
                    $(#[$value_doc])*
                    pub const $konst: u32 = $number;
                )*
            }
        )*

        /// Every enum value above, as `(message, enum, value, number)`, for the same check.
        pub const ENUM_VALUES: &[(&str, &str, &str, u32)] = &[
            $($( ($message, $enum_name, $value, $module::$konst), )*)*
        ];
    };
}

proto_messages! {
    /// The whole file: a `Trace` is a repeated `TracePacket` and nothing else, which is what
    /// lets an encoder append packets to an open file instead of building the trace in memory.
    trace = "Trace" {
        PACKET = 1 as "packet",
    }

    /// The envelope every event travels in.
    trace_packet = "TracePacket" {
        /// Where a timestamp goes. **Never** inside `TrackEvent`: field 1 there is
        /// `timestamp_delta_us`, so an absolute nanosecond stamp written into a `TrackEvent`
        /// decodes as a microsecond delta and lands the event somewhere else entirely.
        TIMESTAMP = 8 as "timestamp",
        /// Required in practice. Perfetto drops packets that name no sequence, since
        /// interning and incremental state are sequence-scoped.
        TRUSTED_PACKET_SEQUENCE_ID = 10 as "trusted_packet_sequence_id",
        TRACK_EVENT = 11 as "track_event",
        TRACK_DESCRIPTOR = 60 as "track_descriptor",
    }

    /// Declares a track: a row in the UI, and the thing every event names.
    track_descriptor = "TrackDescriptor" {
        UUID = 1 as "uuid",
        NAME = 2 as "name",
        PROCESS = 3 as "process",
        THREAD = 4 as "thread",
        PARENT_UUID = 5 as "parent_uuid",
        COUNTER = 8 as "counter",
        CHILD_ORDERING = 11 as "child_ordering",
        SIBLING_ORDER_RANK = 12 as "sibling_order_rank",
        DESCRIPTION = 14 as "description",
        PROCESS_ORDERING = 19 as "process_ordering",
        THREAD_ORDERING = 20 as "thread_ordering",
    }

    thread_descriptor = "ThreadDescriptor" {
        PID = 1 as "pid",
        TID = 2 as "tid",
        THREAD_NAME = 5 as "thread_name",
    }

    process_descriptor = "ProcessDescriptor" {
        PID = 1 as "pid",
        CMDLINE = 2 as "cmdline",
        PROCESS_NAME = 6 as "process_name",
        START_TIMESTAMP_NS = 7 as "start_timestamp_ns",
    }

    counter_descriptor = "CounterDescriptor" {
        TYPE = 1 as "type",
        CATEGORIES = 2 as "categories",
        UNIT = 3 as "unit",
        UNIT_MULTIPLIER = 4 as "unit_multiplier",
        IS_INCREMENTAL = 5 as "is_incremental",
        UNIT_NAME = 6 as "unit_name",
        Y_AXIS_SHARE_KEY = 7 as "y_axis_share_key",
    }

    track_event = "TrackEvent" {
        TYPE = 9 as "type",
        TRACK_UUID = 11 as "track_uuid",
        DEBUG_ANNOTATIONS = 4 as "debug_annotations",
        CATEGORIES = 22 as "categories",
        NAME = 23 as "name",
        COUNTER_VALUE = 30 as "counter_value",
        DOUBLE_COUNTER_VALUE = 44 as "double_counter_value",
        /// Listed so the two timestamp fields that must stay empty are named somewhere a
        /// reader will find them. gcscope writes neither: see `trace_packet::TIMESTAMP`.
        TIMESTAMP_DELTA_US = 1 as "timestamp_delta_us",
        TIMESTAMP_ABSOLUTE_US = 16 as "timestamp_absolute_us",
    }

    debug_annotation = "DebugAnnotation" {
        /// **Field 10, not field 1, and it looks like a typo.** The proto declares
        /// `oneof name_field { uint64 name_iid = 1; string name = 10; }`. Only one member of a
        /// oneof may be set, and field 1 is the interned id, so a string written there is read
        /// as a garbage IID that can collide with a real interned name. Do not "fix" this to 1.
        NAME = 10 as "name",
        BOOL_VALUE = 2 as "bool_value",
        INT_VALUE = 4 as "int_value",
        STRING_VALUE = 6 as "string_value",
    }
}

proto_enums! {
    track_event_type = "TrackEvent"."Type" {
        SLICE_BEGIN = 1 as "TYPE_SLICE_BEGIN",
        SLICE_END = 2 as "TYPE_SLICE_END",
        INSTANT = 3 as "TYPE_INSTANT",
        COUNTER = 4 as "TYPE_COUNTER",
    }

    /// How a track's children are arranged under it.
    child_tracks_ordering = "TrackDescriptor"."ChildTracksOrdering" {
        UNKNOWN = 0 as "UNKNOWN",
        LEXICOGRAPHIC = 1 as "LEXICOGRAPHIC",
        CHRONOLOGICAL = 2 as "CHRONOLOGICAL",
        EXPLICIT = 3 as "EXPLICIT",
    }

    /// How top-level process tracks are arranged. Only meaningful on the root descriptor.
    process_ordering = "TrackDescriptor"."ProcessOrdering" {
        UNSPECIFIED = 0 as "PROCESS_ORDERING_UNSPECIFIED",
        EXPLICIT = 1 as "PROCESS_ORDERING_EXPLICIT",
    }

    thread_ordering = "TrackDescriptor"."ThreadOrdering" {
        UNSPECIFIED = 0 as "THREAD_ORDERING_UNSPECIFIED",
        EXPLICIT = 1 as "THREAD_ORDERING_EXPLICIT",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tables are what the checker is handed, and an empty one passes every check in it.
    /// The declarations and the table entries come from the same macro arm, so this asks only
    /// whether that arm is still collecting — the numbers themselves are the descriptors' to
    /// confirm, not this test's.
    #[test]
    fn the_declarations_reach_the_tables_the_checker_reads() {
        assert!(FIELDS.contains(&("DebugAnnotation", "name", 10)));
        assert!(FIELDS.contains(&("Trace", "packet", 1)));
        assert!(ENUM_VALUES.contains(&("TrackEvent", "Type", "TYPE_SLICE_BEGIN", 1)));
        assert!(ENUM_VALUES.contains(&(
            "TrackDescriptor",
            "ProcessOrdering",
            "PROCESS_ORDERING_EXPLICIT",
            1
        )));
    }

    /// Inside one message, a field name and a field number each belong to one constant. Two
    /// constants sharing either would both pass the descriptor check while the code reads only
    /// one of them, which is how a copied line goes unnoticed.
    #[test]
    fn no_message_claims_a_field_or_a_number_twice() {
        for (index, first) in FIELDS.iter().enumerate() {
            for second in &FIELDS[index + 1..] {
                assert!(
                    !(first.0 == second.0 && (first.1 == second.1 || first.2 == second.2)),
                    "{first:?} and {second:?} claim the same field or the same number"
                );
            }
        }
    }

    /// The one constant whose correct value reads as a mistake. Pinned here as well as against
    /// the descriptors, so a "fix" back to 1 fails in the crate that made it rather than only
    /// on the leg with the `perfetto` package installed.
    #[test]
    fn a_debug_annotation_names_itself_at_field_ten() {
        assert_eq!(debug_annotation::NAME, 10);
    }
}
