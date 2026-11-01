//! The sandboxed image normalizer. See `connection` for the protocol and process model.

use std::process::ExitCode;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
  match confer_image_normalizer::connection::serve() {
    Ok(()) => ExitCode::SUCCESS,
    Err(error) => {
      eprintln!("confer-image-normalizer: {error}");
      ExitCode::FAILURE
    }
  }
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
  eprintln!(
    "confer-image-normalizer: requires Linux; examples/normalize_once.rs decodes one image anywhere"
  );
  ExitCode::FAILURE
}
