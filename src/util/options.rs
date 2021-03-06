use crate::util::constants::DEFAULT_STRESS_FACTOR;
use std::cell::UnsafeCell;
use std::default::Default;
use std::ops::Deref;

custom_derive! {
    #[derive(Copy, Clone, EnumFromStr, Debug)]
    pub enum NurseryZeroingOptions {
        Temporal,
        Nontemporal,
        Concurrent,
        Adaptive,
    }
}

custom_derive! {
    #[derive(Copy, Clone, EnumFromStr, Debug)]
    pub enum PlanSelector {
        NoGC,
        SemiSpace,
        GenCopy,
        MarkSweep
    }
}

pub struct UnsafeOptionsWrapper(UnsafeCell<Options>);

// TODO: We should carefully examine the unsync with UnsafeCell. We should be able to provide a safe implementation.
unsafe impl Sync for UnsafeOptionsWrapper {}

impl UnsafeOptionsWrapper {
    pub const fn new(o: Options) -> UnsafeOptionsWrapper {
        UnsafeOptionsWrapper(UnsafeCell::new(o))
    }
    /// # Safety
    /// This method is not thread safe, as internally it acquires a mutable reference to self.
    /// It is supposed to be used by one thread during boot time.
    pub unsafe fn process(&self, name: &str, value: &str) -> bool {
        (&mut *self.0.get()).set_from_camelcase_str(name, value)
    }
}
impl Deref for UnsafeOptionsWrapper {
    type Target = Options;
    fn deref(&self) -> &Options {
        unsafe { &*self.0.get() }
    }
}

fn always_valid<T>(_: &T) -> bool {
    true
}
macro_rules! options {
    ($($name:ident: $type:ty[$validator:expr] = $default:expr),*,) => [
        options!($($name: $type[$validator] = $default),*);
    ];
    ($($name:ident: $type:ty[$validator:expr] = $default:expr),*) => [
        pub struct Options {
            $(pub $name: $type),*
        }
        impl Options {
            pub fn set_from_str(&mut self, s: &str, val: &str)->bool {
                match s {
                    // Parse the given value from str (by env vars or by calling process()) to the right type
                    $(stringify!($name) => if let Ok(ref val) = val.parse::<$type>() {
                        // Validate
                        let validate_fn = $validator;
                        let is_valid = validate_fn(val);
                        if is_valid {
                            // Only set value if valid.
                            self.$name = val.clone();
                        } else {
                            eprintln!("Warn: unable to set {}={:?}. Invalid value. Default value will be used.", s, val);
                        }
                        is_valid
                    } else {
                        eprintln!("Warn: unable to set {}={:?}. Cant parse value. Default value will be used.", s, val);
                        false
                    })*
                    _ => panic!("Invalid Options key")
                }
            }
        }
        impl Default for Options {
            fn default() -> Self {
                let mut options = Options {
                    $($name: $default),*
                };

                // If we have env vars that start with MMTK_ and match any option (such as MMTK_STRESS_FACTOR),
                // we set the option to its value (if it is a valid value). Otherwise, use the default value.
                const PREFIX: &str = "MMTK_";
                for (key, val) in std::env::vars() {
                    // strip the prefix, and get the lower case string
                    if let Some(rest_of_key) = key.strip_prefix(PREFIX) {
                        let lowercase: &str = &rest_of_key.to_lowercase();
                        match lowercase {
                            $(stringify!($name) => { options.set_from_str(lowercase, &val); },)*
                            _ => {}
                        }
                    }
                }
                return options;
            }
        }
    ]
}
options! {
    // The plan to use. This needs to be initialized before creating an MMTk instance (currently by setting env vars)
    plan:                  PlanSelector         [always_valid] = PlanSelector::NoGC,
    // Number of GC threads.
    threads:               usize                [|v: &usize| *v > 0]    = num_cpus::get(),
    // Enable an optimization that only scans the part of the stack that has changed since the last GC (not supported)
    use_short_stack_scans: bool                 [always_valid] = false,
    // Enable a return barrier (not supported)
    use_return_barrier:    bool                 [always_valid] = false,
    // Should we eagerly finish sweeping at the start of a collection? (not supported)
    eager_complete_sweep:  bool                 [always_valid] = false,
    // Should we ignore GCs requested by the user (e.g. java.lang.System.gc)?
    ignore_system_g_c:     bool                 [always_valid] = false,
    // The upper bound of nursery size. This needs to be initialized before creating an MMTk instance (currently by setting env vars)
    max_nursery:           usize                [|v: &usize| *v > 0]    = (32 * 1024 * 1024),
    // The lower bound of nusery size. This needs to be initialized before creating an MMTk instance (currently by setting env vars)
    min_nursery:           usize                [|v: &usize| *v > 0]    = (32 * 1024 * 1024),
    // Should a major GC be performed when a system GC is required?
    full_heap_system_gc:   bool                 [always_valid] = false,
    // Should we shrink/grow the heap to adjust to application working set? (not supported)
    variable_size_heap:    bool                 [always_valid] = true,
    // Should finalization be disabled?
    no_finalizer:          bool                 [always_valid] = false,
    // Should reference type processing be disabled?
    no_reference_types:    bool                 [always_valid] = false,
    // The zeroing approach to use for new object allocations. Affects each plan differently. (not supported)
    nursery_zeroing:       NurseryZeroingOptions[always_valid] = NurseryZeroingOptions::Temporal,
    // How frequent (every X bytes) should we do a stress GC?
    stress_factor:         usize                [always_valid] = DEFAULT_STRESS_FACTOR,
    // How frequent (every X bytes) should we run analysis (a STW event that collects data)
    analysis_factor:       usize                [always_valid] = DEFAULT_STRESS_FACTOR,
    // The size of vmspace. This needs to be initialized before creating an MMTk instance (currently by setting env vars)
    // FIXME: This value is set for JikesRVM. We need a proper way to set options.
    //   We need to set these values programmatically in VM specific code.
    vm_space_size:         usize                [|v: &usize| *v > 0]    = 0x7cc_cccc,
    // An example string option. Can be deleted when we have other string options.
    // Make sure to include the string option tests in the unit tests.
    example_string_option: String                [|v: &str| v.starts_with("hello") ] = "hello world".to_string(),
}

impl Options {
    fn set_from_camelcase_str(&mut self, s: &str, val: &str) -> bool {
        trace!("Trying to process option pair: ({}, {})", s, val);

        let mut sr = String::with_capacity(s.len());
        for c in s.chars() {
            if c.is_uppercase() {
                sr.push('_');
                for c in c.to_lowercase() {
                    sr.push(c);
                }
            } else {
                sr.push(c)
            }
        }

        let result = self.set_from_str(sr.as_str(), val);

        trace!("Trying to process option pair: ({})", sr);

        if result {
            trace!("Validation passed");
        } else {
            trace!("Validation failed")
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use crate::util::constants::DEFAULT_STRESS_FACTOR;
    use crate::util::options::Options;
    use crate::util::test_util::{serial_test, with_cleanup};

    #[test]
    fn no_env_var() {
        serial_test(|| {
            let options = Options::default();
            assert_eq!(options.stress_factor, DEFAULT_STRESS_FACTOR);
        })
    }

    #[test]
    fn with_valid_env_var() {
        serial_test(|| {
            with_cleanup(
                || {
                    std::env::set_var("MMTK_STRESS_FACTOR", "4096");

                    let options = Options::default();
                    assert_eq!(options.stress_factor, 4096);
                },
                || {
                    std::env::remove_var("MMTK_STRESS_FACTOR");
                },
            )
        })
    }

    #[test]
    fn with_multiple_valid_env_vars() {
        serial_test(|| {
            with_cleanup(
                || {
                    std::env::set_var("MMTK_STRESS_FACTOR", "4096");
                    std::env::set_var("MMTK_NO_FINALIZER", "true");

                    let options = Options::default();
                    assert_eq!(options.stress_factor, 4096);
                    assert!(options.no_finalizer);
                },
                || {
                    std::env::remove_var("MMTK_STRESS_FACTOR");
                    std::env::remove_var("MMTK_NO_FINALIZER");
                },
            )
        })
    }

    #[test]
    fn with_invalid_env_var_value() {
        serial_test(|| {
            with_cleanup(
                || {
                    // invalid value, we cannot parse the value, so use the default value
                    std::env::set_var("MMTK_STRESS_FACTOR", "abc");

                    let options = Options::default();
                    assert_eq!(options.stress_factor, DEFAULT_STRESS_FACTOR);
                },
                || {
                    std::env::remove_var("MMTK_STRESS_FACTOR");
                },
            )
        })
    }

    #[test]
    fn with_invalid_env_var_key() {
        serial_test(|| {
            with_cleanup(
                || {
                    // invalid value, we cannot parse the value, so use the default value
                    std::env::set_var("MMTK_ABC", "42");

                    let options = Options::default();
                    assert_eq!(options.stress_factor, DEFAULT_STRESS_FACTOR);
                },
                || {
                    std::env::remove_var("MMTK_ABC");
                },
            )
        })
    }

    #[test]
    fn test_str_option_default() {
        serial_test(|| {
            let options = Options::default();
            assert_eq!(&options.example_string_option as &str, "hello world");
        })
    }

    #[test]
    fn test_str_option_from_env_var() {
        serial_test(|| {
            with_cleanup(
                || {
                    std::env::set_var("MMTK_EXAMPLE_STRING_OPTION", "hello string");

                    let options = Options::default();
                    assert_eq!(&options.example_string_option as &str, "hello string");
                },
                || {
                    std::env::remove_var("MMTK_EXAMPLE_STRING_OPTION");
                },
            )
        })
    }

    #[test]
    fn test_invalid_str_option_from_env_var() {
        serial_test(|| {
            with_cleanup(
                || {
                    // The option needs to start with "hello", otherwise it is invalid.
                    std::env::set_var("MMTK_EXAMPLE_STRING_OPTION", "abc");

                    let options = Options::default();
                    // invalid value from env var, use default.
                    assert_eq!(&options.example_string_option as &str, "hello world");
                },
                || {
                    std::env::remove_var("MMTK_EXAMPLE_STRING_OPTION");
                },
            )
        })
    }
}
