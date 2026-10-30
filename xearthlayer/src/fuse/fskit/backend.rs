//! Choosing between the FSKit module and macFUSE on macOS.
//!
//! The choice is made at run time, not with `#[cfg]`: the same binary has to
//! work on macOS 26, where the FSKit module can be installed, and on earlier
//! releases, where only macFUSE can mount. `auto` mirrors macFUSE's own
//! `backend=auto`: FSKit when it is usable, the kernel extension otherwise.

use std::str::FromStr;

/// The `fuse.macos_backend` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MacosBackend {
    /// FSKit when the module is installed and enabled, macFUSE otherwise.
    #[default]
    Auto,
    /// Always the FSKit module.
    Fskit,
    /// Always macFUSE.
    Macfuse,
}

impl FromStr for MacosBackend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "fskit" => Ok(Self::Fskit),
            "macfuse" => Ok(Self::Macfuse),
            other => Err(format!(
                "unknown macOS backend '{other}': expected auto, fskit or macfuse"
            )),
        }
    }
}

/// What `pluginkit` reports about the FSKit module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleState {
    /// Not registered: not installed, or macOS is older than 26.
    Missing,
    /// Registered but turned off by the user.
    Disabled,
    /// Registered and not turned off.
    Available,
}

/// The end of the module's bundle id. The prefix depends on who signs the
/// build (the bundle ids must match the signing team's App IDs), so the
/// module is recognised by this suffix alone.
pub const MODULE_BUNDLE_SUFFIX: &str = ".xearthlayer.fskit";

/// The FSKit extension point, for `pluginkit -m -p`.
pub const FSKIT_EXTENSION_POINT: &str = "com.apple.fskit.fsmodule";

/// Read the module's state from `pluginkit -m -p com.apple.fskit.fsmodule`.
///
/// pluginkit prints one line per FSKit module: a few columns of flags, then
/// `<bundle id>(<version>)`. Among the flags, `-` means the user rejected the
/// plug-in; other letters (`+` chosen, `F`, ...) do not stop it loading.
/// XEarthLayer's is the line whose bundle id ends in [`MODULE_BUNDLE_SUFFIX`];
/// no such line means no module.
pub fn parse_pluginkit_listing(output: &str) -> ModuleState {
    for line in output.lines() {
        let Some((head, _version)) = line.split_once('(') else {
            continue;
        };
        let head = head.trim();
        let (flags, id) = head.rsplit_once(char::is_whitespace).unwrap_or(("", head));
        if id.ends_with(MODULE_BUNDLE_SUFFIX) {
            return if flags.contains('-') {
                ModuleState::Disabled
            } else {
                ModuleState::Available
            };
        }
    }
    ModuleState::Missing
}

/// The transport a mount will use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Fskit,
    Macfuse,
}

/// The transport to try first, and whether macFUSE may be tried after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub first: Transport,
    pub fall_back_to_macfuse: bool,
}

/// Decide how to mount from the setting and the module's state.
///
/// An explicit `fskit` never falls back: a user who asked for no kernel
/// extension should get an error naming the problem, not a silent kext mount.
pub fn plan(setting: MacosBackend, module: ModuleState) -> Plan {
    match (setting, module) {
        (MacosBackend::Macfuse, _) | (MacosBackend::Auto, ModuleState::Missing) => Plan {
            first: Transport::Macfuse,
            fall_back_to_macfuse: false,
        },
        (MacosBackend::Auto, ModuleState::Disabled) => Plan {
            first: Transport::Macfuse,
            fall_back_to_macfuse: false,
        },
        (MacosBackend::Auto, ModuleState::Available) => Plan {
            first: Transport::Fskit,
            fall_back_to_macfuse: true,
        },
        (MacosBackend::Fskit, _) => Plan {
            first: Transport::Fskit,
            fall_back_to_macfuse: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "net.defreyssi.xearthlayer.fskit";

    #[test]
    fn the_setting_parses_case_insensitively() {
        assert_eq!("auto".parse(), Ok(MacosBackend::Auto));
        assert_eq!(" FSKit ".parse(), Ok(MacosBackend::Fskit));
        assert_eq!("macFUSE".parse(), Ok(MacosBackend::Macfuse));
        assert!("kext".parse::<MacosBackend>().is_err());
    }

    #[test]
    fn the_setting_values_match_the_config_values() {
        for value in crate::config::defaults::FUSE_MACOS_BACKEND_VALUES {
            assert!(value.parse::<MacosBackend>().is_ok(), "{value}");
        }
    }

    /// What pluginkit printed on macOS 26 for the system's own modules.
    const SYSTEM_MODULES: &str = "     com.apple.fskit.ftp((null))\tE66C2ACE\t2026-08-27\t/System/Library/ExtensionKit/Extensions/com.apple.fskit.ftp.appex
     com.apple.fskit.exfat((null))\t108631B6\t2026-08-27\t/System/Library/ExtensionKit/Extensions/com.apple.fskit.exfat.appex
";

    #[test]
    fn no_listing_means_missing() {
        assert_eq!(parse_pluginkit_listing(""), ModuleState::Missing);
        assert_eq!(
            parse_pluginkit_listing(SYSTEM_MODULES),
            ModuleState::Missing
        );
    }

    #[test]
    fn a_rejected_election_is_disabled() {
        let out = format!("{SYSTEM_MODULES}-    {ID}(0.5.0)\n");
        assert_eq!(parse_pluginkit_listing(&out), ModuleState::Disabled);
    }

    #[test]
    fn a_chosen_or_unelected_module_is_available() {
        for election in ["+", " ", "!"] {
            let out = format!("{SYSTEM_MODULES}{election}    {ID}(0.5.0)\n");
            assert_eq!(
                parse_pluginkit_listing(&out),
                ModuleState::Available,
                "election {election:?}"
            );
        }
    }

    #[test]
    fn a_line_with_flag_columns_is_read() {
        // Verbatim from macOS 26.6 for a freshly registered, unelected module.
        let out = format!("   F {ID}(0.5.0)\n{SYSTEM_MODULES}");
        assert_eq!(parse_pluginkit_listing(&out), ModuleState::Available);
        let out = format!("-  F {ID}(0.5.0)\n");
        assert_eq!(parse_pluginkit_listing(&out), ModuleState::Disabled);
    }

    #[test]
    fn the_module_is_found_whatever_its_bundle_prefix() {
        let out = "+    com.x-z7a.xearthlayer.fskit(0.5.0)\n";
        assert_eq!(parse_pluginkit_listing(out), ModuleState::Available);
    }

    #[test]
    fn a_bundle_id_merely_containing_the_suffix_is_not_the_module() {
        let out = "+    com.example.xearthlayer.fskit.helper(1.0)\n";
        assert_eq!(parse_pluginkit_listing(out), ModuleState::Missing);
    }

    #[test]
    fn auto_uses_fskit_when_available_and_may_fall_back() {
        assert_eq!(
            plan(MacosBackend::Auto, ModuleState::Available),
            Plan {
                first: Transport::Fskit,
                fall_back_to_macfuse: true
            }
        );
    }

    #[test]
    fn auto_uses_macfuse_without_a_usable_module() {
        for state in [ModuleState::Missing, ModuleState::Disabled] {
            assert_eq!(plan(MacosBackend::Auto, state).first, Transport::Macfuse);
        }
    }

    #[test]
    fn an_explicit_fskit_never_falls_back() {
        for state in [
            ModuleState::Missing,
            ModuleState::Disabled,
            ModuleState::Available,
        ] {
            assert_eq!(
                plan(MacosBackend::Fskit, state),
                Plan {
                    first: Transport::Fskit,
                    fall_back_to_macfuse: false
                }
            );
        }
    }

    #[test]
    fn an_explicit_macfuse_ignores_the_module() {
        assert_eq!(
            plan(MacosBackend::Macfuse, ModuleState::Available).first,
            Transport::Macfuse
        );
    }
}
