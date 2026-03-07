/// Extract the SNI (Server Name Indication) hostname from a TLS ClientHello message.
///
/// Returns `None` if the buffer is not a valid TLS ClientHello or contains no SNI extension.
/// This only reads plaintext handshake bytes — no TLS decryption is involved.
pub fn extract_sni(buf: &[u8]) -> Option<String> {
    // TLS record header: ContentType(1) + Version(2) + Length(2)
    if buf.len() < 5 {
        return None;
    }
    // ContentType 22 = Handshake
    if buf[0] != 22 {
        return None;
    }
    let record_len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    let record_end = 5 + record_len;
    if buf.len() < record_end {
        return None;
    }

    let hs = &buf[5..record_end];

    // Handshake header: HandshakeType(1) + Length(3)
    if hs.len() < 4 {
        return None;
    }
    // HandshakeType 1 = ClientHello
    if hs[0] != 1 {
        return None;
    }

    let hs_len = u24(hs, 1)?;
    let ch = &hs[4..4 + hs_len.min(hs.len() - 4)];

    // ClientHello: Version(2) + Random(32) = 34 bytes minimum
    if ch.len() < 34 {
        return None;
    }
    let mut pos = 34;

    // Session ID: length(1) + data
    if pos >= ch.len() {
        return None;
    }
    let session_id_len = ch[pos] as usize;
    pos += 1 + session_id_len;

    // Cipher suites: length(2) + data
    if pos + 2 > ch.len() {
        return None;
    }
    let cipher_suites_len = u16::from_be_bytes([ch[pos], ch[pos + 1]]) as usize;
    pos += 2 + cipher_suites_len;

    // Compression methods: length(1) + data
    if pos >= ch.len() {
        return None;
    }
    let comp_len = ch[pos] as usize;
    pos += 1 + comp_len;

    // Extensions: length(2) + data
    if pos + 2 > ch.len() {
        return None;
    }
    let extensions_len = u16::from_be_bytes([ch[pos], ch[pos + 1]]) as usize;
    pos += 2;
    let extensions_end = pos + extensions_len.min(ch.len() - pos);

    // Walk extensions looking for SNI (type 0x0000)
    while pos + 4 <= extensions_end {
        let ext_type = u16::from_be_bytes([ch[pos], ch[pos + 1]]);
        let ext_len = u16::from_be_bytes([ch[pos + 2], ch[pos + 3]]) as usize;
        pos += 4;

        if ext_type == 0x0000 {
            // SNI extension: ServerNameList length(2), then entries
            if ext_len < 2 || pos + ext_len > extensions_end {
                return None;
            }
            let mut sni_pos = pos + 2; // skip list length
            let sni_end = pos + ext_len;
            while sni_pos + 3 <= sni_end {
                let name_type = ch[sni_pos];
                let name_len = u16::from_be_bytes([ch[sni_pos + 1], ch[sni_pos + 2]]) as usize;
                sni_pos += 3;
                if name_type == 0 && sni_pos + name_len <= sni_end {
                    return String::from_utf8(ch[sni_pos..sni_pos + name_len].to_vec()).ok();
                }
                sni_pos += name_len;
            }
            return None;
        }

        pos += ext_len;
    }

    None
}

fn u24(buf: &[u8], offset: usize) -> Option<usize> {
    if offset + 3 > buf.len() {
        return None;
    }
    Some(
        ((buf[offset] as usize) << 16)
            | ((buf[offset + 1] as usize) << 8)
            | buf[offset + 2] as usize,
    )
}

/// Check whether the SNI hostname matches the CONNECT target domain.
/// Exact match (case-insensitive). The CONNECT domain is what the client asked to tunnel to,
/// so the SNI should be the same domain.
pub fn sni_matches_connect_domain(sni: &str, connect_domain: &str) -> bool {
    sni.eq_ignore_ascii_case(connect_domain)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a minimal TLS 1.2 ClientHello with the given SNI hostname.
    pub fn build_client_hello(sni_hostname: &str) -> Vec<u8> {
        // SNI extension value
        let name_bytes = sni_hostname.as_bytes();
        let sni_entry_len = 3 + name_bytes.len(); // type(1) + len(2) + name
        let sni_list_len = sni_entry_len;
        let sni_ext_value_len = 2 + sni_list_len; // list_len(2) + entries

        let mut sni_ext = Vec::new();
        sni_ext.extend_from_slice(&0u16.to_be_bytes()); // extension type: SNI
        sni_ext.extend_from_slice(&(sni_ext_value_len as u16).to_be_bytes()); // extension data length
        sni_ext.extend_from_slice(&(sni_list_len as u16).to_be_bytes()); // server name list length
        sni_ext.push(0); // name type: host_name
        sni_ext.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
        sni_ext.extend_from_slice(name_bytes);

        let extensions_len = sni_ext.len();

        // ClientHello body
        let mut ch = Vec::new();
        ch.extend_from_slice(&[0x03, 0x03]); // Version: TLS 1.2
        ch.extend_from_slice(&[0u8; 32]); // Random
        ch.push(0); // Session ID length: 0
        ch.extend_from_slice(&2u16.to_be_bytes()); // Cipher suites length: 2
        ch.extend_from_slice(&[0x00, 0xff]); // One cipher suite
        ch.push(1); // Compression methods length: 1
        ch.push(0); // null compression
        ch.extend_from_slice(&(extensions_len as u16).to_be_bytes());
        ch.extend_from_slice(&sni_ext);

        let ch_len = ch.len();

        // Handshake header
        let mut hs = Vec::new();
        hs.push(1); // HandshakeType: ClientHello
        hs.push((ch_len >> 16) as u8);
        hs.push((ch_len >> 8) as u8);
        hs.push(ch_len as u8);
        hs.extend_from_slice(&ch);

        let hs_len = hs.len();

        // TLS record header
        let mut record = Vec::new();
        record.push(22); // ContentType: Handshake
        record.extend_from_slice(&[0x03, 0x01]); // Version: TLS 1.0 (record layer)
        record.extend_from_slice(&(hs_len as u16).to_be_bytes());
        record.extend_from_slice(&hs);

        record
    }

    #[test]
    fn extracts_sni_from_client_hello() {
        let hello = build_client_hello("example.com");
        assert_eq!(extract_sni(&hello), Some("example.com".to_string()));
    }

    #[test]
    fn extracts_long_sni() {
        let hello = build_client_hello("subdomain.deep.example.co.uk");
        assert_eq!(
            extract_sni(&hello),
            Some("subdomain.deep.example.co.uk".to_string())
        );
    }

    #[test]
    fn returns_none_for_empty_buffer() {
        assert_eq!(extract_sni(&[]), None);
    }

    #[test]
    fn returns_none_for_non_tls() {
        assert_eq!(extract_sni(b"GET / HTTP/1.1\r\n"), None);
    }

    #[test]
    fn returns_none_for_truncated_record() {
        let hello = build_client_hello("example.com");
        assert_eq!(extract_sni(&hello[..10]), None);
    }

    /// Build a ClientHello with no extensions at all.
    fn build_client_hello_no_extensions() -> Vec<u8> {
        let mut ch = Vec::new();
        ch.extend_from_slice(&[0x03, 0x03]); // TLS 1.2
        ch.extend_from_slice(&[0u8; 32]); // Random
        ch.push(0); // Session ID length: 0
        ch.extend_from_slice(&2u16.to_be_bytes()); // Cipher suites length: 2
        ch.extend_from_slice(&[0x00, 0xff]); // One cipher suite
        ch.push(1); // Compression methods length: 1
        ch.push(0); // null compression
        // No extensions field at all

        let ch_len = ch.len();
        let mut hs = Vec::new();
        hs.push(1); // ClientHello
        hs.push((ch_len >> 16) as u8);
        hs.push((ch_len >> 8) as u8);
        hs.push(ch_len as u8);
        hs.extend_from_slice(&ch);

        let hs_len = hs.len();
        let mut record = Vec::new();
        record.push(22);
        record.extend_from_slice(&[0x03, 0x01]);
        record.extend_from_slice(&(hs_len as u16).to_be_bytes());
        record.extend_from_slice(&hs);
        record
    }

    /// Build a ClientHello where SNI is the second extension (after a dummy one).
    fn build_client_hello_sni_not_first(sni_hostname: &str) -> Vec<u8> {
        let name_bytes = sni_hostname.as_bytes();
        let sni_entry_len = 3 + name_bytes.len();
        let sni_list_len = sni_entry_len;
        let sni_ext_value_len = 2 + sni_list_len;

        // Dummy extension (type 0x0017 = extended_master_secret, empty value)
        let mut dummy_ext = Vec::new();
        dummy_ext.extend_from_slice(&0x0017u16.to_be_bytes());
        dummy_ext.extend_from_slice(&0u16.to_be_bytes()); // zero-length value

        // SNI extension
        let mut sni_ext = Vec::new();
        sni_ext.extend_from_slice(&0u16.to_be_bytes());
        sni_ext.extend_from_slice(&(sni_ext_value_len as u16).to_be_bytes());
        sni_ext.extend_from_slice(&(sni_list_len as u16).to_be_bytes());
        sni_ext.push(0);
        sni_ext.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
        sni_ext.extend_from_slice(name_bytes);

        let extensions_len = dummy_ext.len() + sni_ext.len();

        let mut ch = Vec::new();
        ch.extend_from_slice(&[0x03, 0x03]);
        ch.extend_from_slice(&[0u8; 32]);
        ch.push(0);
        ch.extend_from_slice(&2u16.to_be_bytes());
        ch.extend_from_slice(&[0x00, 0xff]);
        ch.push(1);
        ch.push(0);
        ch.extend_from_slice(&(extensions_len as u16).to_be_bytes());
        ch.extend_from_slice(&dummy_ext);
        ch.extend_from_slice(&sni_ext);

        let ch_len = ch.len();
        let mut hs = Vec::new();
        hs.push(1);
        hs.push((ch_len >> 16) as u8);
        hs.push((ch_len >> 8) as u8);
        hs.push(ch_len as u8);
        hs.extend_from_slice(&ch);

        let hs_len = hs.len();
        let mut record = Vec::new();
        record.push(22);
        record.extend_from_slice(&[0x03, 0x01]);
        record.extend_from_slice(&(hs_len as u16).to_be_bytes());
        record.extend_from_slice(&hs);
        record
    }

    #[test]
    fn returns_none_for_no_extensions() {
        assert_eq!(extract_sni(&build_client_hello_no_extensions()), None);
    }

    #[test]
    fn finds_sni_when_not_first_extension() {
        let hello = build_client_hello_sni_not_first("later.example.com");
        assert_eq!(extract_sni(&hello), Some("later.example.com".to_string()));
    }

    #[test]
    fn returns_none_for_server_hello() {
        // Build a valid record but with HandshakeType 2 (ServerHello) instead of 1
        let mut hello = build_client_hello("example.com");
        hello[5] = 2; // change handshake type from ClientHello to ServerHello
        assert_eq!(extract_sni(&hello), None);
    }

    #[test]
    fn returns_none_for_non_handshake_record() {
        // ContentType 23 = ApplicationData, not Handshake
        let mut hello = build_client_hello("example.com");
        hello[0] = 23;
        assert_eq!(extract_sni(&hello), None);
    }

    #[test]
    fn handles_tls_13_record_version() {
        // TLS 1.3 records use 0x0303 at the record layer; parser should still work
        let mut hello = build_client_hello("tls13.example.com");
        hello[1] = 0x03;
        hello[2] = 0x03; // record version TLS 1.2 (used by TLS 1.3 records)
        assert_eq!(extract_sni(&hello), Some("tls13.example.com".to_string()));
    }

    #[test]
    fn sni_match_exact() {
        assert!(sni_matches_connect_domain("example.com", "example.com"));
    }

    #[test]
    fn sni_match_case_insensitive() {
        assert!(sni_matches_connect_domain("Example.COM", "example.com"));
    }

    #[test]
    fn sni_mismatch() {
        assert!(!sni_matches_connect_domain("evil.com", "example.com"));
    }

    #[test]
    fn sni_subdomain_does_not_match() {
        assert!(!sni_matches_connect_domain(
            "sub.example.com",
            "example.com"
        ));
    }
}
