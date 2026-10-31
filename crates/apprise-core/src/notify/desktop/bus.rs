//! The freedesktop notification service, which dbus://, glib:// and
//! gnome:// all reach over the D-Bus session bus.
//! See https://specifications.freedesktop.org/notification-spec/latest/

use std::borrow::Cow;

use crate::asset::NotifyImageSize;
use crate::error::NotifyError;
use crate::notify::NotifyContext;

const DESTINATION: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// How long dbus:// and glib:// notifications stay up.
const MESSAGE_TIMEOUT_MS: i32 = 13000;

/// libdbus's default reply timeout, which upstream's bindings use.
#[cfg(unix)]
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

/// One `Notify` call. The replaced id (0), icon name ("") and actions (none)
/// never change.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Notification {
  pub app_name: String,
  pub summary: String,
  pub body: String,
  pub hints: Vec<(&'static str, Hint)>,
  /// Milliseconds; -1 leaves it to the server.
  pub expire_timeout: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Hint {
  Byte(u8),
  Int(i32),
  Int64(i64),
  Image(Image),
}

/// Raw pixels, sent as the `(iiibiiay)` structure GdkPixbuf describes.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Image {
  pub width: i32,
  pub height: i32,
  pub rowstride: i32,
  pub has_alpha: bool,
  pub bits_per_sample: i32,
  pub channels: i32,
  pub data: Vec<u8>,
}

/// How dbus:// and glib:// build their call (upstream dbus.py and glib.py
/// send the same one).
pub(super) fn freedesktop_notification(ctx: &NotifyContext, urgency: u8, position: Option<(i32, i32)>, include_image: bool) -> Notification {
  // Without a title, the body becomes the summary to avoid an empty line
  let (summary, body) = if ctx.title.is_empty() { (ctx.body.clone(), String::new()) } else { (ctx.title.clone(), ctx.body.clone()) };
  let mut hints = vec![("urgency", Hint::Byte(urgency))];
  if let Some((x, y)) = position {
    hints.push(("x", Hint::Int(x)));
    hints.push(("y", Hint::Int(y)));
  }
  if include_image && let Some(image) = icon(ctx) {
    hints.push(("icon_data", Hint::Image(image)));
  }
  Notification { app_name: ctx.asset.app_id.clone(), summary, body, hints, expire_timeout: MESSAGE_TIMEOUT_MS }
}

/// The notification's icon as raw pixels. Upstream loads
/// `image_path(notify_type, extension=".ico")` at 128x128 with GdkPixbuf:
/// the file the asset's `image_path_mask` names, or its own default theme.
/// Without a mask the bundled default-theme icon stands in for the latter.
/// A failure is logged and the notification goes out without one, as
/// upstream does.
pub(super) fn icon(ctx: &NotifyContext) -> Option<Image> {
  let asset = &ctx.asset;
  let (data, source) = if asset.image_path_mask.as_deref().is_some_and(|m| !m.is_empty()) {
    let path = asset.image_path(&ctx.notify_type, NotifyImageSize::Xy128, true, Some(".ico"))?;
    match std::fs::read(&path) {
      Ok(data) => (Cow::Owned(data), path.display().to_string()),
      Err(e) => {
        tracing::warn!("Could not load notification icon ({}): {}", path.display(), e);
        return None;
      }
    }
  } else if asset.theme == "default" {
    (Cow::Borrowed(super::image_png(&ctx.notify_type)), ctx.notify_type.as_str().to_string())
  } else {
    return None;
  };
  decode_image(&data).map_err(|e| tracing::warn!("Could not load notification icon ({}): {}", source, e)).ok()
}

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Decode a PNG or ICO file (the formats Apprise ships icons in) to 8-bit
/// RGB or RGBA pixels.
fn decode_image(data: &[u8]) -> Result<Image, String> {
  if data.starts_with(PNG_SIGNATURE) {
    decode_png(data)
  } else if data.starts_with(&[0, 0, 1, 0]) {
    decode_ico(data)
  } else {
    Err("not a PNG or ICO image".into())
  }
}

fn decode_png(data: &[u8]) -> Result<Image, String> {
  let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
  // Palette, low bit depths and 16-bit samples become 8-bit samples
  decoder.set_transformations(png::Transformations::normalize_to_color8());
  let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
  let mut buf = vec![0; reader.output_buffer_size().ok_or("image too large")?];
  let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
  buf.truncate(info.buffer_size());
  let (width, height) = (info.width as i32, info.height as i32);
  let image = |channels: i32, rowstride: i32, data: Vec<u8>| Image { width, height, rowstride, has_alpha: channels == 4, bits_per_sample: 8, channels, data };
  Ok(match info.color_type {
    png::ColorType::Rgba => image(4, info.line_size as i32, buf),
    png::ColorType::Rgb => image(3, info.line_size as i32, buf),
    png::ColorType::GrayscaleAlpha => image(4, width * 4, buf.as_chunks::<2>().0.iter().flat_map(|&[v, a]| [v, v, v, a]).collect()),
    png::ColorType::Grayscale => image(3, width * 3, buf.iter().flat_map(|&v| [v, v, v]).collect()),
    other => return Err(format!("unsupported pixel format {:?}", other)),
  })
}

/// Decode the largest (then deepest) image of an ICO file, as GdkPixbuf's
/// loader picks it. Entries are PNG data or a BMP DIB (32-bit BGRA, or
/// 24/8/4/1-bit with the AND mask for transparency).
fn decode_ico(data: &[u8]) -> Result<Image, String> {
  let le = |o: usize, n: usize| data.get(o..o + n).map(|b| b.iter().rev().fold(0usize, |acc, v| acc << 8 | *v as usize));
  let count = le(4, 2).ok_or("truncated icon")?;
  let best = (0..count)
    .filter_map(|i| {
      let entry = 6 + i * 16;
      let side = |v: u8| if v == 0 { 256 } else { v as usize };
      let area = side(*data.get(entry)?) * side(*data.get(entry + 1)?);
      let (bpp, size, offset) = (le(entry + 6, 2)?, le(entry + 8, 4)?, le(entry + 12, 4)?);
      Some(((area, bpp), data.get(offset..offset.checked_add(size)?)?))
    })
    .max_by_key(|(key, _)| *key)
    .map(|(_, image)| image)
    .ok_or("icon has no readable images")?;
  if best.starts_with(PNG_SIGNATURE) { decode_png(best) } else { decode_dib(best) }
}

fn decode_dib(dib: &[u8]) -> Result<Image, String> {
  let le = |o: usize, n: usize| dib.get(o..o + n).map(|b| b.iter().rev().fold(0u32, |acc, v| acc << 8 | *v as u32)).ok_or("truncated icon bitmap");
  let header = le(0, 4)? as usize;
  // The height counts the colour bitmap and the AND mask
  let (width, height, bpp) = (le(4, 4)? as i32, le(8, 4)? as i32 / 2, le(14, 2)?);
  if le(16, 4)? != 0 {
    return Err("compressed icon bitmaps are not supported".into());
  }
  if !(1..=256).contains(&width) || !(1..=256).contains(&height) || ![1, 4, 8, 24, 32].contains(&bpp) {
    return Err(format!("unsupported icon bitmap ({}x{}, {} bits)", width, height, bpp));
  }
  let (w, h, bpp) = (width as usize, height as usize, bpp as usize);
  let colours = if bpp <= 8 { Some(le(32, 4)? as usize).filter(|n| *n > 0).unwrap_or(1 << bpp) } else { 0 };
  let palette = dib.get(header..header + colours * 4).ok_or("truncated icon palette")?;
  let pixels = header + colours * 4;
  let stride = (w * bpp).div_ceil(32) * 4;
  let mask_stride = w.div_ceil(32) * 4;
  let mask = pixels + stride * h;
  if dib.len() < mask + mask_stride * h {
    return Err("truncated icon bitmap".into());
  }
  // Rows are stored bottom-up; a set AND-mask bit is a transparent pixel
  let transparent = |x: usize, y: usize| dib[mask + (h - 1 - y) * mask_stride + x / 8] >> (7 - x % 8) & 1 == 1;

  // Pixels are BGR(A) or palette indexes
  let mut out = Vec::with_capacity(w * h * 4);
  for y in 0..h {
    let row = &dib[pixels + (h - 1 - y) * stride..][..stride];
    for x in 0..w {
      let bgra = match bpp {
        32 => [row[x * 4], row[x * 4 + 1], row[x * 4 + 2], row[x * 4 + 3]],
        24 => [row[x * 3], row[x * 3 + 1], row[x * 3 + 2], 255],
        _ => {
          let bit = x * bpp;
          let index = (row[bit / 8] >> (8 - bpp - bit % 8)) as usize & ((1 << bpp) - 1);
          let c = palette.get(index * 4..index * 4 + 3).ok_or("icon palette index out of range")?;
          [c[0], c[1], c[2], 255]
        }
      };
      out.extend_from_slice(&[bgra[2], bgra[1], bgra[0], if bpp != 32 && transparent(x, y) { 0 } else { bgra[3] }]);
    }
  }
  // A 32-bit bitmap with no alpha at all relies on the AND mask instead
  if bpp == 32 && out.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
    for y in 0..h {
      for x in 0..w {
        out[(y * w + x) * 4 + 3] = if transparent(x, y) { 0 } else { 255 };
      }
    }
  }
  Ok(Image { width, height, rowstride: width * 4, has_alpha: true, bits_per_sample: 8, channels: 4, data: out })
}

/// Show `notification` through the session bus, returning its id.
#[cfg(unix)]
pub(super) async fn send(notification: &Notification) -> Result<u32, NotifyError> {
  let call = async {
    let conn = zbus::Connection::session().await?;
    call(&conn, notification).await
  };
  match tokio::time::timeout(CALL_TIMEOUT, call).await {
    Ok(result) => result.map_err(|e| NotifyError::Other(format!("D-Bus: {}", e))),
    Err(_) => Err(NotifyError::Other("D-Bus: timed out waiting for the notification service".into())),
  }
}

#[cfg(not(unix))]
pub(super) async fn send(_notification: &Notification) -> Result<u32, NotifyError> {
  Err(NotifyError::NotSupported)
}

#[cfg(unix)]
async fn call(conn: &zbus::Connection, n: &Notification) -> zbus::Result<u32> {
  let hints: std::collections::HashMap<&str, zbus::Variant<'_>> = n
    .hints
    .iter()
    .map(|(name, hint)| {
      let value = match hint {
        Hint::Byte(v) => zbus::Variant::U8(*v),
        Hint::Int(v) => zbus::Variant::I32(*v),
        Hint::Int64(v) => zbus::Variant::I64(*v),
        Hint::Image(i) => zbus::Variant::from((i.width, i.height, i.rowstride, i.has_alpha, i.bits_per_sample, i.channels, i.data.clone())),
      };
      (*name, value)
    })
    .collect();
  let body = (n.app_name.as_str(), 0u32, "", n.summary.as_str(), n.body.as_str(), Vec::<&str>::new(), hints, n.expire_timeout);
  let reply = conn.call_method(Some(DESTINATION), PATH, Some(DESTINATION), "Notify", &body).await?;
  reply.body().deserialize()
}

#[cfg(all(test, unix))]
pub(super) mod tests {
  use super::*;
  use std::collections::HashMap;
  use zbus::OwnedVariant;

  /// What the notification service received for one `Notify` call.
  pub type Received = (String, u32, String, String, String, Vec<String>, HashMap<String, OwnedVariant>, i32);

  /// Make the `Notify` call against a fake service over a socket pair.
  pub async fn capture(notification: &Notification) -> (String, Received) {
    use zbus::export::futures_core::Stream;

    let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    let server = zbus::connection::Builder::unix_stream(a).server(zbus::Guid::generate()).p2p().build();
    let client = zbus::connection::Builder::unix_stream(b).p2p().build();
    let (server, client) = tokio::try_join!(server, client).unwrap();

    let service = async {
      let mut stream = zbus::MessageStream::from(&server);
      let msg = std::future::poll_fn(|cx| std::pin::Pin::new(&mut stream).poll_next(cx)).await.unwrap().unwrap();
      let header = msg.header();
      let target = format!("{} {} {}", header.path().unwrap(), header.interface().unwrap(), header.member().unwrap());
      let body: Received = msg.body().deserialize().unwrap();
      server.reply(&header, &(42u32,)).await.unwrap();
      (target, body)
    };
    let (received, id) = tokio::join!(service, call(&client, notification));
    assert_eq!(id.unwrap(), 42);
    received
  }

  #[tokio::test]
  async fn test_wire_format() {
    let notification = Notification {
      app_name: "Apprise".into(),
      summary: "T".into(),
      body: "B".into(),
      hints: vec![
        ("urgency", Hint::Byte(2)),
        ("x", Hint::Int(0)),
        ("sender-pid", Hint::Int64(7)),
        ("icon_data", Hint::Image(icon(&NotifyContext::default()).unwrap())),
      ],
      expire_timeout: 13000,
    };
    let (target, (app, id, icon_name, summary, body, actions, hints, timeout)) = capture(&notification).await;
    assert_eq!(target, "/org/freedesktop/Notifications org.freedesktop.Notifications Notify");
    assert_eq!((app.as_str(), id, icon_name.as_str(), summary.as_str(), body.as_str(), actions.len(), timeout), ("Apprise", 0, "", "T", "B", 0, 13000));
    assert_eq!(u8::try_from(&*hints["urgency"]).unwrap(), 2);
    assert_eq!(i32::try_from(&*hints["x"]).unwrap(), 0);
    assert_eq!(i64::try_from(&*hints["sender-pid"]).unwrap(), 7);
    let image = hints["icon_data"].try_clone().unwrap();
    let (w, h, stride, alpha, bits, channels, data): (i32, i32, i32, bool, i32, i32, Vec<u8>) = image.try_into().unwrap();
    assert_eq!((w, h, stride, alpha, bits, channels, data.len()), (128, 128, 512, true, 8, 4, 128 * 512));
  }
}

#[cfg(test)]
mod icon_tests {
  use super::*;
  use crate::types::NotifyType;

  #[test]
  fn test_icon() {
    for notify_type in [NotifyType::Info, NotifyType::Success, NotifyType::Warning, NotifyType::Failure] {
      let image = icon(&NotifyContext { notify_type, ..Default::default() }).unwrap();
      assert_eq!((image.width, image.height, image.rowstride, image.has_alpha, image.bits_per_sample, image.channels), (128, 128, 512, true, 8, 4));
      assert_eq!(image.data.len(), 128 * 128 * 4);
    }
  }
}

#[cfg(test)]
pub(super) mod asset_icon_tests {
  use super::*;
  use crate::asset::AppriseAsset;

  /// A `w`x`h` RGBA PNG whose pixel (x, y) is `[x, y, 7, 255]`.
  pub(crate) fn png_file(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let pixels: Vec<u8> = (0..h).flat_map(|y| (0..w).flat_map(move |x| [x as u8, y as u8, 7, 255])).collect();
    encoder.write_header().unwrap().write_image_data(&pixels).unwrap();
    out
  }

  fn icon_for(mask: &str) -> Option<Image> {
    let ctx = NotifyContext {
      title: "T".into(),
      body: "B".into(),
      asset: AppriseAsset { image_path_mask: Some(mask.into()), ..Default::default() },
      ..Default::default()
    };
    let n = freedesktop_notification(&ctx, 1, None, true);
    n.hints.into_iter().find_map(|(k, v)| match (k, v) {
      ("icon_data", Hint::Image(i)) => Some(i),
      _ => None,
    })
  }

  #[test]
  fn test_custom_asset_icon() {
    // Upstream loads image_path(notify_type, extension=".ico") at 128x128
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("custom-info-128x128.png"), png_file(3, 2)).unwrap();
    let image = icon_for(&format!("{}/custom-{{TYPE}}-{{XY}}.png", dir.path().display())).unwrap();
    assert_eq!((image.width, image.height, image.channels), (3, 2, 4));
    assert_eq!(&image.data[4 * 4..4 * 5], &[1, 1, 7, 255]);

    // A mask naming a file that is not there sends no icon
    assert!(icon_for(&format!("{}/missing-{{TYPE}}.png", dir.path().display())).is_none());

    // {EXTENSION} is .ico, as upstream asks for, and ICO files decode
    std::fs::write(
      dir.path().join("theme-info-128x128.ico"),
      ico(&[(16, 16, dib(2, 2, 32, &[[1, 2, 3, 4], [5, 6, 7, 8], [9, 10, 11, 12], [13, 14, 15, 16]], &[false; 4]))]),
    )
    .unwrap();
    let image = icon_for(&format!("{}/theme-{{TYPE}}-{{XY}}{{EXTENSION}}", dir.path().display())).unwrap();
    assert_eq!((image.width, image.height, image.data), (2, 2, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]));

    // Without a mask only the default theme has (bundled) icons
    let ctx = NotifyContext { asset: AppriseAsset { theme: "other".into(), ..Default::default() }, ..Default::default() };
    assert!(icon(&ctx).is_none());
    assert_eq!(icon(&NotifyContext::default()).unwrap().width, 128);
  }

  /// A BMP DIB as an ICO entry stores it: rows bottom-up, the height
  /// doubled for the AND mask. `bpp` is 32 (BGRA from the RGBA pixels) or 1
  /// (a black/red palette, red where the red channel is set).
  fn dib(w: usize, h: usize, bpp: u16, rgba: &[[u8; 4]], mask: &[bool]) -> Vec<u8> {
    let mut out = Vec::new();
    for v in [40u32, w as u32, 2 * h as u32] {
      out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&bpp.to_le_bytes());
    for v in [0u32, 0, 0, 0, if bpp == 1 { 2 } else { 0 }, 0] {
      out.extend_from_slice(&v.to_le_bytes());
    }
    if bpp == 1 {
      out.extend_from_slice(&[0, 0, 0, 0, 0, 0, 255, 0]);
    }
    let stride = (w * bpp as usize).div_ceil(32) * 4;
    for y in (0..h).rev() {
      let mut row = vec![0u8; stride];
      for x in 0..w {
        let [r, g, b, a] = rgba[y * w + x];
        if bpp == 32 {
          row[x * 4..x * 4 + 4].copy_from_slice(&[b, g, r, a]);
        } else if r > 0 {
          row[x / 8] |= 0x80 >> (x % 8);
        }
      }
      out.extend_from_slice(&row);
    }
    let mask_stride = w.div_ceil(32) * 4;
    for y in (0..h).rev() {
      let mut row = vec![0u8; mask_stride];
      for x in 0..w {
        if mask[y * w + x] {
          row[x / 8] |= 0x80 >> (x % 8);
        }
      }
      out.extend_from_slice(&row);
    }
    out
  }

  /// An ICO file holding `(width, height, data)` entries.
  fn ico(entries: &[(u8, u8, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0, entries.len() as u8, 0];
    let mut offset = 6 + 16 * entries.len();
    for (w, h, data) in entries {
      out.extend_from_slice(&[*w, *h, 0, 0, 1, 0, 32, 0]);
      out.extend_from_slice(&(data.len() as u32).to_le_bytes());
      out.extend_from_slice(&(offset as u32).to_le_bytes());
      offset += data.len();
    }
    for (_, _, data) in entries {
      out.extend_from_slice(data);
    }
    out
  }

  #[test]
  fn test_decode_ico() {
    let red = [255, 0, 0, 255];
    let black = [0, 0, 0, 255];

    // 32-bit pixels keep their own alpha; the mask is ignored
    let image = decode_image(&ico(&[(2, 1, dib(2, 1, 32, &[[255, 0, 0, 128], [0, 255, 0, 255]], &[true, true]))])).unwrap();
    assert_eq!((image.width, image.height, image.rowstride, image.channels), (2, 1, 8, 4));
    assert_eq!(image.data, vec![255, 0, 0, 128, 0, 255, 0, 255]);

    // ...unless every alpha is zero, when the AND mask decides
    let image = decode_image(&ico(&[(2, 1, dib(2, 1, 32, &[[1, 2, 3, 0], [4, 5, 6, 0]], &[true, false]))])).unwrap();
    assert_eq!(image.data, vec![1, 2, 3, 0, 4, 5, 6, 255]);

    // A 1-bit palette bitmap, two rows to check the bottom-up order
    let image = decode_image(&ico(&[(2, 2, dib(2, 2, 1, &[red, black, black, red], &[false, false, true, false]))])).unwrap();
    assert_eq!(image.data, vec![255, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255]);

    // The largest entry wins, and it may be PNG data
    let small = dib(1, 1, 32, &[red], &[false]);
    let image = decode_image(&ico(&[(1, 1, small.clone()), (3, 2, png_file(3, 2))])).unwrap();
    assert_eq!((image.width, image.height), (3, 2));
    let image = decode_image(&ico(&[(3, 2, png_file(3, 2)), (0, 0, small)])).unwrap();
    assert_eq!((image.width, image.height), (1, 1));

    for bad in [&b"GIF89a"[..], &[0, 0, 1, 0, 1, 0][..], &ico(&[(2, 2, dib(2, 2, 32, &[red; 4], &[false; 4])[..50].to_vec())])[..]] {
      assert!(decode_image(bad).is_err());
    }
  }

  #[test]
  fn test_decode_png_formats() {
    // Grey and palette PNGs come out as 8-bit RGB(A)
    let encode = |color: png::ColorType, depth: png::BitDepth, data: &[u8], palette: Option<&[u8]>| {
      let mut out = Vec::new();
      let mut encoder = png::Encoder::new(&mut out, 2, 1);
      encoder.set_color(color);
      encoder.set_depth(depth);
      if let Some(p) = palette {
        encoder.set_palette(p.to_vec());
      }
      encoder.write_header().unwrap().write_image_data(data).unwrap();
      out
    };
    let image = decode_image(&encode(png::ColorType::Grayscale, png::BitDepth::Eight, &[10, 20], None)).unwrap();
    assert_eq!((image.channels, image.rowstride, image.data), (3, 6, vec![10, 10, 10, 20, 20, 20]));
    let image = decode_image(&encode(png::ColorType::GrayscaleAlpha, png::BitDepth::Eight, &[10, 1, 20, 2], None)).unwrap();
    assert_eq!((image.channels, image.data), (4, vec![10, 10, 10, 1, 20, 20, 20, 2]));
    let image = decode_image(&encode(png::ColorType::Indexed, png::BitDepth::Eight, &[1, 0], Some(&[1, 2, 3, 4, 5, 6]))).unwrap();
    assert_eq!((image.channels, image.data), (3, vec![4, 5, 6, 1, 2, 3]));
  }
}
