"""Compares the normalizer's pixels with vLLM's own image loading on a generated corpus.

vLLM 0.29.0 opens image bytes with Pillow, applies exif_transpose (ignoring failures) and
converts to RGB, flattening any transparency onto white. This script repeats those steps with
the Pillow version vLLM ships with, decodes the same bytes with the normalizer (its
normalize_once example, built with `cargo build --release --examples`) and reports how far
apart the results are.

  python3 -I tests/compare_with_vllm.py target/release/examples/normalize_once [photo.jpg]

Requires Pillow at the version pinned in requirements-vllm.lock.
"""
import contextlib
import io
import struct
import subprocess
import sys
import time

from PIL import Image, ImageChops, ImageDraw, ImageOps

ORIENTATION_TAG = 0x0112


class VllmLoader:
  """The steps of vllm/multimodal/media/image.py ImageMediaIO.load_bytes for image_mode RGB."""

  @staticmethod
  def load(data: bytes) -> Image.Image:
    image = Image.open(io.BytesIO(data))
    with contextlib.suppress(Exception):
      image = ImageOps.exif_transpose(image)
    image.load()
    if image.mode == "RGB":
      return image
    if image.mode in ("RGBA", "LA", "PA") or "transparency" in getattr(image, "info", {}):
      if image.mode != "RGBA":
        image = image.convert("RGBA")
      converted = Image.new("RGB", image.size, (255, 255, 255))
      converted.paste(image, mask=image.split()[3])
      return converted
    return image.convert("RGB")


class Normalizer:

  def __init__(self, binary: str):
    self.binary = binary

  def load(self, data: bytes) -> tuple[Image.Image | None, float]:
    started = time.perf_counter()
    result = subprocess.run([self.binary], input=data, capture_output=True, check=False)
    elapsed = time.perf_counter() - started
    if result.returncode != 0:
      return None, elapsed
    width, height = struct.unpack(">II", result.stdout[:8])
    assert len(result.stdout) == 8 + width * height * 3, "frame length mismatch"
    return Image.frombytes("RGB", (width, height), result.stdout[8:]), elapsed


class Corpus:

  def __init__(self, photo: Image.Image):
    self.photo = photo

  def cases(self):
    photo = self.photo
    small = photo.resize((photo.width // 3, photo.height // 3))
    alpha = self.alpha_gradient(small)

    yield "jpeg baseline q85", self.save(photo, "JPEG", quality=85)
    yield "jpeg progressive", self.save(photo, "JPEG", quality=90, progressive=True)
    yield "jpeg 4:4:4", self.save(photo, "JPEG", quality=95, subsampling=0)
    yield "jpeg grayscale", self.save(photo.convert("L"), "JPEG", quality=90)
    yield "jpeg cmyk", self.save(photo.convert("CMYK"), "JPEG", quality=90)
    for orientation in range(1, 9):
      yield f"jpeg exif orientation {orientation}", self.with_orientation(small, "JPEG", orientation)
    yield "mpo two frames", self.save(small, "MPO", save_all=True, append_images=[small.rotate(90)])

    yield "png rgb", self.save(small, "PNG")
    yield "png rgba", self.save(alpha, "PNG")
    yield "png la", self.save(alpha.convert("LA"), "PNG")
    yield "png l", self.save(small.convert("L"), "PNG")
    yield "png 1-bit", self.save(small.convert("1"), "PNG")
    yield "png palette", self.save(small.convert("P"), "PNG")
    yield "png palette trns", self.save(small.convert("P"), "PNG", transparency=0)
    yield "png rgb trns", self.save(small, "PNG", transparency=small.getpixel((0, 0)))
    yield "png 16-bit gray", self.save(small.convert("I").point(lambda v: v * 257).convert("I;16"), "PNG")
    yield "png exif orientation 6", self.with_orientation(small, "PNG", 6)

    yield "gif", self.save(small.convert("P"), "GIF")
    yield "gif transparency", self.save(small.convert("P"), "GIF", transparency=0)
    yield "gif animated", self.save(small.convert("P"), "GIF", save_all=True,
                                    append_images=[small.rotate(180).convert("P")], duration=100)
    yield "gif offset frame", self.offset_frame_gif(small)

    yield "webp lossy", self.save(small, "WEBP", quality=80)
    yield "webp lossless", self.save(small, "WEBP", lossless=True)
    yield "webp lossy alpha", self.save(alpha, "WEBP", quality=80)
    yield "webp lossless alpha", self.save(alpha, "WEBP", lossless=True)
    yield "webp animated", self.save(small, "WEBP", lossless=True, save_all=True,
                                     append_images=[small.rotate(180)], duration=100)
    yield "webp exif orientation 8", self.with_orientation(small, "WEBP", 8, lossless=True)

  @staticmethod
  def save(image: Image.Image, format: str, **options) -> bytes:
    buffer = io.BytesIO()
    image.save(buffer, format, **options)
    return buffer.getvalue()

  @classmethod
  def with_orientation(cls, image: Image.Image, format: str, orientation: int, **options) -> bytes:
    exif = Image.Exif()
    exif[ORIENTATION_TAG] = orientation
    return cls.save(image, format, exif=exif.tobytes(), **options)

  @staticmethod
  def alpha_gradient(image: Image.Image) -> Image.Image:
    rgba = image.convert("RGBA")
    mask = Image.linear_gradient("L").resize(rgba.size)
    rgba.putalpha(mask)
    return rgba

  @classmethod
  def offset_frame_gif(cls, image: Image.Image) -> bytes:
    canvas = image.convert("P")
    second = Image.new("P", image.size, 0)
    ImageDraw.Draw(second).rectangle((10, 10, 60, 60), fill=200)
    return cls.save(canvas, "GIF", save_all=True, append_images=[second], disposal=1,
                    transparency=0, duration=100)


class Comparison:

  @staticmethod
  def describe(expected: Image.Image, actual: Image.Image) -> str:
    if expected.size != actual.size:
      return f"SIZE MISMATCH vLLM {expected.size} normalizer {actual.size}"
    difference = ImageChops.difference(expected, actual)
    histogram = difference.convert("L").histogram()
    samples = sum(ImageChops.difference(expected, actual).histogram())
    channel_histogram = difference.histogram()
    total = sum(channel_histogram)
    mean = sum((index % 256) * count for index, count in enumerate(channel_histogram)) / total
    exact = channel_histogram[0] + channel_histogram[256] + channel_histogram[512]
    largest = max(high for _, high in difference.getextrema())
    assert samples == total and histogram
    return f"max {largest:3d}  mean {mean:6.3f}  exact {100 * exact / total:6.2f}%"


def main() -> int:
  binary = sys.argv[1]
  photo_path = sys.argv[2] if len(sys.argv) > 2 else None
  photo = Image.open(photo_path).convert("RGB") if photo_path else Image.radial_gradient("L").convert("RGB")
  scale = min(1, 1536 / max(photo.size))
  photo = photo.resize((int(photo.width * scale), int(photo.height * scale)), Image.LANCZOS)

  normalizer = Normalizer(binary)
  timings = []
  for name, data in Corpus(photo).cases():
    expected = VllmLoader.load(data)
    actual, elapsed = normalizer.load(data)
    timings.append(elapsed)
    if actual is None:
      print(f"{name:28s} REJECTED by normalizer ({expected.size} in vLLM)")
      continue
    print(f"{name:28s} {expected.size[0]:4d}x{expected.size[1]:<4d} {Comparison.describe(expected, actual)}  "
          f"{1000 * elapsed:6.1f} ms")

  timings.sort()
  print(f"normalizer process time: median {1000 * timings[len(timings) // 2]:.1f} ms, "
        f"max {1000 * timings[-1]:.1f} ms")
  return 0


if __name__ == "__main__":
  raise SystemExit(main())
