use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct Smpp { host: String, port: u16, user: String, password: String, targets: Vec<String>, from: String, secure: bool, tags: Vec<String> }

impl Smpp {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone().unwrap_or_else(|| "localhost".to_string());
        let user = url.get("user").map(|s| s.to_string()).or_else(|| url.user.clone())?;
        let password = url.get("password").map(|s| s.to_string()).or_else(|| url.password.clone())?;
        let port = url.port.unwrap_or(2775);
        let from = url.get("from").or_else(|| url.get("source")).unwrap_or("Apprise").to_string();
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        if targets.is_empty() { return None; }
        let secure = url.schema == "smpps";
        Some(Self { host, port, user, password, targets, from, secure, tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "SMPP", service_url: None, setup_url: None, protocols: vec!["smpp", "smpps"], description: "Send SMS via SMPP protocol.", attachment_support: false } }
}

fn cstring(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

fn make_pdu(command_id: u32, sequence: u32, body: &[u8]) -> Vec<u8> {
    let len = (16 + body.len()) as u32;
    let mut pdu = Vec::with_capacity(len as usize);
    pdu.extend_from_slice(&len.to_be_bytes());
    pdu.extend_from_slice(&command_id.to_be_bytes());
    pdu.extend_from_slice(&0u32.to_be_bytes()); // command_status = ESME_ROK
    pdu.extend_from_slice(&sequence.to_be_bytes());
    pdu.extend_from_slice(body);
    pdu
}

async fn read_pdu_status(stream: &mut TcpStream) -> Result<(u32, u32), NotifyError> {
    let mut header = [0u8; 16];
    stream.read_exact(&mut header).await.map_err(|e| NotifyError::Other(e.to_string()))?;
    let total_len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let command_id = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let command_status = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    // Drain remaining body bytes
    let body_len = total_len.saturating_sub(16);
    if body_len > 0 {
        let mut body = vec![0u8; body_len];
        stream.read_exact(&mut body).await.map_err(|e| NotifyError::Other(e.to_string()))?;
    }
    Ok((command_id, command_status))
}

fn encode_message(text: &str) -> (u8, Vec<u8>) {
    // Use GSM7 default (data_coding=0) for ASCII, UCS2 (data_coding=8) for Unicode
    if text.is_ascii() {
        let bytes: Vec<u8> = text.bytes().take(160).collect();
        (0x00, bytes)
    } else {
        // UTF-16 BE (UCS2), max 70 chars
        let bytes: Vec<u8> = text.chars().take(70)
            .flat_map(|c| {
                let mut buf = [0u16; 2];
                let encoded = c.encode_utf16(&mut buf);
                encoded.iter().flat_map(|&w| w.to_be_bytes()).collect::<Vec<u8>>()
            })
            .collect();
        (0x08, bytes)
    }
}

#[async_trait]
impl Notify for Smpp {
    fn schemas(&self) -> &[&str] { &["smpp", "smpps"] }
    fn service_name(&self) -> &str { "SMPP" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);

        let addr = format!("{}:{}", self.host, self.port);
        let mut stream = TcpStream::connect(&addr).await
            .map_err(|e| NotifyError::Other(format!("SMPP connect to {} failed: {}", addr, e)))?;

        // bind_transmitter (command_id = 0x00000002)
        let mut bind_body = Vec::new();
        bind_body.extend_from_slice(&cstring(&self.user));
        bind_body.extend_from_slice(&cstring(&self.password));
        bind_body.extend_from_slice(&cstring(""));   // system_type
        bind_body.push(0x34); // interface_version = SMPP 3.4
        bind_body.push(0x00); // addr_ton
        bind_body.push(0x00); // addr_npi
        bind_body.extend_from_slice(&cstring("")); // address_range

        stream.write_all(&make_pdu(0x00000002, 1, &bind_body)).await
            .map_err(|e| NotifyError::Other(e.to_string()))?;

        let (resp_id, status) = read_pdu_status(&mut stream).await?;
        if resp_id != 0x80000002 || status != 0 {
            return Err(NotifyError::Auth(format!("SMPP bind failed (cmd={:#x} status={:#x})", resp_id, status)));
        }

        let (data_coding, msg_bytes) = encode_message(&msg);
        let mut all_ok = true;
        let mut seq: u32 = 2;

        for target in &self.targets {
            let mut body = Vec::new();
            body.extend_from_slice(&cstring(""));      // service_type
            body.push(0x01); // source_addr_ton = INTERNATIONAL
            body.push(0x01); // source_addr_npi = ISDN
            body.extend_from_slice(&cstring(&self.from));
            body.push(0x01); // dest_addr_ton
            body.push(0x01); // dest_addr_npi
            body.extend_from_slice(&cstring(target));
            body.push(0x00); // esm_class
            body.push(0x00); // protocol_id
            body.push(0x00); // priority_flag
            body.extend_from_slice(&cstring("")); // schedule_delivery_time
            body.extend_from_slice(&cstring("")); // validity_period
            body.push(0x00); // registered_delivery
            body.push(0x00); // replace_if_present_flag
            body.push(data_coding);
            body.push(0x00); // sm_default_msg_id
            body.push(msg_bytes.len() as u8);
            body.extend_from_slice(&msg_bytes);

            stream.write_all(&make_pdu(0x00000004, seq, &body)).await
                .map_err(|e| NotifyError::Other(e.to_string()))?;
            let (_, status) = read_pdu_status(&mut stream).await?;
            if status != 0 { all_ok = false; }
            seq += 1;
        }

        // unbind (command_id = 0x00000006)
        let _ = stream.write_all(&make_pdu(0x00000006, seq, &[])).await;

        Ok(all_ok)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;
    use crate::utils::parse::ParsedUrl;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "smpp://user:pass@host/+15551234567",
            "smpps://user:pass@host/+15551234567",
            "smpp://user:pass@host:2775/+15551234567",
            "smpp://user:pass@host/+15551234567/+15559876543",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "smpp://",
            "smpp:///",
            "smpp://@/",
            "smpp://user@/",
            "smpp://user:pass/",
            "smpp://user:pass@/",
            "smpp://user@hostname",
            "smpp://user:pass@host:/",
            "smpp://user:pass@host:2775/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_smpp_struct_fields() {
        let parsed = ParsedUrl::parse("smpp://myuser:mypass@smpphost:2775/+15551234567").unwrap();
        let smpp = Smpp::from_url(&parsed).unwrap();
        assert_eq!(smpp.host, "smpphost");
        assert_eq!(smpp.port, 2775);
        assert_eq!(smpp.user, "myuser");
        assert_eq!(smpp.password, "mypass");
        assert!(smpp.targets.contains(&"+15551234567".to_string()));
        assert!(!smpp.secure);
    }

    #[test]
    fn test_smpps_secure_flag() {
        let parsed = ParsedUrl::parse("smpps://user:pass@host/+15551234567").unwrap();
        let smpp = Smpp::from_url(&parsed).unwrap();
        assert!(smpp.secure);
    }

    #[test]
    fn test_smpp_default_port() {
        let parsed = ParsedUrl::parse("smpp://user:pass@host/+15551234567").unwrap();
        let smpp = Smpp::from_url(&parsed).unwrap();
        assert_eq!(smpp.port, 2775);
    }

    #[test]
    fn test_smpp_custom_from() {
        let parsed = ParsedUrl::parse("smpp://user:pass@host/+15551234567?from=MySender").unwrap();
        let smpp = Smpp::from_url(&parsed).unwrap();
        assert_eq!(smpp.from, "MySender");
    }

    #[test]
    fn test_smpp_default_from() {
        let parsed = ParsedUrl::parse("smpp://user:pass@host/+15551234567").unwrap();
        let smpp = Smpp::from_url(&parsed).unwrap();
        assert_eq!(smpp.from, "Apprise");
    }

    #[test]
    fn test_smpp_multiple_targets() {
        let parsed = ParsedUrl::parse("smpp://user:pass@host/+111/+222/+333").unwrap();
        let smpp = Smpp::from_url(&parsed).unwrap();
        assert_eq!(smpp.targets.len(), 3);
    }

    #[test]
    fn test_smpp_to_query_param() {
        let parsed = ParsedUrl::parse("smpp://user:pass@host/+111?to=+222,+333").unwrap();
        let smpp = Smpp::from_url(&parsed).unwrap();
        assert_eq!(smpp.targets.len(), 3);
    }

    #[test]
    fn test_smpp_static_details() {
        let details = Smpp::static_details();
        assert_eq!(details.service_name, "SMPP");
        assert_eq!(details.protocols, vec!["smpp", "smpps"]);
        assert!(!details.attachment_support);
    }
}
