//! Video thumbnails: FFmpeg gives PNG bytes, WIC decodes them, GDI draws them.

use windows::core::{Interface, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

/// A decoded frame: 32-bit BGRA, top-down rows.
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
}

pub fn decode_png(bytes: &[u8]) -> Result<Image> {
    unsafe {
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream = wic.CreateStream()?;
        stream.InitializeFromMemory(bytes)?;
        let dec = wic.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
        let frame = dec.GetFrame(0)?;
        let conv = wic.CreateFormatConverter()?;
        conv.Initialize(&frame, &GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
        let src: IWICBitmapSource = conv.cast()?;
        let (mut w, mut h) = (0u32, 0u32);
        src.GetSize(&mut w, &mut h)?;
        let mut px = vec![0u8; (w * h * 4) as usize];
        src.CopyPixels(std::ptr::null(), w * 4, &mut px)?;
        Ok(Image { w, h, px })
    }
}

/// Draw `img` centred in `dst`, scaled to fit (aspect ratio kept).
pub fn draw_fit(hdc: HDC, img: &Image, dst: RECT) {
    let (dw, dh) = ((dst.right - dst.left) as f32, (dst.bottom - dst.top) as f32);
    if dw <= 0.0 || dh <= 0.0 || img.w == 0 || img.h == 0 {
        return;
    }
    let k = (dw / img.w as f32).min(dh / img.h as f32);
    let (w, h) = ((img.w as f32 * k).round() as i32, (img.h as f32 * k).round() as i32);
    let x = dst.left + (dw as i32 - w) / 2;
    let y = dst.top + (dh as i32 - h) / 2;
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: img.w as i32,
            biHeight: -(img.h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    unsafe {
        SetStretchBltMode(hdc, HALFTONE);
        let _ = SetBrushOrgEx(hdc, 0, 0, None);
        StretchDIBits(
            hdc,
            x,
            y,
            w,
            h,
            0,
            0,
            img.w as i32,
            img.h as i32,
            Some(img.px.as_ptr() as _),
            &bmi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
    }
}

/// A `px`×`px` icon with `img` scaled to fit (transparent padding), for icon buttons.
pub fn icon_fit(img: &Image, px: u32) -> Option<HICON> {
    let k = (px as f32 / img.w as f32).min(px as f32 / img.h as f32);
    let (w, h) = (((img.w as f32 * k).round() as u32).max(1), ((img.h as f32 * k).round() as u32).max(1));
    let (ox, oy) = ((px - w) / 2, (px - h) / 2);
    // Box-filter downscale into a px×px BGRA canvas.
    let mut out = vec![0u8; (px * px * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let (sx0, sx1) = ((x as f32 / k) as u32, (((x + 1) as f32 / k) as u32).max(x / 1 + 1).min(img.w));
            let (sy0, sy1) = ((y as f32 / k) as u32, (((y + 1) as f32 / k) as u32).min(img.h));
            let (sx1, sy1) = (sx1.max(sx0 + 1).min(img.w), sy1.max(sy0 + 1).min(img.h));
            let mut acc = [0u32; 4];
            let mut n = 0;
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let i = ((sy * img.w + sx) * 4) as usize;
                    let a = img.px[i + 3] as u32;
                    for c in 0..3 {
                        acc[c] += img.px[i + c] as u32 * a;
                    }
                    acc[3] += a;
                    n += 1;
                }
            }
            let o = (((y + oy) * px + x + ox) * 4) as usize;
            if acc[3] > 0 {
                for c in 0..3 {
                    out[o + c] = (acc[c] / acc[3]) as u8;
                }
            }
            out[o + 3] = (acc[3] / n.max(1)) as u8;
        }
    }
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: px as i32,
                biHeight: -(px as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        std::ptr::copy_nonoverlapping(out.as_ptr(), bits as *mut u8, out.len());
        let mask = CreateBitmap(px as i32, px as i32, 1, 1, None);
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        icon
    }
}
