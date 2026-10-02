//! OCR of scans (ADR-0028): PDFs without a text layer and images, read with
//! Windows' own recognizer (`Windows.Media.Ocr`) after rendering with its
//! own PDF renderer (`Windows.Data.Pdf`). Nothing to download or bundle,
//! nothing leaves the machine.
//!
//! The recognizer's languages are Windows' language packs. Russian is
//! preferred: its recognizer reads Latin text too (a mixed Russian/English
//! paragraph came out right), the English one does not read Cyrillic. The
//! language used is recorded on the document, so a scan read without
//! Russian is visible as such.
//!
//! Blocking: run it on a blocking thread (the worker's `spawn_blocking`).

use anyhow::Result;

/// What a scan yielded.
pub struct Recognized {
    pub text:     String,
    /// BCP-47 tag of the recognizer used, e.g. "ru".
    pub language: String,
}

/// Pages are rendered at this resolution: 300 dpi is what OCR engines are
/// tuned for. The page size is taken as 1/96-inch units; Windows reports it
/// scaled with the display (a Letter page came as 1020 wide, not 816, at
/// 125 %), so on a scaled display a page renders somewhat finer — at least
/// 300 dpi, never less.
#[cfg(windows)]
const DPI: f32 = 300.0;

#[cfg(windows)]
mod imp {
    use super::{Recognized, DPI};
    use anyhow::{bail, Context, Result};
    use windows::core::HSTRING;
    use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
    use windows::Globalization::Language;
    use windows::Graphics::Imaging::{
        BitmapAlphaMode, BitmapDecoder, BitmapInterpolationMode, BitmapPixelFormat, BitmapTransform,
        ColorManagementMode, ExifOrientationMode, SoftwareBitmap,
    };
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

    /// WinRT calls need the thread in an apartment; a blocking-pool thread
    /// is in none. Joining the multithreaded one again is harmless.
    fn enter_apartment() {
        // SAFETY: plain COM initialisation of the current thread; an
        // "already initialised" result is fine and ignored.
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    }

    fn engine() -> Result<(OcrEngine, String)> {
        let russian = Language::CreateLanguage(&HSTRING::from("ru"))?;
        if OcrEngine::IsLanguageSupported(&russian).unwrap_or(false) {
            return Ok((OcrEngine::TryCreateFromLanguage(&russian)?, "ru".into()));
        }
        let engine = OcrEngine::TryCreateFromUserProfileLanguages().map_err(|_| {
            anyhow::anyhow!(
                "Windows has no text recognizer for any of the user's languages — add Russian \
                 (Settings → Time & language → Language & region) to read scans"
            )
        })?;
        let tag = engine.RecognizerLanguage()?.LanguageTag()?.to_string();
        Ok((engine, tag))
    }

    fn stream_of(bytes: &[u8]) -> Result<InMemoryRandomAccessStream> {
        let stream = InMemoryRandomAccessStream::new()?;
        let writer = DataWriter::CreateDataWriter(&stream)?;
        writer.WriteBytes(bytes)?;
        writer.StoreAsync()?.get()?;
        writer.FlushAsync()?.get()?;
        writer.DetachStream()?;
        stream.Seek(0)?;
        Ok(stream)
    }

    /// Lines of one image, top to bottom as the recognizer orders them.
    fn read(engine: &OcrEngine, bitmap: &SoftwareBitmap) -> Result<String> {
        let result = engine.RecognizeAsync(bitmap)?.get()?;
        let mut text = String::new();
        for line in result.Lines()? {
            text.push_str(&line.Text()?.to_string());
            text.push('\n');
        }
        Ok(text)
    }

    /// Every frame of a decoded image (a multi-page TIFF has several), at
    /// most the recognizer's maximum on the longer side, turned the way the
    /// camera says (EXIF), as BGRA8 — a format the recognizer takes.
    fn frames(decoder: &BitmapDecoder) -> Result<Vec<SoftwareBitmap>> {
        let max = OcrEngine::MaxImageDimension()?;
        let mut out = Vec::new();
        for i in 0..decoder.FrameCount()? {
            let frame = decoder.GetFrameAsync(i)?.get()?;
            let (w, h) = (frame.OrientedPixelWidth()?, frame.OrientedPixelHeight()?);
            let transform = BitmapTransform::new()?;
            if w.max(h) > max {
                let k = max as f64 / w.max(h) as f64;
                transform.SetScaledWidth(((frame.PixelWidth()? as f64) * k) as u32)?;
                transform.SetScaledHeight(((frame.PixelHeight()? as f64) * k) as u32)?;
                transform.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
            }
            out.push(
                frame
                    .GetSoftwareBitmapTransformedAsync(
                        BitmapPixelFormat::Bgra8,
                        BitmapAlphaMode::Premultiplied,
                        &transform,
                        ExifOrientationMode::RespectExifOrientation,
                        ColorManagementMode::DoNotColorManage,
                    )?
                    .get()?,
            );
        }
        Ok(out)
    }

    pub fn image(bytes: &[u8]) -> Result<Recognized> {
        enter_apartment();
        let (engine, language) = engine()?;
        let decoder = BitmapDecoder::CreateAsync(&stream_of(bytes)?)?.get().context("not an image Windows can decode")?;
        let mut text = String::new();
        for bitmap in frames(&decoder)? {
            text.push_str(&read(&engine, &bitmap)?);
            text.push('\n');
        }
        Ok(Recognized { text, language })
    }

    pub fn pdf(bytes: &[u8]) -> Result<Recognized> {
        enter_apartment();
        let (engine, language) = engine()?;
        let doc = PdfDocument::LoadFromStreamAsync(&stream_of(bytes)?)?.get().context("Windows could not open the PDF")?;
        if doc.IsPasswordProtected()? {
            bail!("the PDF is password-protected");
        }
        let max = OcrEngine::MaxImageDimension()? as f32;
        let mut text = String::new();
        for i in 0..doc.PageCount()? {
            let page = doc.GetPage(i)?;
            let size = page.Size()?; // in 1/96 inch
            let scale = (DPI / 96.0).min(max / size.Width.max(size.Height).max(1.0));
            let options = PdfPageRenderOptions::new()?;
            options.SetDestinationWidth((size.Width * scale) as u32)?;
            options.SetDestinationHeight((size.Height * scale) as u32)?;
            let rendered = InMemoryRandomAccessStream::new()?;
            page.RenderWithOptionsToStreamAsync(&rendered, &options)?.get()?;
            let decoder = BitmapDecoder::CreateAsync(&rendered)?.get()?;
            for bitmap in frames(&decoder)? {
                text.push_str(&read(&engine, &bitmap)?);
            }
            text.push('\n');
        }
        Ok(Recognized { text, language })
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Recognized;
    use anyhow::{bail, Result};

    pub fn image(_: &[u8]) -> Result<Recognized> {
        bail!("OCR uses Windows' text recognizer and is not available on this system")
    }

    pub fn pdf(_: &[u8]) -> Result<Recognized> {
        bail!("OCR uses Windows' text recognizer and is not available on this system")
    }
}

/// Whether Windows can read Russian here (tests skip without it).
#[cfg(windows)]
pub fn russian_available() -> bool {
    use windows::{core::HSTRING, Globalization::Language, Media::Ocr::OcrEngine};
    Language::CreateLanguage(&HSTRING::from("ru"))
        .and_then(|l| OcrEngine::IsLanguageSupported(&l))
        .unwrap_or(false)
}

/// Read a PDF that has no text layer, page by page.
pub fn pdf(bytes: &[u8]) -> Result<Recognized> {
    imp::pdf(bytes)
}

/// Read an image (JPEG, PNG, TIFF — every page of a multi-page one — BMP).
pub fn image(bytes: &[u8]) -> Result<Recognized> {
    imp::image(bytes)
}
