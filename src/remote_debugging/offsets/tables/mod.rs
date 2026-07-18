use crate::remote_debugging::offsets::offset_table::DebugOffsetsLayout;

/// Return the `DebugOffsetsLayout` for a known version hex, or `None`.
pub fn layout_for_version(version_hex: u64) -> Option<&'static DebugOffsetsLayout> {
    match version_hex {
        0x030d0df0 => Some(&v_3_13_13_53e07256802::LAYOUT),
        0x030d01f0 => Some(&v_3_13_1::LAYOUT),
        0x030e04f0 => Some(&v_3_14_4::LAYOUT),
        0x030f00b1 => Some(&v_3_15_0b1_6a660056998::LAYOUT),
        0x030f00a7 => Some(&v_3_15_0a7::LAYOUT),
        _ => None,
    }
}

mod v_3_13_13_53e07256802;
mod v_3_13_1;
mod v_3_14_4;
mod v_3_15_0b1_6a660056998;
mod v_3_15_0a7;
