use crate::{relays, store::Store};
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use qrcode::{Color, QrCode};
use serde_json::{json, Value};

pub fn identity(s: &Store) -> Result<Value> {
    let public_key = s.identity()?.public.clone();
    let relays = vec![relays::selected(s, false)?];
    // Keep the field order and standard padded Base64 identical to
    // MoxChat/composables/tools/qrcode.ts::buildMoxpubPayload.
    let payload = json!({"pub": public_key, "relays": relays});
    let link = format!("moxpub:{}", STANDARD.encode(serde_json::to_vec(&payload)?));
    Ok(json!({"pub": public_key, "relays": relays, "moxpub": link}))
}

pub struct TerminalQr {
    pub text: String,
    pub columns: usize,
}

pub fn qr(link: &str) -> Result<TerminalQr> {
    let code =
        QrCode::new(link.as_bytes()).context("QR_TOO_LARGE: 身份链接过长，无法生成二维码")?;
    let side = code.width();
    // Four modules of quiet zone on every side. Two vertical modules per
    // terminal cell preserve a square QR in a normal monospace terminal.
    let border = 4;
    let columns = side + border * 2;
    let dark = |x: usize, y: usize| {
        x >= border
            && y >= border
            && x < side + border
            && y < side + border
            && code[(x - border, y - border)] == Color::Dark
    };
    let mut text = String::new();
    for y in (0..columns).step_by(2) {
        for x in 0..columns {
            text.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        text.push('\n');
    }
    Ok(TerminalQr { text, columns })
}
