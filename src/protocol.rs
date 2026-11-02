use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    io::{Read, Write},
    net::{IpAddr, SocketAddr},
};

pub const VERSION: u16 = 3;
pub const MAX_PREVIEW: usize = 256 * 1024;
pub const PREVIEW_WIDTH: u32 = 480;
pub const PREVIEW_HEIGHT: u32 = 270;
pub const MAX_JSON: usize = 16 * 1024;
pub const MAX_FRAME: usize = 8 * 1024 * 1024;
pub const MAX_EVENTS: usize = 64;
pub const SERVER_NAME: &str = "localnet.local";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteError {
    pub code: String,
    pub message: String,
}

impl RemoteError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SessionTicket {
    pub version: u16,
    pub address: SocketAddr,
    pub certificate: Vec<u8>,
    pub token: String,
}

impl SessionTicket {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "Update Localnet on both computers (protocol mismatch)"
        );
        ensure!(
            is_local(self.address.ip()) && self.address.port() != 0,
            "Only private LAN or loopback addresses are allowed"
        );
        ensure!(
            !self.certificate.is_empty() && self.certificate.len() <= 4096,
            "Invalid host certificate"
        );
        ensure!(
            self.token.len() == 64 && self.token.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid session ticket"
        );
        Ok(())
    }
}

pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private() || ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local(),
    }
}

pub fn random_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("Random source failed: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Serialize, Deserialize)]
pub struct Hello {
    pub version: u16,
    pub token: String,
    pub channel: Channel,
}

#[derive(Serialize, Deserialize)]
pub enum Channel {
    Control,
    Video { session: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub name: String,
    pub mac: bool,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl DisplayInfo {
    pub fn point(&self, x: f32, y: f32) -> Result<(i32, i32)> {
        ensure!(x.is_finite() && y.is_finite(), "Invalid pointer coordinate");
        ensure!(self.width > 0 && self.height > 0, "Display has zero size");
        let x = i64::from(self.x) + (x.clamp(0.0, 1.0) * (self.width - 1) as f32).round() as i64;
        let y = i64::from(self.y) + (y.clamp(0.0, 1.0) * (self.height - 1) as f32).round() as i64;
        Ok((i32::try_from(x)?, i32::try_from(y)?))
    }
}

#[derive(Serialize, Deserialize)]
pub struct Welcome {
    pub session: String,
    pub display: DisplayInfo,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modifiers {
    pub primary: bool,
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Input {
    Move { x: f32, y: f32 },
    Button { button: Button, pressed: bool },
    Scroll { x: i32, y: i32 },
    Modifiers(Modifiers),
    Key { name: String, pressed: bool },
    Text(String),
    Shortcut { letter: char, shift: bool },
    ReleaseAll,
}

pub fn write_packet(writer: &mut impl Write, bytes: &[u8], max: usize) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= max,
        "Packet exceeds size limit"
    );
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn read_packet(reader: &mut impl Read, max: usize) -> Result<Vec<u8>> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > max {
        bail!("Invalid packet length: {length}");
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub fn write_json<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<()> {
    write_packet(writer, &serde_json::to_vec(value)?, MAX_JSON)
}

pub fn read_json<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T> {
    Ok(serde_json::from_slice(&read_packet(reader, MAX_JSON)?)?)
}

pub fn write_frame(writer: &mut impl Write, bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() < MAX_FRAME,
        "Frame exceeds size limit"
    );
    writer.write_all(&((bytes.len() + 1) as u32).to_be_bytes())?;
    writer.write_all(&[0])?;
    writer.write_all(bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn write_frame_error(writer: &mut impl Write, error: &str) -> Result<()> {
    let mut bytes = vec![1];
    bytes.extend_from_slice(error.as_bytes());
    write_packet(writer, &bytes, MAX_JSON)
}

pub fn read_frame(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut packet = read_packet(reader, MAX_FRAME)?;
    match packet.remove(0) {
        0 => {
            ensure!(!packet.is_empty(), "Empty frame");
            Ok(packet)
        }
        1 => {
            ensure!(packet.len() < MAX_JSON, "Invalid error packet");
            bail!("Remote video: {}", std::str::from_utf8(&packet)?);
        }
        _ => bail!("Unknown video packet. Update Localnet on both computers."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn framing_roundtrip_and_limits() {
        let mut bytes = Vec::new();
        write_json(&mut bytes, &vec![Input::ReleaseAll]).unwrap();
        let decoded: Vec<Input> = read_json(&mut Cursor::new(bytes)).unwrap();
        assert!(matches!(decoded[0], Input::ReleaseAll));
        assert!(read_packet(&mut Cursor::new(u32::MAX.to_be_bytes()), MAX_JSON).is_err());
        assert!(read_packet(&mut Cursor::new([0, 0, 0, 2, 1]), MAX_JSON).is_err());
        assert!(write_packet(&mut Vec::new(), &[], MAX_JSON).is_err());
    }

    #[test]
    fn tickets_reject_public_routes_and_wrong_versions() {
        let mut pair = SessionTicket {
            version: VERSION,
            address: "192.168.1.10:45777".parse().unwrap(),
            certificate: vec![1],
            token: "a".repeat(64),
        };
        assert!(pair.validate().is_ok());
        pair.address = "8.8.8.8:45777".parse().unwrap();
        assert!(pair.validate().is_err());
        pair.address = "127.0.0.1:45777".parse().unwrap();
        pair.version = 1;
        assert!(pair.validate().is_err());
    }

    #[test]
    fn frames_carry_remote_errors_without_exposing_decoder_to_json() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, b"jpeg").unwrap();
        assert_eq!(read_frame(&mut Cursor::new(bytes)).unwrap(), b"jpeg");
        let mut bytes = Vec::new();
        write_frame_error(&mut bytes, "Screen Recording permission required").unwrap();
        assert!(
            read_frame(&mut Cursor::new(bytes))
                .unwrap_err()
                .to_string()
                .contains("Screen Recording")
        );
        assert!(read_frame(&mut Cursor::new([0, 0, 0, 1, 0])).is_err());
    }

    #[test]
    fn retina_coordinates_use_display_bounds_not_frame_pixels() {
        let info = DisplayInfo {
            name: "Retina".into(),
            mac: true,
            x: -100,
            y: 20,
            width: 1440,
            height: 900,
        };
        assert_eq!(info.point(0.0, 0.0).unwrap(), (-100, 20));
        assert_eq!(info.point(1.0, 1.0).unwrap(), (1339, 919));
        assert_eq!(info.point(-1.0, 2.0).unwrap(), (-100, 919));
        assert!(info.point(f32::NAN, 0.0).is_err());
    }
}
