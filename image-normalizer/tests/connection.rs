//! The multiplexed connection protocol, exercised against the real binary over a socket pair.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use confer_image_normalizer::{MAX_INPUT_BYTES, normalize};
use image::codecs::png::PngEncoder;
use image::{DynamicImage, ExtendedColorType, ImageEncoder, ImageFormat, Rgb, RgbImage};

/// 32x24, lossy (VP8), written by Pillow.
const LOSSY_WEBP: &[u8] = include_bytes!("fixtures/lossy.webp");

const HEADER: u8 = 1;
const PIXELS: u8 = 2;
const DONE: u8 = 3;
const FAILED: u8 = 4;

#[derive(Debug, PartialEq)]
enum Outcome {
  Frame(u32, u32, Vec<u8>),
  Failed,
}

struct Normalizer {
  process: Child,
  connection: UnixStream,
}

impl Normalizer {
  fn start() -> Self {
    let (connection, theirs) = UnixStream::pair().unwrap();
    let stdin = OwnedFd::from(theirs.try_clone().unwrap());
    let stdout = OwnedFd::from(theirs);
    let process = Command::new(env!("CARGO_BIN_EXE_confer-image-normalizer"))
      .stdin(Stdio::from(stdin))
      .stdout(Stdio::from(stdout))
      .stderr(Stdio::null())
      .spawn()
      .unwrap();

    connection
      .set_read_timeout(Some(Duration::from_secs(20)))
      .unwrap();
    Normalizer {
      process,
      connection,
    }
  }

  fn chunk(&mut self, id: u32, bytes: &[u8]) {
    self.connection.write_all(&id.to_be_bytes()).unwrap();
    self
      .connection
      .write_all(&(bytes.len() as u32).to_be_bytes())
      .unwrap();
    self.connection.write_all(bytes).unwrap();
  }

  fn send(&mut self, id: u32, image: &[u8]) {
    for chunk in image.chunks(4096) {
      self.chunk(id, chunk);
    }
    self.chunk(id, &[]);
  }

  /// The next message, or `None` once the normalizer has ended the connection.
  fn message(&mut self) -> Option<(u32, u8, Vec<u8>)> {
    let mut header = [0u8; 9];
    self.connection.read_exact(&mut header).ok()?;

    let id = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let length = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;
    let mut payload = vec![0; length];
    self.connection.read_exact(&mut payload).ok()?;

    Some((id, header[4], payload))
  }

  /// Reads messages until `count` images have an outcome; `None` if the connection ends first.
  fn outcomes(&mut self, count: usize) -> Option<BTreeMap<u32, Outcome>> {
    let mut frames: BTreeMap<u32, (u32, u32, Vec<u8>)> = BTreeMap::new();
    let mut outcomes = BTreeMap::new();

    while outcomes.len() < count {
      let (id, kind, payload) = self.message()?;
      match kind {
        HEADER => {
          let width = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
          let height = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
          assert!(
            frames.insert(id, (width, height, Vec::new())).is_none(),
            "second header for {id}"
          );
        }
        PIXELS => frames
          .get_mut(&id)
          .expect("pixels before a header")
          .2
          .extend(payload),
        DONE => {
          let (width, height, pixels) = frames.remove(&id).expect("done before a header");
          assert_eq!(pixels.len(), width as usize * height as usize * 3);
          outcomes.insert(id, Outcome::Frame(width, height, pixels));
        }
        FAILED => {
          frames.remove(&id);
          outcomes.insert(id, Outcome::Failed);
        }
        other => panic!("unknown message kind {other}"),
      }
    }

    Some(outcomes)
  }

  fn finish(mut self) -> i32 {
    self.connection.shutdown(Shutdown::Write).ok();
    self.process.wait().unwrap().code().unwrap_or(-1)
  }
}

fn png(width: u32, height: u32, seed: u8) -> Vec<u8> {
  let image = RgbImage::from_fn(width, height, |x, y| Rgb([x as u8 ^ seed, y as u8, seed]));
  let mut bytes = Vec::new();
  PngEncoder::new(&mut bytes)
    .write_image(image.as_raw(), width, height, ExtendedColorType::Rgb8)
    .unwrap();
  bytes
}

fn expected(input: &[u8]) -> Outcome {
  let image = normalize(input).unwrap();
  Outcome::Frame(image.width(), image.height(), image.into_raw())
}

#[test]
fn normalizes_images_one_after_another() {
  let mut normalizer = Normalizer::start();

  for id in 0..3u32 {
    let input = png(64 + id * 7, 48, id as u8);
    normalizer.send(id, &input);
    assert_eq!(
      normalizer.outcomes(1).unwrap().remove(&id),
      Some(expected(&input))
    );
  }

  assert_eq!(normalizer.finish(), 0);
}

#[test]
fn normalizes_interleaved_images_at_once() {
  let mut normalizer = Normalizer::start();
  let inputs: Vec<Vec<u8>> = (0..4u8)
    .map(|seed| png(300, 200 + u32::from(seed), seed))
    .collect();

  // Round-robin small chunks of every image, then end them in reverse order.
  let chunks: Vec<Vec<&[u8]>> = inputs
    .iter()
    .map(|input| input.chunks(1000).collect())
    .collect();
  for index in 0..chunks.iter().map(Vec::len).max().unwrap() {
    for (id, image) in chunks.iter().enumerate() {
      if let Some(chunk) = image.get(index) {
        normalizer.chunk(id as u32 + 10, chunk);
      }
    }
  }
  for id in (0..4u32).rev() {
    normalizer.chunk(id + 10, &[]);
  }

  let outcomes = normalizer.outcomes(4).unwrap();
  for (id, input) in inputs.iter().enumerate() {
    assert_eq!(outcomes[&(id as u32 + 10)], expected(input), "image {id}");
  }
  assert_eq!(normalizer.finish(), 0);
}

#[test]
fn a_rejected_image_fails_alone() {
  let mut normalizer = Normalizer::start();
  let good = png(40, 30, 1);
  let mut broken = png(40, 30, 2);
  broken.truncate(60);

  normalizer.send(1, &good);
  normalizer.send(2, &broken);
  normalizer.send(3, &good);

  let outcomes = normalizer.outcomes(3).unwrap();
  assert_eq!(outcomes[&1], expected(&good));
  assert_eq!(outcomes[&2], Outcome::Failed);
  assert_eq!(outcomes[&3], expected(&good));

  normalizer.send(4, &good);
  assert_eq!(normalizer.outcomes(1).unwrap()[&4], expected(&good));
  assert_eq!(normalizer.finish(), 0);
}

/// Each format reaches different decoder code, and so possibly different system calls; the
/// child is killed by any call its filter lacks.
#[test]
fn decodes_every_supported_format_inside_the_child_filter() {
  let image = RgbImage::from_fn(32, 24, |x, y| Rgb([x as u8 * 8, y as u8 * 10, 128]));
  let mut normalizer = Normalizer::start();
  let formats = [
    ImageFormat::Png,
    ImageFormat::Jpeg,
    ImageFormat::Gif,
    ImageFormat::WebP,
  ];

  for (id, format) in formats.into_iter().enumerate() {
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image.clone())
      .write_to(&mut encoded, format)
      .unwrap();
    normalizer.send(id as u32, encoded.get_ref());

    let outcome = normalizer
      .outcomes(1)
      .unwrap()
      .remove(&(id as u32))
      .unwrap();
    assert!(
      matches!(outcome, Outcome::Frame(32, 24, _)),
      "{format:?} failed"
    );
  }

  // image-webp's encoder is lossless only, so add a lossy file made elsewhere.
  normalizer.send(9, LOSSY_WEBP);
  let outcome = normalizer.outcomes(1).unwrap().remove(&9).unwrap();
  assert!(
    matches!(outcome, Outcome::Frame(32, 24, _)),
    "lossy WebP failed"
  );

  assert_eq!(normalizer.finish(), 0);
}

#[test]
fn fails_an_empty_image_and_carries_on() {
  let mut normalizer = Normalizer::start();
  normalizer.chunk(1, &[]);
  assert_eq!(normalizer.outcomes(1).unwrap()[&1], Outcome::Failed);

  let input = png(8, 8, 3);
  normalizer.send(2, &input);
  assert_eq!(normalizer.outcomes(1).unwrap()[&2], expected(&input));
  assert_eq!(normalizer.finish(), 0);
}

#[test]
fn fails_an_image_over_the_input_limit_and_discards_the_rest_of_it() {
  let mut normalizer = Normalizer::start();
  normalizer.chunk(1, &[0x89; 1024]);
  normalizer.chunk(1, &vec![0; MAX_INPUT_BYTES]);
  normalizer.chunk(1, &[0; 10]);
  normalizer.chunk(1, &[]);
  assert_eq!(normalizer.outcomes(1).unwrap()[&1], Outcome::Failed);

  let input = png(8, 8, 4);
  normalizer.send(2, &input);
  assert_eq!(normalizer.outcomes(1).unwrap()[&2], expected(&input));
  assert_eq!(normalizer.finish(), 0);
}

#[test]
fn a_chunk_after_an_image_ended_ends_the_connection() {
  let mut normalizer = Normalizer::start();
  let input = png(8, 8, 5);
  normalizer.chunk(1, &input);
  normalizer.chunk(1, &[]);
  normalizer.chunk(1, &input);

  assert!(normalizer.outcomes(2).is_none());
  assert_ne!(normalizer.finish(), 0);
}

#[test]
fn too_many_images_in_flight_end_the_connection() {
  let mut normalizer = Normalizer::start();
  let start = png(8, 8, 6);
  for id in 0..9u32 {
    normalizer.chunk(id, &start[..20]);
  }

  assert!(normalizer.outcomes(9).is_none());
  assert_ne!(normalizer.finish(), 0);
}

#[test]
fn a_truncated_chunk_ends_the_connection() {
  let mut normalizer = Normalizer::start();
  let input = png(16, 16, 7);
  normalizer
    .connection
    .write_all(&1u32.to_be_bytes())
    .unwrap();
  normalizer
    .connection
    .write_all(&(input.len() as u32 + 10).to_be_bytes())
    .unwrap();
  normalizer.connection.write_all(&input).unwrap();
  normalizer.connection.shutdown(Shutdown::Write).unwrap();

  assert!(normalizer.outcomes(1).is_none());
  assert_ne!(normalizer.finish(), 0);
}

#[test]
fn fails_images_left_unfinished_when_the_proxy_closes() {
  let mut normalizer = Normalizer::start();
  normalizer.chunk(1, &png(16, 16, 8)[..30]);
  normalizer.connection.shutdown(Shutdown::Write).unwrap();

  assert_eq!(normalizer.outcomes(1).unwrap()[&1], Outcome::Failed);
  assert_eq!(normalizer.process.wait().unwrap().code(), Some(0));
}

#[test]
fn an_idle_connection_waits_for_its_next_image() {
  let mut normalizer = Normalizer::start();
  thread::sleep(Duration::from_millis(500));

  let input = png(16, 16, 9);
  normalizer.send(1, &input);
  assert_eq!(normalizer.outcomes(1).unwrap()[&1], expected(&input));
  assert_eq!(normalizer.finish(), 0);
}
