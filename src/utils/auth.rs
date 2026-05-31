/// Generates a deterministic offline UUID from a username.
///
/// Uses a FNV-1a–inspired mix to produce a UUID v3-style string that is
/// stable across calls for the same username. Suitable for offline-mode
/// Minecraft sessions where a real Microsoft account is not available.
pub fn offline_uuid(username: &str) -> String {
    let mut a = 0xcbf29ce484222325u64;
    let mut b = 0x14650fb0739d0383u64;
    for byte in username.bytes() {
        a ^= byte as u64;
        a = a.wrapping_mul(0x100000001b3);
        b ^= a;
        b = b.wrapping_mul(0x517cc1b727220a95);
    }
    format!(
        "{:08x}-{:04x}-3{:03x}-{:04x}-{:012x}",
        (b >> 32) as u32,
        (b >> 16) as u16,
        b as u16 & 0x0fff,
        ((a >> 48) as u16 & 0x3fff) | 0x8000,
        a & 0x0000_ffff_ffff_ffff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_username_same_uuid() {
        assert_eq!(offline_uuid("Steve"), offline_uuid("Steve"));
    }

    #[test]
    fn different_usernames_different_uuids() {
        assert_ne!(offline_uuid("Steve"), offline_uuid("Alex"));
    }

    #[test]
    fn output_is_valid_uuid_format() {
        let u = offline_uuid("Player");
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
    }
}
