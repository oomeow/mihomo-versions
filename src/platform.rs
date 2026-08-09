use std::fmt;

use crate::error::Error;

/// A publish platform: an `OS × architecture` combination covering the
/// consumers' targets: macOS, Windows and Linux across the general arch
/// vocabulary — `x86_64`, `aarch64`, and the 32-bit `x86` / `arm` (Windows
/// and Linux only; macOS publishes no 32-bit builds). Variant names follow
/// the general platform vocabulary (macOS); `as_str` identifies the platform
/// (`darwin-x86_64`, ...). Asset-name aliases (`darwin-amd64`, `linux-386`)
/// are matched through the classifier config patterns, not the enum.
/// Platforms outside these (freebsd, android, mips, ...) are handled by
/// custom classifier configs or ignored by the default rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    DarwinX86_64,
    DarwinAarch64,
    WindowsX86_64,
    WindowsAarch64,
    WindowsX86,
    WindowsArm,
    LinuxX86_64,
    LinuxAarch64,
    LinuxX86,
    LinuxArm,
}

impl Platform {
    /// All supported platforms, in canonical order.
    pub const ALL: [Platform; 10] = [
        Platform::DarwinX86_64,
        Platform::DarwinAarch64,
        Platform::WindowsX86_64,
        Platform::WindowsAarch64,
        Platform::WindowsX86,
        Platform::WindowsArm,
        Platform::LinuxX86_64,
        Platform::LinuxAarch64,
        Platform::LinuxX86,
        Platform::LinuxArm,
    ];

    /// Canonical identifier written to the index (e.g. `darwin-x86_64`).
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::DarwinX86_64 => "darwin-x86_64",
            Platform::DarwinAarch64 => "darwin-aarch64",
            Platform::WindowsX86_64 => "windows-x86_64",
            Platform::WindowsAarch64 => "windows-aarch64",
            Platform::WindowsX86 => "windows-x86",
            Platform::WindowsArm => "windows-arm",
            Platform::LinuxX86_64 => "linux-x86_64",
            Platform::LinuxAarch64 => "linux-aarch64",
            Platform::LinuxX86 => "linux-x86",
            Platform::LinuxArm => "linux-arm",
        }
    }

    /// Detects the current platform from compile-time OS/arch constants.
    pub fn current() -> Result<Self, Error> {
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "windows",
            "linux" => "linux",
            other => return Err(Error::UnsupportedPlatform(other.to_string())),
        };
        let Some(arch) = normalize_arch(std::env::consts::ARCH) else {
            return Err(Error::UnsupportedPlatform(format!("{os}-{}", std::env::consts::ARCH)));
        };
        parse_platform(&format!("{os}-{arch}"))
    }
}
impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Normalizes an architecture string to one of the canonical arch words
/// (`x86` / `x86_64` / `arm` / `aarch64`), accepting the common aliases used
/// across toolchains. Returns `None` for unrecognized architectures.
pub fn normalize_arch(s: &str) -> Option<&'static str> {
    const ALIASES: &[(&str, &str)] = &[
        ("x86", "x86"),
        ("386", "x86"),
        ("i386", "x86"),
        ("i686", "x86"),
        ("x86_64", "x86_64"),
        ("amd64", "x86_64"),
        ("x64", "x86_64"),
        ("x86-64", "x86_64"),
        ("arm", "arm"),
        ("armv7", "arm"),
        ("armhf", "arm"),
        ("aarch64", "aarch64"),
        ("arm64", "aarch64"),
    ];
    ALIASES.iter().find(|(alias, _)| s.eq_ignore_ascii_case(alias)).map(|(_, canonical)| *canonical)
}

/// Parses a canonical platform identifier (e.g. `darwin-x86_64`) into a
/// `Platform`. Unknown identifiers (freebsd, android, linux-mips, ...) are
/// rejected with `Error::UnsupportedPlatform`.
pub fn parse_platform(s: &str) -> Result<Platform, Error> {
    match s {
        "darwin-x86_64" => Ok(Platform::DarwinX86_64),
        "darwin-aarch64" => Ok(Platform::DarwinAarch64),
        "windows-x86_64" => Ok(Platform::WindowsX86_64),
        "windows-aarch64" => Ok(Platform::WindowsAarch64),
        "windows-x86" => Ok(Platform::WindowsX86),
        "windows-arm" => Ok(Platform::WindowsArm),
        "linux-x86_64" => Ok(Platform::LinuxX86_64),
        "linux-aarch64" => Ok(Platform::LinuxAarch64),
        "linux-x86" => Ok(Platform::LinuxX86),
        "linux-arm" => Ok(Platform::LinuxArm),
        other => Err(Error::UnsupportedPlatform(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_platforms() {
        for p in Platform::ALL {
            assert_eq!(parse_platform(p.as_str()).unwrap(), p);
        }
    }

    #[test]
    fn rejects_unknown_platforms() {
        for unknown in ["freebsd-amd64", "android-arm64", "linux-mips", "darwin-386", "darwin-x86", "darwin-arm", ""] {
            assert!(matches!(parse_platform(unknown), Err(Error::UnsupportedPlatform(_))), "{unknown}");
        }
    }

    #[test]
    fn current_platform_never_panics() {
        let _ = Platform::current();
    }

    #[test]
    fn normalizes_arch_aliases() {
        assert_eq!(normalize_arch("x86"), Some("x86"));
        assert_eq!(normalize_arch("386"), Some("x86"));
        assert_eq!(normalize_arch("i686"), Some("x86"));
        assert_eq!(normalize_arch("x86_64"), Some("x86_64"));
        assert_eq!(normalize_arch("amd64"), Some("x86_64"));
        assert_eq!(normalize_arch("x64"), Some("x86_64"));
        assert_eq!(normalize_arch("arm"), Some("arm"));
        assert_eq!(normalize_arch("armv7"), Some("arm"));
        assert_eq!(normalize_arch("aarch64"), Some("aarch64"));
        assert_eq!(normalize_arch("arm64"), Some("aarch64"));
        assert_eq!(normalize_arch("mips"), None);
        assert_eq!(normalize_arch("riscv64"), None);
        assert_eq!(normalize_arch(""), None);
    }

    #[test]
    fn displays_canonical_names() {
        assert_eq!(Platform::DarwinAarch64.to_string(), "darwin-aarch64");
        assert_eq!(Platform::WindowsX86.to_string(), "windows-x86");
        assert_eq!(Platform::LinuxArm.to_string(), "linux-arm");
    }

    #[test]
    fn current_uses_normalized_arch_directly() {
        // Every normalized arch word has a platform (macOS lacks 32-bit).
        for arch in ["x86", "x86_64", "arm", "aarch64"] {
            let id = format!("linux-{arch}");
            assert!(parse_platform(&id).is_ok(), "{id}");
            assert!(parse_platform(&format!("windows-{arch}")).is_ok());
        }
        assert!(parse_platform("darwin-x86").is_err());
        assert!(parse_platform("darwin-arm").is_err());
        assert!(parse_platform("darwin-x86_64").is_ok());
        assert!(parse_platform("darwin-aarch64").is_ok());
    }
}
