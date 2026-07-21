use std::fmt;

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

impl Check {
    pub fn new(name: impl Into<String>, passed: bool, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            passed,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ValidationReport {
    pub checks: Vec<Check>,
}

impl ValidationReport {
    pub fn new(checks: Vec<Check>) -> Self {
        ValidationReport { checks }
    }
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "\n[validation]")?;
        let mut all_passed = true;
        for check in &self.checks {
            let mark = if check.passed { "✓" } else { all_passed = false; "✗" };
            writeln!(f, "  {:<24} {}  {}", check.name, mark, check.detail)?;
        }
        if all_passed {
            writeln!(f, "  all checks passed ✓")?;
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! impl_validate_debug_offsets {
    ($main:ty, $rst:ty, $ist:ty, $tst:ty, $est:ty, $ift:ty, $cot:ty, $pyt:ty, $tyt:ty,
     $hpt:ty, $tut:ty, $lit:ty, $sot:ty, $dit:ty, $flt:ty, $lot:ty, $byt:ty, $unt:ty,
     $gct:ty, $got:ty, $llt:ty, $dbt:ty) => {

        fn _check_section<T>(f: &T) -> u64
        where
            T: SectionSize,
        {
            f.section_size()
        }

        trait SectionSize {
            fn section_size(&self) -> u64;
        }

        impl SectionSize for $rst { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $ist { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $tst { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $ift { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $cot { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $pyt { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $tyt { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $hpt { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $tut { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $lit { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $sot { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $dit { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $flt { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $lot { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $byt { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $unt { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $gct { fn section_size(&self) -> u64 { self.size } }
        impl SectionSize for $got { fn section_size(&self) -> u64 { self.size } }

        pub fn validate_offsets(
            off: &$main,
            expected_version: u64,
        ) -> $crate::remote_debugging::offsets::validation::ValidationReport {
            let mut checks = Vec::new();

            // Cookie
            let cookie_bytes: &[u8] = unsafe {
                ::std::slice::from_raw_parts(
                    off.cookie.as_ptr() as *const u8,
                    off.cookie.len(),
                )
            };
            let cookie_ok = cookie_bytes.starts_with(b"xdebugpy");
            checks.push($crate::remote_debugging::offsets::validation::Check::new(
                "cookie",
                cookie_ok,
                if cookie_ok { "\"xdebugpy\"" } else { "mismatch" },
            ));

            // Version
            let ver_ok = off.version == expected_version;
            checks.push($crate::remote_debugging::offsets::validation::Check::new(
                "version",
                ver_ok,
                format!("expected 0x{:08x}, got 0x{:08x}",
                    expected_version, off.version),
            ));

            // Free-threaded
            let ft_passed = off.free_threaded == 0;
            checks.push($crate::remote_debugging::offsets::validation::Check::new(
                "free_threaded",
                ft_passed,
                format!("{} (expected 0)", off.free_threaded),
            ));

            // Section sizes
            macro_rules! check_size {
                ($name:expr, $sec:expr) => {
                    let sz = _check_section(&$sec);
                    let ok = sz > 0;
                    checks.push($crate::remote_debugging::offsets::validation::Check::new(
                        $name,
                        ok,
                        if ok { format!("{}", sz) } else { "0 (expected > 0)".into() },
                    ));
                };
            }
            check_size!("runtime_state.size", off.runtime_state);
            check_size!("interpreter_state.size", off.interpreter_state);
            check_size!("thread_state.size", off.thread_state);
            check_size!("interpreter_frame.size", off.interpreter_frame);
            check_size!("code_object.size", off.code_object);
            check_size!("pyobject.size", off.pyobject);
            check_size!("type_object.size", off.type_object);
            check_size!("heap_type_object.size", off.heap_type_object);
            check_size!("tuple_object.size", off.tuple_object);
            check_size!("list_object.size", off.list_object);
            check_size!("set_object.size", off.set_object);
            check_size!("dict_object.size", off.dict_object);
            check_size!("float_object.size", off.float_object);
            check_size!("long_object.size", off.long_object);
            check_size!("bytes_object.size", off.bytes_object);
            check_size!("unicode_object.size", off.unicode_object);
            check_size!("gc.size", off.gc);
            check_size!("gen_object.size", off.gen_object);

            // Key field bounds validation
            macro_rules! check_field {
                ($prefix:expr, $field:expr, $size:expr) => {
                    let w: u64 = 8;
                    let ok = $field + w <= $size;
                    checks.push($crate::remote_debugging::offsets::validation::Check::new(
                        $prefix,
                        ok,
                        if ok {
                            format!("{} + {} <= {}", $field, w, $size)
                        } else {
                            format!("{} + {} > {} (out of bounds)", $field, w, $size)
                        },
                    ));
                };
            }

            check_field!("runtime_state.finalizing",
                off.runtime_state.finalizing, off.runtime_state.size);
            check_field!("runtime_state.interpreters_head",
                off.runtime_state.interpreters_head, off.runtime_state.size);
            check_field!("interpreter_state.gc",
                off.interpreter_state.gc, off.interpreter_state.size);
            check_field!("interpreter_state.threads_head",
                off.interpreter_state.threads_head, off.interpreter_state.size);
            check_field!("interpreter_state.threads_main",
                off.interpreter_state.threads_main, off.interpreter_state.size);
            check_field!("thread_state.interp",
                off.thread_state.interp, off.thread_state.size);
            check_field!("thread_state.current_frame",
                off.thread_state.current_frame, off.thread_state.size);
            check_field!("gc.collecting",
                off.gc.collecting, off.gc.size);
            check_field!("gc.frame",
                off.gc.frame, off.gc.size);
            check_field!("gc.generation_stats",
                off.gc.generation_stats, off.gc.size);
            check_field!("interpreter_frame.previous",
                off.interpreter_frame.previous, off.interpreter_frame.size);
            check_field!("code_object.filename",
                off.code_object.filename, off.code_object.size);
            check_field!("code_object.co_code_adaptive",
                off.code_object.co_code_adaptive, off.code_object.size);
            check_field!("set_object.table",
                off.set_object.table, off.set_object.size);
            check_field!("dict_object.ma_keys",
                off.dict_object.ma_keys, off.dict_object.size);

            $crate::remote_debugging::offsets::validation::ValidationReport::new(checks)
        }
    };
}



#[cfg(test)]
mod tests {
    use super::*;

    /// When every check passes, the report prints the summary line and no `✗`
    /// marker — the summary is the human's at-a-glance "this build is good" signal.
    #[test]
    fn all_passing_report_prints_the_summary_line() {
        let report = ValidationReport::new(vec![
            Check::new("cookie", true, "\"xdebugpy\""),
            Check::new("version", true, "0x030f00b1"),
        ]);
        let out = report.to_string();
        assert!(out.contains("all checks passed ✓"), "output: {out}");
        assert!(!out.contains('✗'), "output: {out}");
    }

    /// A single failing check must suppress the "all passed" summary and surface a
    /// `✗` — otherwise a wrong offset would validate as healthy.
    #[test]
    fn a_single_failure_suppresses_the_summary_and_marks_the_row() {
        let report = ValidationReport::new(vec![
            Check::new("cookie", true, "\"xdebugpy\""),
            Check::new("gc.size", false, "0 (expected > 0)"),
        ]);
        let out = report.to_string();
        assert!(!out.contains("all checks passed"), "output: {out}");
        assert!(out.contains('✗'), "output: {out}");
        // The failing check's name and detail are both rendered for diagnosis.
        assert!(out.contains("gc.size"), "output: {out}");
        assert!(out.contains("0 (expected > 0)"), "output: {out}");
    }

    /// An empty report has nothing to fail, so it reports success — the `all_passed`
    /// flag starts true and stays true with no rows to flip it.
    #[test]
    fn an_empty_report_counts_as_all_passed() {
        let out = ValidationReport::new(vec![]).to_string();
        assert!(out.contains("all checks passed ✓"), "output: {out}");
    }

    #[test]
    fn check_new_accepts_str_and_string_and_stores_fields() {
        let c = Check::new("version", false, format!("got {:#x}", 0x030d01f0u64));
        assert_eq!(c.name, "version");
        assert!(!c.passed);
        assert_eq!(c.detail, "got 0x30d01f0");
    }
}
