#[link(name = "kernel32")]
extern "system" {
    fn MultiByteToWideChar(
        CodePage: u32,
        dwFlags: u32,
        lpMultiByteStr: *const u8,
        cbMultiByte: i32,
        lpWideCharStr: *mut u16,
        cchWideChar: i32,
    ) -> i32;
}

const CP949: u32 = 949;

/// Decode raw entry name bytes.
/// - If valid UTF-8, returns the UTF-8 string directly.
/// - If not valid UTF-8, automatically attempts Windows CP949 (EUC-KR) decoding.
/// - Falls back to lossy UTF-8 if CP949 conversion fails.
pub fn decode_entry_name(raw_bytes: &[u8]) -> String {
    // 1. Try UTF-8 first (Modern standard)
    if let Ok(utf8_str) = std::str::from_utf8(raw_bytes) {
        return utf8_str.to_string();
    }

    // 2. Fallback to Windows CP949 decoding
    if !raw_bytes.is_empty() {
        unsafe {
            let required_len = MultiByteToWideChar(
                CP949,
                0,
                raw_bytes.as_ptr(),
                raw_bytes.len() as i32,
                std::ptr::null_mut(),
                0,
            );

            if required_len > 0 {
                let mut wide_buf = vec![0u16; required_len as usize];
                let converted = MultiByteToWideChar(
                    CP949,
                    0,
                    raw_bytes.as_ptr(),
                    raw_bytes.len() as i32,
                    wide_buf.as_mut_ptr(),
                    required_len,
                );

                if converted > 0 {
                    return String::from_utf16_lossy(&wide_buf);
                }
            }
        }
    }

    // 3. Last resort fallback
    String::from_utf8_lossy(raw_bytes).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_utf8_passthrough() {
        let utf8_bytes = "안녕하세요.txt".as_bytes();
        assert_eq!(decode_entry_name(utf8_bytes), "안녕하세요.txt");
    }

    #[test]
    fn test_cp949_decoding() {
        // "홍길동.txt" in CP949 bytes:
        // 홍: 0xC8 0xAB, 길: 0xB1 0xE6, 동: 0xB5 0xBF, .: 0x2E, t: 0x74, x: 0x78, t: 0x74
        let cp949_bytes = &[0xC8, 0xAB, 0xB1, 0xE6, 0xB5, 0xBF, 0x2E, 0x74, 0x78, 0x74];
        assert_eq!(decode_entry_name(cp949_bytes), "홍길동.txt");
    }
}
