use crate::{Error, Result};
use std::ffi::{OsStr, OsString};

/// Validate raw arguments against a Browser-managed proxy before lowering it.
pub(crate) fn validate_proxy_args(args: &[OsString]) -> Result<()> {
    for arg in args {
        if matches!(
            validated_switch_name(arg)?.as_deref(),
            Some("proxy-server" | "no-proxy-server" | "proxy-pac-url" | "proxy-auto-detect")
        ) {
            return Err(Error::InvalidArgument(
                "raw proxy routing switch conflicts with the explicit proxy".into(),
            ));
        }
    }
    Ok(())
}

/// Inspect Chrome's switch spelling without changing the original OS argument.
/// Reject switches that override launcher ownership or stop switch parsing.
pub(crate) fn validated_switch_name(arg: &OsStr) -> Result<Option<String>> {
    let text = arg.to_string_lossy();
    // Chromium base/strings/whitespace_constants.h: BOM is not whitespace.
    #[cfg(windows)]
    let text = text.trim_matches(|c| {
        matches!(
            c,
            '\u{9}'..='\u{d}'
                | ' '
                | '\u{85}'
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
        )
    });
    #[cfg(not(windows))]
    let text = text.trim_matches(['\t', '\n', '\u{b}', '\u{c}', '\r', ' ']);
    if text == "--" {
        return Err(Error::InvalidArgument(
            "the -- terminator would disable launcher arguments".into(),
        ));
    }
    let flag = text.strip_prefix("--").or_else(|| text.strip_prefix('-'));
    #[cfg(windows)]
    let flag = flag.or_else(|| text.strip_prefix('/'));
    let Some(flag) = flag.filter(|flag| !flag.is_empty()) else {
        return Ok(None);
    };
    let name = flag.split('=').next().unwrap_or_default();
    #[cfg(windows)]
    let name = name.to_ascii_lowercase();
    #[cfg(not(windows))]
    let name = name.to_owned();
    if name.starts_with("user-data-dir") || name.starts_with("remote-debugging-") {
        return Err(Error::InvalidArgument(
            "use builder or LaunchOptions fields for profile/debugging settings".into(),
        ));
    }
    #[cfg(windows)]
    if name == "single-argument" {
        return Err(Error::InvalidArgument(
            "single-argument would disable launcher arguments".into(),
        ));
    }
    Ok(Some(name))
}
