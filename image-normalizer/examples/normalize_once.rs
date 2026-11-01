//! Decodes one image from stdin and writes its frame (width, height, RGB) to stdout, with no
//! sandbox. For comparing pixels with vLLM and for fuzzing on any platform; never installed.

use std::io::{self, Read, Write};
use std::process::ExitCode;

use confer_image_normalizer::{MAX_INPUT_BYTES, normalize};

fn main() -> ExitCode {
  let mut input = Vec::new();
  if let Err(error) = io::stdin()
    .lock()
    .take(MAX_INPUT_BYTES as u64 + 1)
    .read_to_end(&mut input)
  {
    eprintln!("normalize_once: read failed: {error}");
    return ExitCode::FAILURE;
  }

  let image = match normalize(&input) {
    Ok(image) => image,
    Err(error) => {
      eprintln!("normalize_once: {error}");
      return ExitCode::FAILURE;
    }
  };

  let mut output = io::stdout().lock();
  let written = output
    .write_all(&image.width().to_be_bytes())
    .and_then(|()| output.write_all(&image.height().to_be_bytes()))
    .and_then(|()| output.write_all(image.as_raw()))
    .and_then(|()| output.flush());

  match written {
    Ok(()) => ExitCode::SUCCESS,
    Err(error) => {
      eprintln!("normalize_once: write failed: {error}");
      ExitCode::FAILURE
    }
  }
}
