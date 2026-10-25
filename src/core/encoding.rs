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

const CP_ACP: u32 = 0;
const CP949: u32 = 949;
const MB_ERR_INVALID_CHARS: u32 = 0x00000008;

// Unicode Standard Annex #15 Hangul Composition Constants
const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT; // 588

/// Algorithmic Hangul NFD (Decomposed Jamo) to NFC (Precomposed Syllables) composition.
/// Solves the classic macOS "자모 분리" issue on Windows Explorer with zero external dependencies.
pub fn compose_hangul_nfc(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        let code = ch as u32;
        // Check if ch is Hangul Choseong (Leading consonant: U+1100 ..= U+1112)
        if (L_BASE..L_BASE + L_COUNT).contains(&code) {
            let l_index = code - L_BASE;
            // Check if next char is Jungseong (Vowel: U+1161 ..= U+1175)
            if let Some(&next_ch) = chars.peek() {
                let next_code = next_ch as u32;
                if (V_BASE..V_BASE + V_COUNT).contains(&next_code) {
                    chars.next(); // consume vowel
                    let v_index = next_code - V_BASE;
                    let mut lv_index = l_index * N_COUNT + v_index * T_COUNT;

                    // Check if following char is Jongseong (Trailing consonant: U+11A8 ..= U+11C2)
                    if let Some(&third_ch) = chars.peek() {
                        let third_code = third_ch as u32;
                        if (T_BASE + 1..T_BASE + T_COUNT).contains(&third_code) {
                            chars.next(); // consume trailing consonant
                            let t_index = third_code - T_BASE;
                            lv_index += t_index;
                        }
                    }

                    if let Some(syllable) = char::from_u32(S_BASE + lv_index) {
                        result.push(syllable);
                        continue;
                    }
                }
            }
        }
        result.push(ch);
    }
    result
}

/// Attempt to decode raw bytes with a specific Windows code page using strict validation.
unsafe fn try_decode_codepage(codepage: u32, raw_bytes: &[u8]) -> Option<String> {
    let required_len = MultiByteToWideChar(
        codepage,
        MB_ERR_INVALID_CHARS,
        raw_bytes.as_ptr(),
        raw_bytes.len() as i32,
        std::ptr::null_mut(),
        0,
    );

    if required_len > 0 {
        let mut wide_buf = vec![0u16; required_len as usize];
        let converted = MultiByteToWideChar(
            codepage,
            MB_ERR_INVALID_CHARS,
            raw_bytes.as_ptr(),
            raw_bytes.len() as i32,
            wide_buf.as_mut_ptr(),
            required_len,
        );

        if converted > 0 {
            return Some(String::from_utf16_lossy(&wide_buf));
        }
    }
    None
}

/// Decode raw entry name bytes.
/// 1. UTF-8 first (Modern standard, with Hangul NFD->NFC composition).
/// 2. System ANSI code page (CP_ACP: CP949 on Korean Windows, CP932 on Japanese, CP1252 on US/EU).
/// 3. CP949 fallback (in case a non-Korean machine opens a Korean ALZip archive).
/// 4. Lossy UTF-8 fallback.
pub fn decode_entry_name(raw_bytes: &[u8]) -> String {
    compose_hangul_nfc(&decode_entry_name_raw(raw_bytes))
}

/// Decode the filename without normalization so exact member lookup can precede aliases.
pub fn decode_entry_name_raw(raw_bytes: &[u8]) -> String {
    // 1. Try UTF-8 first (Modern standard)
    if let Ok(utf8_str) = std::str::from_utf8(raw_bytes) {
        return utf8_str.to_string();
    }

    if !raw_bytes.is_empty() {
        unsafe {
            // 2. Try the current Windows system ANSI code page
            if let Some(decoded) = try_decode_codepage(CP_ACP, raw_bytes) {
                return decoded;
            }

            // 3. Try Korean CP949 (if system ANSI code page was not 949)
            if let Some(decoded) = try_decode_codepage(CP949, raw_bytes) {
                return decoded;
            }
        }
    }

    // 4. Last resort fallback
    String::from_utf8_lossy(raw_bytes).into_owned()
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

    #[test]
    fn test_macos_hangul_nfd_to_nfc() {
        // macOS NFD decomposed "한글":
        // ㅎ (U+1112) + ㅏ (U+1161) + ㄴ (U+11AB) = 한 (U+D55C)
        // ㄱ (U+1100) + ㅡ (U+1173) + ㄹ (U+11AF) = 글 (U+AE00)
        let nfd_hangul = "\u{1112}\u{1161}\u{11AB}\u{1100}\u{1173}\u{11AF}.txt";
        let composed = compose_hangul_nfc(nfd_hangul);
        assert_eq!(composed, "한글.txt");

        // Verify full decode_entry_name pipeline
        assert_eq!(decode_entry_name(nfd_hangul.as_bytes()), "한글.txt");
    }

    #[test]
    fn test_hangul_nfd_vowel_only_syllable() {
        // "가나" without trailing consonants:
        // ㄱ (U+1100) + ㅏ (U+1161) = 가 (U+AC00)
        // ㄴ (U+1102) + ㅏ (U+1161) = 나 (U+B098)
        let nfd_gana = "\u{1100}\u{1161}\u{1102}\u{1161}.docx";
        assert_eq!(compose_hangul_nfc(nfd_gana), "가나.docx");
    }
}
