#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub arch: String,
}

impl Platform {
    pub fn new(os: &str, arch: &str) -> Self {
        Self {
            os: os.to_string(),
            arch: arch.to_string(),
        }
    }

    pub fn current() -> Self {
        Self {
            os: map_os(std::env::consts::OS).to_string(),
            arch: map_arch(std::env::consts::ARCH).to_string(),
        }
    }
}

pub fn map_os(os: &str) -> &str {
    os
}

pub fn map_arch(arch: &str) -> &str {
    match arch {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        "arm" | "armv7" => "arm",
        "x86" | "i686" => "x86",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_os() {
        assert_eq!(map_os("macos"), "macos");
        assert_eq!(map_os("windows"), "windows");
        assert_eq!(map_os("linux"), "linux");
    }

    #[test]
    fn maps_known_arch() {
        assert_eq!(map_arch("aarch64"), "aarch64");
        assert_eq!(map_arch("x86_64"), "x86_64");
        assert_eq!(map_arch("arm"), "arm");
        assert_eq!(map_arch("armv7"), "arm");
        assert_eq!(map_arch("x86"), "x86");
        assert_eq!(map_arch("i686"), "x86");
    }

    #[test]
    fn current_uses_mapped_keys() {
        let p = Platform::current();
        assert!(!p.os.is_empty());
        assert!(!p.arch.is_empty());
    }
}
