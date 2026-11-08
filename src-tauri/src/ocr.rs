//! Text recognition with the operating system's built-in, on-device OCR: Windows.Media.Ocr on
//! Windows and the Vision framework on macOS. Both are faster and more accurate on
//! screenshots than Tesseract in WebAssembly, read every language the system supports, and
//! release their memory after each image. `recognize` returns `Ok(None)` when no native
//! engine is available (for example, Windows without an OCR language installed); the web
//! worker's Tesseract is then used instead.

use crate::AppState;
use std::path::Path;
use tauri::State;

/// Recognizes text in an image file, one line of text per line.
pub fn recognize_file(path: &Path) -> Result<Option<String>, String> {
    let image = image::open(path).map_err(|error| format!("Cannot read screenshot: {error}"))?;
    platform::recognize(&image)
}

/// Reads a library screenshot with native OCR. `None` means native OCR is unavailable and
/// the caller should fall back to Tesseract.
#[tauri::command]
pub async fn recognize_text(state: State<'_, AppState>, id: i64) -> Result<Option<String>, String> {
    let db_path = state.db_path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let path = crate::library::screenshot_path_for_id(&db_path, id)?;
        recognize_file(&path)
    })
    .await
    .map_err(|error| format!("Text recognition failed: {error}"))?
}

#[cfg(windows)]
mod platform {
    use image::DynamicImage;
    use std::sync::Once;
    use windows::{
        Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap},
        Media::Ocr::OcrEngine,
        Storage::Streams::DataWriter,
        Win32::System::Com::CoIncrementMTAUsage,
    };

    /// WinRT objects need a COM apartment. Keeping the process-wide multithreaded apartment
    /// alive lets any worker thread use them without initializing COM itself.
    fn ensure_apartment() {
        static APARTMENT: Once = Once::new();
        APARTMENT.call_once(|| {
            let _ = unsafe { CoIncrementMTAUsage() };
        });
    }

    pub fn recognize(image: &DynamicImage) -> Result<Option<String>, String> {
        ensure_apartment();
        // Uses the languages in the user's Windows settings. It fails when none of them has
        // OCR support installed, which falls back to Tesseract.
        let Ok(engine) = OcrEngine::TryCreateFromUserProfileLanguages() else {
            return Ok(None);
        };
        let limit = OcrEngine::MaxImageDimension().unwrap_or(2600);
        let image = if image.width() > limit || image.height() > limit {
            image.resize(limit, limit, image::imageops::FilterType::Triangle)
        } else {
            image.clone()
        };
        let mut pixels = image.to_rgba8();
        let (width, height) = pixels.dimensions();
        // SoftwareBitmap wants BGRA.
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        let text = (|| -> windows::core::Result<String> {
            let writer = DataWriter::new()?;
            writer.WriteBytes(&pixels)?;
            let buffer = writer.DetachBuffer()?;
            let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
                &buffer,
                BitmapPixelFormat::Bgra8,
                width as i32,
                height as i32,
            )?;
            let result = engine.RecognizeAsync(&bitmap)?.join()?;
            let mut lines = Vec::new();
            for line in result.Lines()? {
                lines.push(line.Text()?.to_string());
            }
            Ok(lines.join("\n"))
        })()
        .map_err(|error| format!("Windows OCR failed: {error}"))?;
        Ok(Some(text))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use image::{DynamicImage, ImageFormat};
    use objc2::{AllocAnyThread, rc::autoreleasepool, runtime::AnyObject, sel};
    use objc2_foundation::{NSArray, NSData, NSDictionary, NSObjectProtocol};
    use objc2_vision::{
        VNImageOption, VNImageRequestHandler, VNRecognizeTextRequest, VNRequest,
        VNRequestTextRecognitionLevel,
    };
    use std::io::Cursor;

    pub fn recognize(image: &DynamicImage) -> Result<Option<String>, String> {
        let mut encoded = Cursor::new(Vec::new());
        image
            .write_to(&mut encoded, ImageFormat::Png)
            .map_err(|error| error.to_string())?;
        let encoded = encoded.into_inner();
        autoreleasepool(|_| {
            let request = VNRecognizeTextRequest::new();
            request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
            request.setUsesLanguageCorrection(true);
            // Language detection exists on macOS 13 and later; older versions read the
            // default language only.
            if request.respondsToSelector(sel!(setAutomaticallyDetectsLanguage:)) {
                request.setAutomaticallyDetectsLanguage(true);
            }
            let data = NSData::with_bytes(&encoded);
            let options = NSDictionary::<VNImageOption, AnyObject>::new();
            let handler = VNImageRequestHandler::initWithData_options(
                VNImageRequestHandler::alloc(),
                &data,
                &options,
            );
            let as_request: &VNRequest = &request;
            handler
                .performRequests_error(&NSArray::from_slice(&[as_request]))
                .map_err(|error| format!("Vision OCR failed: {error}"))?;
            let mut lines = Vec::new();
            if let Some(observations) = request.results() {
                for observation in observations.iter() {
                    if let Some(best) = observation.topCandidates(1).firstObject() {
                        lines.push(best.string().to_string());
                    }
                }
            }
            Ok(Some(lines.join("\n")))
        })
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use image::DynamicImage;

    pub fn recognize(_image: &DynamicImage) -> Result<Option<String>, String> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs only where a native engine exists; elsewhere `None` is the expected answer.
    #[test]
    fn reads_the_sample_screenshot() {
        let sample = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../test-assets/ather-null-project-budget.png");
        let started = std::time::Instant::now();
        match recognize_file(&sample).unwrap() {
            Some(text) => {
                println!("native OCR in {:?}: {text:?}", started.elapsed());
                let lower = text.to_lowercase();
                assert!(lower.contains("project"), "{text}");
                assert!(lower.contains("budget"), "{text}");
            }
            None => println!("no native OCR engine on this system"),
        }
    }
}
