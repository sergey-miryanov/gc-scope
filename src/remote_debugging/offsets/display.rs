#[macro_export]
macro_rules! impl_display_debug_offsets {
    ($ty:ty, $rst:ty, $ist:ty, $tst:ty, $est:ty, $ift:ty, $cot:ty, $pyt:ty, $tyt:ty,
     $hpt:ty, $tut:ty, $lit:ty, $sot:ty, $dit:ty, $flt:ty, $lot:ty, $byt:ty, $unt:ty,
     $gct:ty, $got:ty, $llt:ty, $dbt:ty) => {

        fn _fmt64(val: u64) -> String {
            if val == 0 { "0".to_string() } else { format!("{}", val) }
        }

        fn _write_section<T: ::std::fmt::Display>(
            f: &mut ::std::fmt::Formatter<'_>,
            name: &str,
            section: &T,
        ) -> ::std::fmt::Result {
            writeln!(f, "\n[{}]", name)?;
            write!(f, "{}", section)
        }

        impl ::std::fmt::Display for $ty {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                let cookie_str = {
                    let bytes: &[u8] = unsafe {
                        ::std::slice::from_raw_parts(
                            self.cookie.as_ptr() as *const u8,
                            self.cookie.len(),
                        )
                    };
                    let s = ::std::string::String::from_utf8_lossy(bytes);
                    let trimmed = s.trim_end_matches('\0');
                    let mark = if trimmed == "xdebugpy" { " ✓" } else { " ✗" };
                    format!("\"{}\"{}", trimmed, mark)
                };
                writeln!(f, "cookie:             {}", cookie_str)?;
                writeln!(f, "version:            {}", self.version)?;
                writeln!(f, "free_threaded:      {}", self.free_threaded)?;
                _write_section(f, "runtime_state", &self.runtime_state)?;
                _write_section(f, "interpreter_state", &self.interpreter_state)?;
                _write_section(f, "thread_state", &self.thread_state)?;
                _write_section(f, "err_stackitem", &self.err_stackitem)?;
                _write_section(f, "interpreter_frame", &self.interpreter_frame)?;
                _write_section(f, "code_object", &self.code_object)?;
                _write_section(f, "pyobject", &self.pyobject)?;
                _write_section(f, "type_object", &self.type_object)?;
                _write_section(f, "heap_type_object", &self.heap_type_object)?;
                _write_section(f, "tuple_object", &self.tuple_object)?;
                _write_section(f, "list_object", &self.list_object)?;
                _write_section(f, "set_object", &self.set_object)?;
                _write_section(f, "dict_object", &self.dict_object)?;
                _write_section(f, "float_object", &self.float_object)?;
                _write_section(f, "long_object", &self.long_object)?;
                _write_section(f, "bytes_object", &self.bytes_object)?;
                _write_section(f, "unicode_object", &self.unicode_object)?;
                _write_section(f, "gc", &self.gc)?;
                _write_section(f, "gen_object", &self.gen_object)?;
                _write_section(f, "llist_node", &self.llist_node)?;
                _write_section(f, "debugger_support", &self.debugger_support)
            }
        }

        impl ::std::fmt::Display for $rst {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "finalizing", _fmt64(self.finalizing))?;
                writeln!(f, "  {:<32} {}", "interpreters_head", _fmt64(self.interpreters_head))
            }
        }
        impl ::std::fmt::Display for $ist {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "id", _fmt64(self.id))?;
                writeln!(f, "  {:<32} {}", "next", _fmt64(self.next))?;
                writeln!(f, "  {:<32} {}", "threads_head", _fmt64(self.threads_head))?;
                writeln!(f, "  {:<32} {}", "threads_main", _fmt64(self.threads_main))?;
                writeln!(f, "  {:<32} {}", "gc", _fmt64(self.gc))?;
                writeln!(f, "  {:<32} {}", "imports_modules", _fmt64(self.imports_modules))?;
                writeln!(f, "  {:<32} {}", "sysdict", _fmt64(self.sysdict))?;
                writeln!(f, "  {:<32} {}", "builtins", _fmt64(self.builtins))?;
                writeln!(f, "  {:<32} {}", "ceval_gil", _fmt64(self.ceval_gil))?;
                writeln!(f, "  {:<32} {}", "gil_runtime_state", _fmt64(self.gil_runtime_state))?;
                writeln!(f, "  {:<32} {}", "gil_runtime_state_enabled", _fmt64(self.gil_runtime_state_enabled))?;
                writeln!(f, "  {:<32} {}", "gil_runtime_state_locked", _fmt64(self.gil_runtime_state_locked))?;
                writeln!(f, "  {:<32} {}", "gil_runtime_state_holder", _fmt64(self.gil_runtime_state_holder))?;
                writeln!(f, "  {:<32} {}", "code_object_generation", _fmt64(self.code_object_generation))?;
                writeln!(f, "  {:<32} {}", "tlbc_generation", _fmt64(self.tlbc_generation))
            }
        }
        impl ::std::fmt::Display for $tst {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "prev", _fmt64(self.prev))?;
                writeln!(f, "  {:<32} {}", "next", _fmt64(self.next))?;
                writeln!(f, "  {:<32} {}", "interp", _fmt64(self.interp))?;
                writeln!(f, "  {:<32} {}", "current_frame", _fmt64(self.current_frame))?;
                writeln!(f, "  {:<32} {}", "base_frame", _fmt64(self.base_frame))?;
                writeln!(f, "  {:<32} {}", "last_profiled_frame", _fmt64(self.last_profiled_frame))?;
                writeln!(f, "  {:<32} {}", "thread_id", _fmt64(self.thread_id))?;
                writeln!(f, "  {:<32} {}", "native_thread_id", _fmt64(self.native_thread_id))?;
                writeln!(f, "  {:<32} {}", "datastack_chunk", _fmt64(self.datastack_chunk))?;
                writeln!(f, "  {:<32} {}", "status", _fmt64(self.status))?;
                writeln!(f, "  {:<32} {}", "holds_gil", _fmt64(self.holds_gil))?;
                writeln!(f, "  {:<32} {}", "gil_requested", _fmt64(self.gil_requested))?;
                writeln!(f, "  {:<32} {}", "current_exception", _fmt64(self.current_exception))?;
                writeln!(f, "  {:<32} {}", "exc_state", _fmt64(self.exc_state))
            }
        }
        impl ::std::fmt::Display for $est {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "exc_value", _fmt64(self.exc_value))
            }
        }
        impl ::std::fmt::Display for $ift {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "previous", _fmt64(self.previous))?;
                writeln!(f, "  {:<32} {}", "executable", _fmt64(self.executable))?;
                writeln!(f, "  {:<32} {}", "instr_ptr", _fmt64(self.instr_ptr))?;
                writeln!(f, "  {:<32} {}", "localsplus", _fmt64(self.localsplus))?;
                writeln!(f, "  {:<32} {}", "owner", _fmt64(self.owner))?;
                writeln!(f, "  {:<32} {}", "stackpointer", _fmt64(self.stackpointer))?;
                writeln!(f, "  {:<32} {}", "tlbc_index", _fmt64(self.tlbc_index))
            }
        }
        impl ::std::fmt::Display for $cot {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "filename", _fmt64(self.filename))?;
                writeln!(f, "  {:<32} {}", "name", _fmt64(self.name))?;
                writeln!(f, "  {:<32} {}", "qualname", _fmt64(self.qualname))?;
                writeln!(f, "  {:<32} {}", "linetable", _fmt64(self.linetable))?;
                writeln!(f, "  {:<32} {}", "firstlineno", _fmt64(self.firstlineno))?;
                writeln!(f, "  {:<32} {}", "argcount", _fmt64(self.argcount))?;
                writeln!(f, "  {:<32} {}", "localsplusnames", _fmt64(self.localsplusnames))?;
                writeln!(f, "  {:<32} {}", "localspluskinds", _fmt64(self.localspluskinds))?;
                writeln!(f, "  {:<32} {}", "co_code_adaptive", _fmt64(self.co_code_adaptive))?;
                writeln!(f, "  {:<32} {}", "co_tlbc", _fmt64(self.co_tlbc))
            }
        }
        impl ::std::fmt::Display for $pyt {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "ob_type", _fmt64(self.ob_type))
            }
        }
        impl ::std::fmt::Display for $tyt {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "tp_name", _fmt64(self.tp_name))?;
                writeln!(f, "  {:<32} {}", "tp_repr", _fmt64(self.tp_repr))?;
                writeln!(f, "  {:<32} {}", "tp_flags", _fmt64(self.tp_flags))?;
                writeln!(f, "  {:<32} {}", "tp_basicsize", _fmt64(self.tp_basicsize))?;
                writeln!(f, "  {:<32} {}", "tp_dictoffset", _fmt64(self.tp_dictoffset))
            }
        }
        impl ::std::fmt::Display for $hpt {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "ht_cached_keys", _fmt64(self.ht_cached_keys))
            }
        }
        impl ::std::fmt::Display for $tut {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "ob_item", _fmt64(self.ob_item))?;
                writeln!(f, "  {:<32} {}", "ob_size", _fmt64(self.ob_size))
            }
        }
        impl ::std::fmt::Display for $lit {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "ob_item", _fmt64(self.ob_item))?;
                writeln!(f, "  {:<32} {}", "ob_size", _fmt64(self.ob_size))
            }
        }
        impl ::std::fmt::Display for $sot {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "used", _fmt64(self.used))?;
                writeln!(f, "  {:<32} {}", "table", _fmt64(self.table))?;
                writeln!(f, "  {:<32} {}", "mask", _fmt64(self.mask))
            }
        }
        impl ::std::fmt::Display for $dit {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "ma_keys", _fmt64(self.ma_keys))?;
                writeln!(f, "  {:<32} {}", "ma_values", _fmt64(self.ma_values))
            }
        }
        impl ::std::fmt::Display for $flt {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "ob_fval", _fmt64(self.ob_fval))
            }
        }
        impl ::std::fmt::Display for $lot {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "lv_tag", _fmt64(self.lv_tag))?;
                writeln!(f, "  {:<32} {}", "ob_digit", _fmt64(self.ob_digit))
            }
        }
        impl ::std::fmt::Display for $byt {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "ob_size", _fmt64(self.ob_size))?;
                writeln!(f, "  {:<32} {}", "ob_sval", _fmt64(self.ob_sval))
            }
        }
        impl ::std::fmt::Display for $unt {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "state", _fmt64(self.state))?;
                writeln!(f, "  {:<32} {}", "length", _fmt64(self.length))?;
                writeln!(f, "  {:<32} {}", "asciiobject_size", _fmt64(self.asciiobject_size))?;
                writeln!(f, "  {:<32} {}", "compactunicodeobject_size", _fmt64(self.compactunicodeobject_size))
            }
        }
        impl ::std::fmt::Display for $gct {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "collecting", _fmt64(self.collecting))?;
                writeln!(f, "  {:<32} {}", "frame", _fmt64(self.frame))?;
                writeln!(f, "  {:<32} {}", "generation_stats_size", _fmt64(self.generation_stats_size))?;
                writeln!(f, "  {:<32} {}", "generation_stats", _fmt64(self.generation_stats))
            }
        }
        impl ::std::fmt::Display for $got {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "size", _fmt64(self.size))?;
                writeln!(f, "  {:<32} {}", "gi_name", _fmt64(self.gi_name))?;
                writeln!(f, "  {:<32} {}", "gi_iframe", _fmt64(self.gi_iframe))?;
                writeln!(f, "  {:<32} {}", "gi_frame_state", _fmt64(self.gi_frame_state))
            }
        }
        impl ::std::fmt::Display for $llt {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "next", _fmt64(self.next))?;
                writeln!(f, "  {:<32} {}", "prev", _fmt64(self.prev))
            }
        }
        impl ::std::fmt::Display for $dbt {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                writeln!(f, "  {:<32} {}", "eval_breaker", _fmt64(self.eval_breaker))?;
                writeln!(f, "  {:<32} {}", "remote_debugger_support", _fmt64(self.remote_debugger_support))?;
                writeln!(f, "  {:<32} {}", "remote_debugging_enabled", _fmt64(self.remote_debugging_enabled))?;
                writeln!(f, "  {:<32} {}", "debugger_pending_call", _fmt64(self.debugger_pending_call))?;
                writeln!(f, "  {:<32} {}", "debugger_script_path", _fmt64(self.debugger_script_path))?;
                writeln!(f, "  {:<32} {}", "debugger_script_path_size", _fmt64(self.debugger_script_path_size))
            }
        }
    };
}
